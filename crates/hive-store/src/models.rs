//! The model job queue and the `models` capability a guest reaches it
//! through (D42).
//!
//! An install asks for one of four things of a local model (`read`,
//! `transcribe`, `generate`, `embed`) with bytes it holds or text it has;
//! the worker claims the job under a lease, answers it, and the finish
//! appends `model.job.finished` under the requesting owner so the app can
//! come back for the result. The worker writes no document: the app that
//! asked is the app that stores.
//!
//! Three rules, each the store's rather than the worker's:
//!
//! - **A blob input is proved, not named.** `submit` resolves the hash
//!   through the caller's own references and refuses one it does not hold
//!   (invariant 3). The worker then reads the bytes by hash, as a
//!   host-internal consumer of a reference already checked.
//! - **A result is keyed on the install that asked** (invariant 14). The
//!   owner alone is not the key: two of one person's apps do not read each
//!   other's jobs.
//! - **Every result is untrusted** (invariant 9). A model's output is
//!   derived from content, and content can carry an instruction.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hive_blob::{Catalog, Hash};
use hive_db::query;
use hive_identity::{Credential, Owner, PrincipalKind};
use hive_trust::Level;
use hive_wasmhost::{HostError, Models, Request, Response};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::appdata::resolve_active_install;
use crate::events::{Event, append_events};
use crate::{Result, Store, StoreError};

/// The four things the platform knows how to ask a model (D42 §1).
pub const MODEL_CAPABILITIES: [&str; 4] = ["read", "transcribe", "generate", "embed"];

pub const JOB_PENDING: &str = "pending";
pub const JOB_CLAIMED: &str = "claimed";
pub const JOB_DONE: &str = "done";
pub const JOB_FAILED: &str = "failed";

/// The event a finished job appends under the requesting owner. Its body
/// names the job, the capability and the terminal state; the result itself
/// is read through the capability, never carried on the log.
pub const JOB_FINISHED_EVENT: &str = "model.job.finished";

/// How many times a job is handed back to the queue after its worker's
/// lease lapsed before it is failed as abandoned.
pub const MAX_ATTEMPTS: i64 = 3;

/// Bounds the text an app can hand to a model in one job.
const MAX_INPUT_TEXT: usize = 1 << 20;
/// Bounds the options document.
const MAX_OPTIONS: usize = 64 << 10;

/// What a job is asked to do.
#[derive(Clone, Debug)]
pub struct JobSpec {
    pub capability: String,
    pub input: JobInput,
    pub options: serde_json::Value,
}

#[derive(Clone, Debug)]
pub enum JobInput {
    /// Bytes the caller holds, and their content type as the catalogue
    /// describes them.
    Blob(Hash, String),
    Text(String),
}

/// A job the worker holds under a lease.
#[derive(Clone, Debug)]
pub struct ClaimedJob {
    pub id: Uuid,
    pub capability: String,
    pub input_blob: Option<Hash>,
    pub input_mime: Option<String>,
    pub input_text: Option<String>,
    pub options: serde_json::Value,
    pub install_id: Uuid,
    pub owner: Owner,
    pub attempts: i64,
}

/// What the asking install sees of a job.
#[derive(Clone, Debug, Serialize)]
pub struct JobView {
    pub job: Uuid,
    pub capability: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The in-process bell a submit rings so the worker does not wait out its
/// poll. A hint, as the events bell is: a worker in another process polls.
pub fn job_wake() -> &'static Notify {
    static WAKE: OnceLock<Notify> = OnceLock::new();
    WAKE.get_or_init(Notify::new)
}

fn lease_until(lease: Duration) -> DateTime<Utc> {
    hive_db::now() + chrono::Duration::from_std(lease).unwrap_or_default()
}

/// The queue, as the worker and the capability both see it.
#[derive(Clone)]
pub struct ModelJobs {
    store: Store,
}

impl ModelJobs {
    pub fn new(store: Store) -> ModelJobs {
        ModelJobs { store }
    }

    /// Writes a job for `install`, asked by `cred`. The input blob, if any,
    /// must already have been resolved through the caller's references by
    /// the capability; this is the row and nothing more.
    pub async fn submit(&self, cred: &Credential, install: Uuid, spec: &JobSpec) -> Result<Uuid> {
        cred.validate()?;
        if !MODEL_CAPABILITIES.contains(&spec.capability.as_str()) {
            return Err(StoreError::InvalidInput(format!(
                "models: {:?} is not a model capability",
                spec.capability
            )));
        }
        let options = serde_json::to_string(&spec.options)?;
        if options.len() > MAX_OPTIONS {
            return Err(StoreError::InvalidInput("models: options too large".into()));
        }
        let (blob, mime, text) = match &spec.input {
            JobInput::Blob(h, mime) => (Some(h.to_string()), Some(mime.clone()), None),
            JobInput::Text(t) => {
                if t.len() > MAX_INPUT_TEXT {
                    return Err(StoreError::InvalidInput(
                        "models: input text too large".into(),
                    ));
                }
                (None, None, Some(t.clone()))
            }
        };
        let owner = cred.owner_of();
        let conn = self.store.conn().await?;
        let id: Uuid = query(
            "INSERT INTO model_jobs (
                 id, capability, input_blob, input_mime, input_text, options,
                 install_id, owner_kind, owner_id, author_actor, principal_kind, principal_id,
                 created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(&spec.capability)
        .bind(blob)
        .bind(mime)
        .bind(text)
        .bind(&options)
        .bind(install)
        .bind(owner.kind.as_str())
        .bind(owner.id)
        .bind(cred.actor_id)
        .bind(cred.principal_kind.as_str())
        .bind(cred.principal_id)
        .bind(hive_db::now())
        .fetch_scalar(&conn)
        .await
        .map_err(|e| StoreError::db("models: submit", e))?;
        drop(conn);
        job_wake().notify_one();
        Ok(id)
    }

    /// Takes the oldest pending job under a lease. One writer at a time
    /// per file is what serialises competing workers.
    pub async fn claim(&self, worker: &str, lease: Duration) -> Result<Option<ClaimedJob>> {
        if worker.is_empty() || lease.is_zero() {
            return Err(StoreError::Other(
                "models: a claim needs a worker name and a positive lease".into(),
            ));
        }
        let tx = self.store.begin().await?;
        let row = query(
            "SELECT id, capability, input_blob, input_mime, input_text, options, install_id,
                    owner_kind, owner_id, attempts
               FROM model_jobs WHERE state = ?1 ORDER BY created_at, id LIMIT 1",
        )
        .bind(JOB_PENDING)
        .fetch_optional(&tx)
        .await
        .map_err(|e| StoreError::db("models: claim", e))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let kind: String = row.get("owner_kind");
        let blob: Option<String> = row.get("input_blob");
        let options: String = row.get("options");
        let job = ClaimedJob {
            id: row.get("id"),
            capability: row.get("capability"),
            input_blob: blob
                .map(|b| Hash::parse(&b).map_err(|e| StoreError::Other(format!("models: {e}"))))
                .transpose()?,
            input_mime: row.get("input_mime"),
            input_text: row.get("input_text"),
            options: serde_json::from_str(&options)?,
            install_id: row.get("install_id"),
            owner: Owner::new(
                PrincipalKind::parse(&kind)
                    .ok_or_else(|| StoreError::Other(format!("owner kind {kind:?}")))?,
                row.get("owner_id"),
            ),
            attempts: row.get("attempts"),
        };
        query(
            "UPDATE model_jobs
                SET state = ?1, claimed_by = ?2, claimed_at = ?3, lease_expires_at = ?4,
                    attempts = attempts + 1
              WHERE id = ?5",
        )
        .bind(JOB_CLAIMED)
        .bind(worker)
        .bind(hive_db::now())
        .bind(lease_until(lease))
        .bind(job.id)
        .execute(&tx)
        .await
        .map_err(|e| StoreError::db("models: claim", e))?;
        crate::commit(tx, "models: claim").await?;
        Ok(Some(ClaimedJob {
            attempts: job.attempts + 1,
            ..job
        }))
    }

    /// The heartbeat. False when the claim is no longer this worker's.
    pub async fn extend_lease(&self, job: Uuid, worker: &str, lease: Duration) -> Result<bool> {
        let conn = self.store.conn().await?;
        let n = query(
            "UPDATE model_jobs SET lease_expires_at = ?1
              WHERE id = ?2 AND state = ?3 AND claimed_by = ?4",
        )
        .bind(lease_until(lease))
        .bind(job)
        .bind(JOB_CLAIMED)
        .bind(worker)
        .execute(&conn)
        .await
        .map_err(|e| StoreError::db("models: extend lease", e))?;
        Ok(n == 1)
    }

    /// Lands a claimed job as done or failed and appends the finished event
    /// in the same transaction. False when the claim was no longer this
    /// worker's: a late answer does not overwrite a reclaim.
    pub async fn finish(
        &self,
        job: Uuid,
        worker: &str,
        outcome: std::result::Result<serde_json::Value, String>,
    ) -> Result<bool> {
        let tx = self.store.begin().await?;
        let row = query(
            "SELECT capability, install_id, owner_kind, owner_id, author_actor,
                    principal_kind, principal_id
               FROM model_jobs WHERE id = ?1 AND state = ?2 AND claimed_by = ?3",
        )
        .bind(job)
        .bind(JOB_CLAIMED)
        .bind(worker)
        .fetch_optional(&tx)
        .await
        .map_err(|e| StoreError::db("models: finish", e))?;
        let Some(row) = row else {
            return Ok(false);
        };
        let (state, result, error) = match &outcome {
            Ok(v) => (JOB_DONE, Some(serde_json::to_string(v)?), None),
            Err(e) => (JOB_FAILED, None, Some(e.clone())),
        };
        query(
            "UPDATE model_jobs SET state = ?1, result = ?2, error = ?3, finished_at = ?4
              WHERE id = ?5",
        )
        .bind(state)
        .bind(result)
        .bind(error)
        .bind(hive_db::now())
        .bind(job)
        .execute(&tx)
        .await
        .map_err(|e| StoreError::db("models: finish", e))?;

        // The announcement, under the owner who asked, by the actor who
        // asked, so a replay filters it the way it filters everything.
        let pk: String = row.get("principal_kind");
        let ok: String = row.get("owner_kind");
        let cred = Credential::new(
            row.get("author_actor"),
            PrincipalKind::parse(&pk).ok_or_else(|| StoreError::Other(format!("kind {pk:?}")))?,
            row.get("principal_id"),
        );
        let capability: String = row.get("capability");
        let install: Uuid = row.get("install_id");
        let mut ev = Event::new(
            JOB_FINISHED_EVENT,
            &cred,
            serde_json::to_vec(&serde_json::json!({
                "job": job, "capability": capability, "state": state, "install": install,
            }))?,
        );
        ev.owner = Owner::new(
            PrincipalKind::parse(&ok).ok_or_else(|| StoreError::Other(format!("kind {ok:?}")))?,
            row.get("owner_id"),
        );
        append_events(&tx, std::slice::from_mut(&mut ev)).await?;
        crate::commit(tx, "models: finish").await?;
        Ok(true)
    }

    /// What `install` may see of `job`: everything, if it asked; nothing,
    /// if another install did, whatever the owner.
    pub async fn result(
        &self,
        cred: &Credential,
        install: Uuid,
        job: Uuid,
    ) -> Result<Option<JobView>> {
        cred.validate()?;
        let conn = self.store.conn().await?;
        let row = query(
            "SELECT capability, state, result, error FROM model_jobs
              WHERE id = ?1 AND install_id = ?2",
        )
        .bind(job)
        .bind(install)
        .fetch_optional(&conn)
        .await
        .map_err(|e| StoreError::db("models: result", e))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let result: Option<String> = row.get("result");
        Ok(Some(JobView {
            job,
            capability: row.get("capability"),
            state: row.get("state"),
            result: result.map(|r| serde_json::from_str(&r)).transpose()?,
            error: row.get("error"),
        }))
    }

    /// Hands lapsed claims back to the queue, or fails them once they have
    /// been abandoned `MAX_ATTEMPTS` times. Returns (requeued, failed).
    pub async fn reclaim_lapsed(&self) -> Result<(u64, u64)> {
        let now = hive_db::now();
        let conn = self.store.conn().await?;
        let failed = query(
            "UPDATE model_jobs
                SET state = ?1, error = 'abandoned: the worker holding the lease stopped answering',
                    finished_at = ?2
              WHERE state = ?3 AND lease_expires_at < ?2 AND attempts >= ?4",
        )
        .bind(JOB_FAILED)
        .bind(now)
        .bind(JOB_CLAIMED)
        .bind(MAX_ATTEMPTS)
        .execute(&conn)
        .await
        .map_err(|e| StoreError::db("models: reclaim", e))?;
        let requeued = query(
            "UPDATE model_jobs
                SET state = ?1, claimed_by = NULL, claimed_at = NULL, lease_expires_at = NULL
              WHERE state = ?2 AND lease_expires_at < ?3",
        )
        .bind(JOB_PENDING)
        .bind(JOB_CLAIMED)
        .bind(now)
        .execute(&conn)
        .await
        .map_err(|e| StoreError::db("models: reclaim", e))?;
        if requeued > 0 {
            job_wake().notify_one();
        }
        Ok((requeued, failed))
    }
}

/// The capability a guest links as `hive_models`: `submit` and `result`.
pub struct GuestModels {
    jobs: ModelJobs,
    blobs: Arc<Catalog>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SubmitBody {
    capability: String,
    blob: String,
    text: String,
    options: serde_json::Value,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ResultBody {
    job: String,
}

impl GuestModels {
    pub fn new(store: Store, blobs: Arc<Catalog>) -> GuestModels {
        GuestModels {
            jobs: ModelJobs::new(store),
            blobs,
        }
    }

    async fn submit_inner(&self, req: &Request) -> Result<Response> {
        req.caller
            .validate()
            .map_err(|e| HostError::denied(format!("models.submit: {e}")))?;
        let input: SubmitBody = serde_json::from_slice(&req.body)
            .map_err(|_| HostError::invalid("models.submit: body is not an object"))?;
        let conn = self.jobs.store.conn().await?;
        resolve_active_install(&conn, req.caller.install_id).await?;
        drop(conn);
        let input_kind = match (input.blob.trim().is_empty(), input.text.is_empty()) {
            (false, true) => {
                let h = Hash::parse(input.blob.trim())
                    .map_err(|_| HostError::invalid("models.submit: malformed blob address"))?;
                // The caller's references, never the global hash space. The
                // same not-found for "no such blob" and "not yours".
                let (desc, _) = self
                    .blobs
                    .resolve(&req.caller.cred, h)
                    .await
                    .map_err(|_| HostError::not_found("blob not found"))?;
                JobInput::Blob(h, desc.mime)
            }
            (true, false) => JobInput::Text(input.text),
            _ => {
                return Err(StoreError::Host(HostError::invalid(
                    "models.submit: exactly one of blob or text",
                )));
            }
        };
        let options = if input.options.is_null() {
            serde_json::json!({})
        } else {
            input.options
        };
        let id = self
            .jobs
            .submit(
                &req.caller.cred,
                req.caller.install_id,
                &JobSpec {
                    capability: input.capability,
                    input: input_kind,
                    options,
                },
            )
            .await?;
        Ok(Response::trusted(serde_json::to_vec(
            &serde_json::json!({ "job": id }),
        )?))
    }

    async fn result_inner(&self, req: &Request) -> Result<Response> {
        req.caller
            .validate()
            .map_err(|e| HostError::denied(format!("models.result: {e}")))?;
        let input: ResultBody = serde_json::from_slice(&req.body)
            .map_err(|_| HostError::invalid("models.result: body is not an object"))?;
        let job = Uuid::parse_str(input.job.trim())
            .map_err(|_| HostError::invalid("models.result: job is not a uuid"))?;
        let view = self
            .jobs
            .result(&req.caller.cred, req.caller.install_id, job)
            .await?
            .ok_or_else(|| HostError::not_found("no such job"))?;
        // A result is a model's words about content: untrusted, whatever
        // asked for it (invariant 9). A job with no result yet says nothing
        // and taints nothing.
        let level = if view.result.is_some() {
            Level::Untrusted
        } else {
            Level::Trusted
        };
        Ok(Response::with_trust(level, serde_json::to_vec(&view)?))
    }
}

fn to_host(e: StoreError) -> HostError {
    match e {
        StoreError::Host(h) => h,
        StoreError::Denied => HostError::denied("denied"),
        StoreError::InvalidInput(m) => HostError::invalid(m),
        other => {
            tracing::error!(err = %other, "models capability");
            HostError::error("models: internal")
        }
    }
}

#[async_trait]
impl Models for GuestModels {
    async fn submit(&self, req: Request) -> std::result::Result<Response, HostError> {
        self.submit_inner(&req).await.map_err(to_host)
    }
    async fn result(&self, req: Request) -> std::result::Result<Response, HostError> {
        self.result_inner(&req).await.map_err(to_host)
    }
}

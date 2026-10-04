//! The local model seam's worker (D42): the daemon role that claims model
//! jobs and answers them through OpenAI-shaped endpoints.
//!
//! Four capabilities, each bound by configuration to a URL and a model
//! name: `read` (chat completions with an image), `transcribe`
//! (`/audio/transcriptions`), `generate` (chat completions, with an
//! optional JSON schema and an optional closed list of answers), `embed`
//! (`/embeddings`). Nothing here knows which engine is behind a URL, and
//! nothing here writes a document: a job lands as a result on its row and
//! the store appends the finished event; the app that asked reads it.
//!
//! The claim pattern is the chat worker's: a lease, a heartbeat while the
//! call runs, a reclaimer for abandoned claims, a kick from the submitter.
//! Bytes for a blob job are read by hash through the catalogue's driver,
//! as a host-internal consumer of a reference the store already checked at
//! submit (invariant 3 is enforced there; this is the half that reads).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use hive_blob::{Catalog, Hash, Range};
use hive_store::{ClaimedJob, ModelJobs, Store, job_wake};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// One OpenAI-shaped endpoint for one capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The API base, up to and excluding `/chat/completions` and the rest:
    /// `http://models.local:8080/v1`.
    pub url: String,
    pub model: String,
    /// Sent as a bearer token when non-empty. A local server needs none.
    pub api_key: String,
}

/// Which capabilities this worker can answer. A job for an absent one
/// fails at once with the variable to set: the daemon is the one worker,
/// and an app learning "not configured" beats an app waiting forever.
#[derive(Clone, Debug, Default)]
pub struct Endpoints {
    pub read: Option<Endpoint>,
    pub transcribe: Option<Endpoint>,
    pub generate: Option<Endpoint>,
    pub embed: Option<Endpoint>,
}

impl Endpoints {
    pub fn get(&self, capability: &str) -> Option<&Endpoint> {
        match capability {
            "read" => self.read.as_ref(),
            "transcribe" => self.transcribe.as_ref(),
            "generate" => self.generate.as_ref(),
            "embed" => self.embed.as_ref(),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_none()
            && self.transcribe.is_none()
            && self.generate.is_none()
            && self.embed.is_none()
    }

    /// Reads the endpoints from the environment. `HIVE_SANDBOX_MODELS_URL`
    /// and `_KEY` are the shared defaults; each capability may override
    /// with `HIVE_SANDBOX_MODELS_<CAP>_URL` and `_KEY`, and is enabled by
    /// `HIVE_SANDBOX_MODELS_<CAP>_MODEL` naming its model.
    pub fn from_env() -> Endpoints {
        let env = |k: &str| std::env::var(k).unwrap_or_default().trim().to_string();
        let shared_url = env("HIVE_SANDBOX_MODELS_URL");
        let shared_key = env("HIVE_SANDBOX_MODELS_KEY");
        let cap = |name: &str| {
            let upper = name.to_ascii_uppercase();
            let model = env(&format!("HIVE_SANDBOX_MODELS_{upper}_MODEL"));
            if model.is_empty() {
                return None;
            }
            let url = {
                let own = env(&format!("HIVE_SANDBOX_MODELS_{upper}_URL"));
                if own.is_empty() {
                    shared_url.clone()
                } else {
                    own
                }
            };
            if url.is_empty() {
                return None;
            }
            let api_key = {
                let own = env(&format!("HIVE_SANDBOX_MODELS_{upper}_KEY"));
                if own.is_empty() {
                    shared_key.clone()
                } else {
                    own
                }
            };
            Some(Endpoint {
                url: url.trim_end_matches('/').to_string(),
                model,
                api_key,
            })
        };
        Endpoints {
            read: cap("read"),
            transcribe: cap("transcribe"),
            generate: cap("generate"),
            embed: cap("embed"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Names the worker on the claim it holds.
    pub name: String,
    pub endpoints: Endpoints,
    /// How long one call may take before the job fails. Zero is ten minutes.
    pub call_timeout: Duration,
    /// Zero is one second.
    pub poll_interval: Duration,
    /// Zero is one.
    pub concurrency: usize,
}

impl Config {
    fn defaults(mut self) -> Config {
        if self.call_timeout.is_zero() {
            self.call_timeout = Duration::from_secs(600);
        }
        if self.poll_interval.is_zero() {
            self.poll_interval = Duration::from_secs(1);
        }
        if self.concurrency == 0 {
            self.concurrency = 1;
        }
        self
    }
}

/// The lease a claim is taken under and extended by each heartbeat.
const LEASE: Duration = Duration::from_secs(60);
const HEARTBEAT: Duration = Duration::from_secs(20);
/// Bounds the bytes a blob job reads into memory for one call.
const MAX_INPUT_BYTES: u64 = 64 << 20;

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("models worker: {0}")]
    Config(String),
    #[error(transparent)]
    Store(#[from] hive_store::StoreError),
}

pub struct Worker {
    jobs: ModelJobs,
    blobs: Arc<Catalog>,
    client: reqwest::Client,
    cfg: Config,
}

impl Worker {
    pub fn new(store: Store, blobs: Arc<Catalog>, cfg: Config) -> Result<Worker, WorkerError> {
        let cfg = cfg.defaults();
        if cfg.name.is_empty() {
            return Err(WorkerError::Config("a worker needs a name".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(cfg.call_timeout)
            .build()
            .map_err(|e| WorkerError::Config(format!("http client: {e}")))?;
        Ok(Worker {
            jobs: ModelJobs::new(store),
            blobs,
            client,
            cfg,
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Drives the claim loops and the reclaimer until `cancel` fires.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let mut loops = Vec::new();
        for _ in 0..self.cfg.concurrency {
            let w = self.clone();
            let c = cancel.clone();
            loops.push(tokio::spawn(async move { w.work_loop(c).await }));
        }
        let mut reclaim = tokio::time::interval(Duration::from_secs(30));
        reclaim.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = reclaim.tick() => {
                    match self.jobs.reclaim_lapsed().await {
                        Ok((0, 0)) => {}
                        Ok((requeued, failed)) => tracing::warn!(requeued, failed, "models: reclaimed lapsed claims"),
                        Err(e) => tracing::error!(err = %e, "models: reclaim"),
                    }
                }
            }
        }
        for l in loops {
            let _ = l.await;
        }
    }

    async fn work_loop(&self, cancel: CancellationToken) {
        loop {
            match self.run_one().await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => tracing::error!(err = %e, "models worker"),
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = job_wake().notified() => {}
                _ = tokio::time::sleep(self.cfg.poll_interval) => {}
            }
        }
    }

    /// Claims and answers one job. True when there was one.
    pub async fn run_one(&self) -> Result<bool, WorkerError> {
        let Some(job) = self.jobs.claim(&self.cfg.name, LEASE).await? else {
            return Ok(false);
        };
        let outcome = self.answer(&job).await;
        if let Err(e) = &outcome {
            tracing::warn!(job = %job.id, capability = %job.capability, err = %e, "models: job failed");
        }
        let landed = self.jobs.finish(job.id, &self.cfg.name, outcome).await?;
        if !landed {
            tracing::warn!(job = %job.id, "models: the claim lapsed before the answer landed; discarded");
        }
        Ok(true)
    }

    /// The call, with the heartbeat running beside it.
    async fn answer(&self, job: &ClaimedJob) -> Result<serde_json::Value, String> {
        let Some(ep) = self.cfg.endpoints.get(&job.capability) else {
            return Err(format!(
                "no endpoint is configured for {:?} on this daemon (HIVE_SANDBOX_MODELS_{}_MODEL)",
                job.capability,
                job.capability.to_ascii_uppercase()
            ));
        };
        let heartbeat = async {
            let mut tick = tokio::time::interval(HEARTBEAT);
            tick.tick().await;
            loop {
                tick.tick().await;
                match self.jobs.extend_lease(job.id, &self.cfg.name, LEASE).await {
                    Ok(true) => {}
                    Ok(false) => return "the claim is no longer this worker's".to_string(),
                    Err(e) => return format!("heartbeat: {e}"),
                }
            }
        };
        tokio::select! {
            res = self.call(ep, job) => res,
            lost = heartbeat => Err(lost),
        }
    }

    async fn call(&self, ep: &Endpoint, job: &ClaimedJob) -> Result<serde_json::Value, String> {
        match job.capability.as_str() {
            "read" => {
                let (bytes, mime) = self.input_bytes(job).await?;
                let prompt = job.options["prompt"]
                    .as_str()
                    .unwrap_or("Transcribe every word on this page exactly as written, preserving line breaks. Output only the text.");
                let data_url = format!(
                    "data:{mime};base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                );
                let body = serde_json::json!({
                    "model": ep.model,
                    "messages": [{"role": "user", "content": [
                        {"type": "text", "text": prompt},
                        {"type": "image_url", "image_url": {"url": data_url}}
                    ]}],
                });
                let text = self.chat(ep, body).await?;
                Ok(serde_json::json!({ "text": text }))
            }
            "transcribe" => {
                let (bytes, mime) = self.input_bytes(job).await?;
                let filename = format!("audio.{}", extension_for(&mime));
                let part = reqwest::multipart::Part::bytes(bytes)
                    .file_name(filename)
                    .mime_str(&mime)
                    .map_err(|e| format!("transcribe: mime: {e}"))?;
                let form = reqwest::multipart::Form::new()
                    .text("model", ep.model.clone())
                    .text("response_format", "verbose_json")
                    .part("file", part);
                let v: serde_json::Value = self
                    .post(ep, "/audio/transcriptions", |r| r.multipart(form))
                    .await?;
                let text = v["text"].as_str().unwrap_or_default().to_string();
                let mut out = serde_json::json!({ "text": text });
                if let Some(segs) = v.get("segments") {
                    out["segments"] = segs.clone();
                }
                if let Some(lang) = v.get("language") {
                    out["language"] = lang.clone();
                }
                Ok(out)
            }
            "generate" => {
                let text = self.input_text(job).await?;
                let prompt = job.options["prompt"].as_str().unwrap_or("");
                let mut messages = Vec::new();
                if let Some(system) = job.options["system"].as_str() {
                    messages.push(serde_json::json!({"role": "system", "content": system}));
                }
                let user = if prompt.is_empty() {
                    text.clone()
                } else {
                    format!("{prompt}\n\n{text}")
                };
                messages.push(serde_json::json!({"role": "user", "content": user}));
                let mut body = serde_json::json!({ "model": ep.model, "messages": messages });
                let schema = job.options.get("schema").filter(|s| s.is_object());
                if let Some(schema) = schema {
                    body["response_format"] = serde_json::json!({
                        "type": "json_schema",
                        "json_schema": {"name": "answer", "schema": schema, "strict": true}
                    });
                }
                let content = self.chat(ep, body).await?;
                // A closed list is the platform's one contribution to
                // categorisation: an answer off it is a refusal, not a new
                // category (D42 §1).
                if let Some(choices) = job.options["choices"].as_array() {
                    let answer = content.trim().trim_matches('"');
                    let hit = choices
                        .iter()
                        .filter_map(|c| c.as_str())
                        .find(|c| c.eq_ignore_ascii_case(answer));
                    return match hit {
                        Some(c) => Ok(serde_json::json!({ "choice": c })),
                        None => Err(format!(
                            "the model answered {answer:?}, which is not one of the choices"
                        )),
                    };
                }
                if schema.is_some() {
                    let json: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
                        format!("the model's answer is not the JSON its schema asked for: {e}")
                    })?;
                    return Ok(serde_json::json!({ "json": json }));
                }
                Ok(serde_json::json!({ "text": content }))
            }
            "embed" => {
                let text = self.input_text(job).await?;
                let body = serde_json::json!({ "model": ep.model, "input": text });
                let v: serde_json::Value = self.post(ep, "/embeddings", |r| r.json(&body)).await?;
                let embedding = v["data"][0]["embedding"]
                    .as_array()
                    .ok_or_else(|| "embeddings: no data[0].embedding in the answer".to_string())?;
                let dim = embedding.len();
                Ok(serde_json::json!({ "embedding": embedding, "dim": dim }))
            }
            other => Err(format!("{other:?} is not a model capability")),
        }
    }

    async fn input_text(&self, job: &ClaimedJob) -> Result<String, String> {
        if let Some(t) = &job.input_text {
            return Ok(t.clone());
        }
        let (bytes, mime) = self.input_bytes(job).await?;
        if !mime.starts_with("text/") && mime != "application/json" {
            return Err(format!(
                "{}: a {mime} blob is not text; use read or transcribe first",
                job.capability
            ));
        }
        String::from_utf8(bytes).map_err(|_| "the blob is not UTF-8 text".to_string())
    }

    async fn input_bytes(&self, job: &ClaimedJob) -> Result<(Vec<u8>, String), String> {
        let Some(h) = job.input_blob else {
            return Err(format!(
                "{}: this capability takes a blob, not text",
                job.capability
            ));
        };
        read_blob(&self.blobs, h).await.map(|b| {
            (
                b,
                job.input_mime
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".into()),
            )
        })
    }

    async fn chat(&self, ep: &Endpoint, body: serde_json::Value) -> Result<String, String> {
        let v: serde_json::Value = self
            .post(ep, "/chat/completions", |r| r.json(&body))
            .await?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "chat: no choices[0].message.content in the answer".to_string())
    }

    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        ep: &Endpoint,
        path: &str,
        build: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<T, String> {
        let mut req = self.client.post(format!("{}{path}", ep.url));
        if !ep.api_key.is_empty() {
            req = req.bearer_auth(&ep.api_key);
        }
        let res = build(req)
            .send()
            .await
            .map_err(|e| format!("{path}: {}", redact(&e.to_string())))?;
        let status = res.status();
        let body = res
            .bytes()
            .await
            .map_err(|e| format!("{path}: reading the answer: {e}"))?;
        if !status.is_success() {
            let snippet = String::from_utf8_lossy(&body[..body.len().min(300)]).into_owned();
            return Err(format!("{path}: {status}: {snippet}"));
        }
        serde_json::from_slice(&body).map_err(|e| format!("{path}: the answer is not JSON: {e}"))
    }
}

/// Reads a blob's bytes by hash through the driver, bounded. The reference
/// was checked when the job was submitted; this is the host reading what
/// the store already authorised.
async fn read_blob(blobs: &Catalog, h: Hash) -> Result<Vec<u8>, String> {
    let mut rd = blobs
        .driver()
        .open(h, Range::FULL)
        .await
        .map_err(|e| format!("blob {h}: {e}"))?;
    let mut out = Vec::new();
    (&mut rd)
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut out)
        .await
        .map_err(|e| format!("blob {h}: read: {e}"))?;
    if out.len() as u64 > MAX_INPUT_BYTES {
        return Err(format!(
            "blob {h} is larger than the {MAX_INPUT_BYTES} bytes one job may read"
        ));
    }
    Ok(out)
}

fn extension_for(mime: &str) -> &'static str {
    match mime {
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/ogg" => "ogg",
        "audio/webm" => "webm",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a",
        _ => "bin",
    }
}

/// An error string from the HTTP client can carry the URL, and a URL can
/// carry a key in its query. Keep the message, drop the query.
fn redact(s: &str) -> String {
    s.split('?').next().unwrap_or(s).to_string()
}

/// What a worker reports about itself, for the readiness page.
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub name: String,
    pub capabilities: Vec<&'static str>,
}

impl Worker {
    pub fn status(&self) -> Status {
        let e = &self.cfg.endpoints;
        let mut capabilities = Vec::new();
        if e.read.is_some() {
            capabilities.push("read");
        }
        if e.transcribe.is_some() {
            capabilities.push("transcribe");
        }
        if e.generate.is_some() {
            capabilities.push("generate");
        }
        if e.embed.is_some() {
            capabilities.push("embed");
        }
        Status {
            name: self.cfg.name.clone(),
            capabilities,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_string_never_reaches_an_error() {
        assert_eq!(
            redact("error sending request for url (http://x/v1/chat?key=secret)"),
            "error sending request for url (http://x/v1/chat"
        );
    }

    #[test]
    fn endpoints_need_a_model_and_a_url() {
        // No environment in a unit test; the shape is what is checked.
        let e = Endpoints::default();
        assert!(e.is_empty());
        assert!(e.get("read").is_none());
        assert!(e.get("paint").is_none());
    }
}

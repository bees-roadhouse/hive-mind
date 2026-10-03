//! Putting an app and a file INTO the platform over HTTP: `POST /blobs` and
//! `POST /apps`, plus `GET /apps` to see what is installed.
//!
//! Neither handler decides anything. An upload writes a reference through
//! the catalogue under the caller's credential, which is what makes the
//! bytes theirs (invariant 3); an install goes through `register_build`,
//! `stage_install` and `activate_install` exactly as the registry and the
//! tests do, so who may activate is the store's answer (D19.4) and what the
//! app may reach is derived there from its manifest (#86). A module named by
//! hash is read through the caller's own references, never the global hash
//! space: naming a digest is not holding the bytes.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use futures::StreamExt;
use hive_blob::{CreateUpload, Hash, Provenance, Range, RefSpec, SourceKind};
use hive_httpauth::Authed;
use hive_manifest::Manifest;
use hive_registry::prepare;
use hive_store::{
    BuildSpec, InstallSpec, StoreError, activate_install, register_build, stage_install,
};
use hive_trust::Level;
use hive_wasmhost::{BytesSource, CapabilitySet, Exports, Module, hash_module};
use http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::{AppState, fail, json};

/// Bounds one upload. Streamed to the driver as it arrives, so this is a
/// policy limit rather than a memory one.
const MAX_UPLOAD: u64 = 1 << 30;
/// Bounds an install request's JSON.
const MAX_INSTALL_BODY: usize = 1 << 20;
/// Bounds a module read back for its exports. A guest is small by design.
const MAX_MODULE: u64 = 64 << 20;

#[derive(Serialize)]
struct UploadResponse {
    hash: String,
    size: u64,
    mime: String,
    trust: &'static str,
}

/// Stores bytes and writes the caller a reference to them. The response is
/// the content address; a second upload of the same bytes by anyone costs
/// no storage and still gets its own reference.
pub(crate) async fn upload(
    State(s): State<AppState>,
    Authed(cred): Authed,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(blobs) = &s.blobs else {
        return fail(StatusCode::NOT_FOUND, "not found");
    };
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > MAX_UPLOAD)
    {
        return fail(StatusCode::PAYLOAD_TOO_LARGE, "too large");
    }

    let mut up = match blobs.begin_upload(CreateUpload::default()).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(err = %e, "begin upload");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
    };
    let mut total: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => {
                let _ = up.abort().await;
                return fail(StatusCode::BAD_REQUEST, "bad_request");
            }
        };
        total += chunk.len() as u64;
        if total > MAX_UPLOAD {
            let _ = up.abort().await;
            return fail(StatusCode::PAYLOAD_TOO_LARGE, "too large");
        }
        if let Err(e) = up.write(&chunk).await {
            let _ = up.abort().await;
            tracing::error!(err = %e, "upload write");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
    }
    let sealed = match up.seal().await {
        Ok(sl) => sl,
        Err(e) => {
            tracing::error!(err = %e, "upload seal");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
    };

    let Ok(tx) = s.store().begin().await else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    };
    let published = blobs
        .publish(
            &tx,
            sealed,
            &mime,
            // A person's own upload: the original of whatever it is. Trust
            // is the reference's and the uploader is the principal, so it
            // is theirs to trust; a guest that reads it later gets that
            // level and no more.
            &Provenance::original(),
            &RefSpec {
                cred,
                source_kind: SourceKind::Upload,
                source_id: Uuid::new_v4().to_string(),
                trust: Level::Trusted,
            },
        )
        .await;
    let (desc, _) = match published {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(err = %e, actor = %cred.actor_id, "publish upload");
            return fail(StatusCode::BAD_REQUEST, "bad_request");
        }
    };
    if tx.commit().await.is_err() {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    }
    json(
        StatusCode::CREATED,
        &UploadResponse {
            hash: desc.hash.to_string(),
            size: desc.size,
            mime: desc.mime.clone(),
            trust: Level::Trusted.as_str(),
        },
    )
}

#[derive(Deserialize)]
struct InstallRequest {
    manifest: Manifest,
    /// The content address of the guest module the caller uploaded, or
    /// absent for an app with no guest code.
    #[serde(default)]
    module: String,
}

#[derive(Serialize)]
struct InstallResponse {
    install: Uuid,
    build: Uuid,
    app: String,
    schema: String,
    state: &'static str,
}

/// Registers, stages and activates an app for the caller's own principal.
///
/// The three acts are the store's and happen in one transaction, so a
/// manifest the registry refuses, a module whose exports do not match it, or
/// a `uses` the owner cannot satisfy leaves nothing behind. Activation is
/// decided by the store: a person installs their own apps; an AI acting for
/// them is refused (D19.4), because installing is a person's act.
pub(crate) async fn install(
    State(s): State<AppState>,
    Authed(cred): Authed,
    body: Body,
) -> Response {
    let (Some(blobs), Some(host)) = (&s.blobs, &s.host) else {
        return fail(StatusCode::NOT_FOUND, "not found");
    };
    let Ok(bytes) = axum::body::to_bytes(body, MAX_INSTALL_BODY).await else {
        return fail(StatusCode::BAD_REQUEST, "bad_request");
    };
    let Ok(req) = serde_json::from_slice::<InstallRequest>(&bytes) else {
        return fail(StatusCode::BAD_REQUEST, "bad_request");
    };
    let m = req.manifest;

    // The module's exports, read from bytes the CALLER holds. The catalogue
    // answers "no such blob" and "not yours" the same way, and so does this.
    let exports = if req.module.trim().is_empty() {
        Exports::none()
    } else {
        let Ok(h) = Hash::parse(req.module.trim()) else {
            return fail(StatusCode::BAD_REQUEST, "malformed blob address");
        };
        let (desc, _, mut rd) = match blobs.open(&cred, h, Range::FULL).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(actor = %cred.actor_id, blob = %h, err = %e, "module read refused");
                return fail(StatusCode::NOT_FOUND, "blob not found");
            }
        };
        if desc.size > MAX_MODULE {
            return fail(StatusCode::PAYLOAD_TOO_LARGE, "too large");
        }
        let mut wasm = Vec::with_capacity(desc.size as usize);
        if rd.read_to_end(&mut wasm).await.is_err() {
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
        // The catalogue's address is the digest; `module_exports` binds what
        // it finds to the hash it is told, so the two are held equal here
        // rather than assumed.
        if hash_module(&wasm) != h.to_string() {
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
        let module = Module {
            hash: h.to_string(),
            app: m.name.clone(),
            version: m.version.to_string(),
            memory_pages: 0,
            capabilities: CapabilitySet::from_names(&m.capabilities),
            ..Default::default()
        };
        match host
            .module_exports(&module, Arc::new(BytesSource::new(wasm.as_slice())))
            .await
        {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(actor = %cred.actor_id, app = %m.name, err = %e, "module does not load");
                return fail(StatusCode::UNPROCESSABLE_ENTITY, "invalid_module");
            }
        }
    };

    let prepared = match prepare(&m, &exports) {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(actor = %cred.actor_id, app = %m.name, err = %e, "manifest refused");
            return fail(StatusCode::UNPROCESSABLE_ENTITY, "invalid_manifest");
        }
    };
    let owner = cred.owner_of();
    let spec = match prepared.install_spec(owner.kind.as_str(), &owner.id.to_string()) {
        Ok(sp) => sp,
        Err(_) => return fail(StatusCode::UNPROCESSABLE_ENTITY, "invalid_manifest"),
    };

    let Ok(tx) = s.store().begin().await else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    };
    let registered = register_build(
        &tx,
        &BuildSpec {
            spec,
            owner: Some(owner),
            trust: "local".into(),
        },
        &cred,
    )
    .await;
    let reg = match registered {
        Ok(r) => r,
        Err(e) => return install_failure(&cred, &m.name, "register", e),
    };
    let staged = stage_install(
        &tx,
        &InstallSpec {
            build_id: reg.build_id,
            slug: m.name.clone(),
            owner,
        },
        &cred,
    )
    .await;
    let install = match staged {
        Ok(i) => i,
        Err(e) => return install_failure(&cred, &m.name, "stage", e),
    };
    if let Err(e) = activate_install(&tx, install, &cred).await {
        return install_failure(&cred, &m.name, "activate", e);
    }
    if tx.commit().await.is_err() {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    }
    json(
        StatusCode::CREATED,
        &InstallResponse {
            install,
            build: reg.build_id,
            app: m.name,
            schema: reg.schema_name,
            state: "active",
        },
    )
}

/// The store's refusals, each to the status it means. The transaction is
/// dropped by the caller's return, which rolls it back.
fn install_failure(
    cred: &hive_identity::Credential,
    app: &str,
    step: &str,
    e: StoreError,
) -> Response {
    match &e {
        StoreError::Denied | StoreError::NotHuman(_) => fail(StatusCode::FORBIDDEN, "forbidden"),
        StoreError::Other(msg) if msg.contains("no active install of") => {
            fail(StatusCode::CONFLICT, "unmet_uses")
        }
        StoreError::UnsafeIdentifier(_) | StoreError::NotImplemented(_) => {
            fail(StatusCode::UNPROCESSABLE_ENTITY, "invalid_manifest")
        }
        _ if e.is_constraint() => fail(StatusCode::CONFLICT, "conflict"),
        _ => {
            tracing::error!(actor = %cred.actor_id, app = %app, step = %step, err = %e, "install");
            fail(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

#[derive(Serialize)]
struct InstallRow {
    install: Uuid,
    app: String,
    state: String,
    schema: String,
}

/// The caller's principal's installs, every state. What a person sees on a
/// settings page; the candidate set a guest sees is the predicate's and is
/// served by the tool surface, not here.
pub(crate) async fn list(State(s): State<AppState>, Authed(cred): Authed) -> Response {
    let Ok(conn) = s.store().conn().await else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    };
    match hive_store::installs_of(&conn, cred.owner_of()).await {
        Ok(rows) => {
            let out: Vec<InstallRow> = rows
                .into_iter()
                .map(|i| InstallRow {
                    install: i.id,
                    app: i.slug,
                    state: i.state,
                    schema: i.schema_name,
                })
                .collect();
            json(StatusCode::OK, &serde_json::json!({ "installs": out }))
        }
        Err(e) => {
            tracing::error!(actor = %cred.actor_id, err = %e, "list installs");
            fail(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

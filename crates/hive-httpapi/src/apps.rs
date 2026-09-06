//! An install's mounted routes: `/apps/{app}/...`. The host owns the prefix
//! (D2.3), so two installs cannot collide and an app cannot mount itself over
//! a platform endpoint. Resolution, permission and dispatch live behind the
//! [`AppRouter`] seam; this module maps HTTP to it and back.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hive_httpauth::Authed;
use hive_identity::Credential;
use hive_trust::Level;
use http::{HeaderValue, Method, StatusCode, Uri};

use crate::AppState;

/// One request routed to an app. `path` is relative to the mount point and
/// starts with `/`.
#[derive(Clone, Debug)]
pub struct AppRequest {
    pub cred: Credential,
    pub app: String,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub body: Vec<u8>,
}

/// What the app answered. `body` is JSON by the ABI's contract.
#[derive(Clone, Debug)]
pub struct AppResponse {
    pub body: Vec<u8>,
    pub trust: Level,
    pub tainted_by: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// No such app, no such route, or not yours. One answer, on purpose.
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    /// The app ran, or tried to, and failed. The message is the app's own.
    #[error("{0}")]
    Failed(String),
}

/// Resolves and runs one app route. The daemon implements it over the
/// surfaces crate; tests implement it in a few lines.
#[async_trait]
pub trait AppRouter: Send + Sync {
    async fn call(&self, req: AppRequest) -> Result<AppResponse, AppError>;
}

const MAX_BODY: usize = 1 << 20;

/// The trust header on every app response, so a client composing app output
/// into anything a model reads can see what it is composing.
pub const TRUST_HEADER: &str = "x-hive-trust";

fn error(status: StatusCode, code: &str, detail: Option<String>) -> Response {
    let mut body = serde_json::json!({"error": code});
    if let Some(d) = detail {
        body["detail"] = serde_json::Value::String(d);
    }
    (status, axum::Json(body)).into_response()
}

async fn dispatch(
    router: Arc<dyn AppRouter>,
    cred: Credential,
    app: String,
    rest: String,
    method: Method,
    uri: Uri,
    body: Body,
) -> Response {
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "too_large", None);
    };
    let path = if rest.is_empty() {
        "/".to_string()
    } else {
        format!("/{rest}")
    };
    let req = AppRequest {
        cred,
        app,
        method: method.as_str().to_string(),
        path,
        query: uri.query().map(str::to_string),
        body: bytes.to_vec(),
    };
    match router.call(req).await {
        Ok(res) => {
            let mut r = (
                StatusCode::OK,
                [(http::header::CONTENT_TYPE, "application/json")],
                res.body,
            )
                .into_response();
            if let Ok(v) = HeaderValue::from_str(res.trust.as_str()) {
                r.headers_mut().insert(TRUST_HEADER, v);
            }
            r
        }
        Err(AppError::NotFound) => error(StatusCode::NOT_FOUND, "not_found", None),
        Err(AppError::BadRequest(d)) => error(StatusCode::BAD_REQUEST, "bad_request", Some(d)),
        Err(AppError::Failed(d)) => error(StatusCode::BAD_GATEWAY, "app_failed", Some(d)),
    }
}

/// `/apps/{app}/{*rest}`.
pub(crate) async fn route(
    State(s): State<AppState>,
    Authed(cred): Authed,
    Path((app, rest)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    body: Body,
) -> Response {
    let Some(router) = s.apps.clone() else {
        return error(StatusCode::NOT_FOUND, "not_found", None);
    };
    dispatch(router, cred, app, rest, method, uri, body).await
}

/// `/apps/{app}`: the install's root route, `/`.
pub(crate) async fn root(
    State(s): State<AppState>,
    Authed(cred): Authed,
    Path(app): Path<String>,
    method: Method,
    uri: Uri,
    body: Body,
) -> Response {
    let Some(router) = s.apps.clone() else {
        return error(StatusCode::NOT_FOUND, "not_found", None);
    };
    dispatch(router, cred, app, String::new(), method, uri, body).await
}

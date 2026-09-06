//! The browser client's static half: the stylesheet, htmx, and the two small
//! scripts the server-rendered pages load, embedded so a daemon binary is the
//! whole deployment, served under `/assets/` with the Content-Security-Policy
//! every HTML response also carries.
//!
//! The pages themselves are rendered by `hive-httpapi` (D32: server-rendered,
//! htmx-swapped); this crate serves only bytes that never change per request.
//! The policy is what lets a rendered message be inert: no inline script, no
//! inline style, no remote anything, so a message body that somehow became
//! markup would still have nowhere to send anything.

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::{HeaderValue, StatusCode, header};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets"]
struct Files;

/// What the page may load: itself and nothing else.
pub const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
img-src 'self' data:; font-src 'self'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'";

/// One prefix, so an API route can never be shadowed by a file. Merge it into
/// the API router.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/assets/{*path}", get(asset))
}

async fn asset(Path(path): Path<String>) -> Response {
    // The file server canonicalises nothing: the path is looked up verbatim in
    // the embedded set, so `..` is just a name that matches no file.
    serve(&path)
}

fn serve(name: &str) -> Response {
    let Some(file) = Files::get(name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(POLICY),
        )
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        // Nothing here is worth caching across a deploy: a stale script
        // against a new set of fragments is a support call.
        .header(header::CACHE_CONTROL, "no-cache");
    if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", hex(&file.metadata.sha256_hash()))) {
        resp = resp.header(header::ETAG, v);
    }
    resp.body(Body::from(file.data.into_owned()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// An embedded asset, for a test that wants to read it without a server.
pub fn asset_bytes(name: &str) -> Option<Vec<u8>> {
    Files::get(name).map(|f| f.data.into_owned())
}

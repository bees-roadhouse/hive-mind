//! The daemon's HTTP surface: device enrollment, the reads a client needs,
//! liveness, the event stream and chat. Two routers would be two places to
//! forget an endpoint, so it is one.
//!
//! What this crate is deliberately NOT: a place for authorization decisions.
//! Who may issue a credential is enforced by the `credentials_issue_check`
//! trigger (D19.3) and who may see data is enforced by the store's guard; the
//! handlers below resolve facts from the presented token and pass them on.

pub mod apps;
mod blobs;
mod chat;
mod chatstream;
mod credentials;
pub mod mcp;
mod readyz;
mod session;
pub mod ui;

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRef, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hive_blob::Catalog;
use hive_bus::Bus;
use hive_chat::Hub;
use hive_httpauth::Auth;
use hive_store::{Chat, Store};
use http::{HeaderMap, StatusCode, Uri, header};

pub use apps::{AppError, AppRequest, AppResponse, AppRouter};
pub use blobs::parse_range;

/// What the daemon owns and the router borrows.
#[derive(Clone, Default)]
pub struct Options {
    /// Printed by `--version` and reported by /healthz and /whoami.
    pub version: String,
    /// Enables the blob read route. A daemon without a catalog simply does
    /// not serve blobs, rather than serving them unauthorized.
    pub blobs: Option<Arc<Catalog>>,
    /// Enables the conversation routes. `hub` is where the turn worker
    /// publishes; the stream subscribes to it, so the two must share one, and
    /// a missing hub gets a private one nothing publishes to ... a stream that
    /// is correct and never live.
    pub chat: Option<Arc<Chat>>,
    pub hub: Option<Hub>,
    /// Called after a message is accepted, so an in-process worker does not
    /// wait out its poll interval.
    pub wake: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The deployment serves plain HTTP and the session cookie must not be
    /// Secure, or no browser would ever send it. Off by default; the operator
    /// says so once, deliberately (D26).
    pub plain_http: bool,
    /// Enables `POST /mcp`. Absent, the endpoint is not mounted: a daemon with
    /// no tool surface answers 404 rather than an empty list.
    pub mcp: Option<Arc<hive_mcp::Server>>,
    /// Enables `/apps/{app}/...`. Same rule.
    pub apps: Option<Arc<dyn AppRouter>>,
}

#[derive(Clone)]
pub struct AppState {
    pub(crate) store: Option<Store>,
    pub(crate) bus: Option<Bus>,
    pub(crate) auth: Option<Auth>,
    pub(crate) blobs: Option<Arc<Catalog>>,
    pub(crate) chat: Option<Arc<Chat>>,
    pub(crate) hub: Hub,
    pub(crate) wake: Option<Arc<dyn Fn() + Send + Sync>>,
    pub(crate) plain_http: bool,
    pub(crate) version: String,
    pub(crate) mcp: Option<Arc<hive_mcp::Server>>,
    pub(crate) apps: Option<Arc<dyn AppRouter>>,
}

impl FromRef<AppState> for Auth {
    fn from_ref(s: &AppState) -> Auth {
        s.auth
            .clone()
            .expect("authenticated routes are only mounted with a store")
    }
}

impl AppState {
    pub(crate) fn store(&self) -> &Store {
        self.store
            .as_ref()
            .expect("route mounted only with a store")
    }
    pub(crate) fn chat(&self) -> &Arc<Chat> {
        self.chat.as_ref().expect("route mounted only with chat")
    }
}

/// Builds the whole router. `store` may be `None` only when `bus` is too:
/// that shape is the workflow-only process.
pub fn router(store: Option<Store>, bus: Option<Bus>, opts: Options) -> Router {
    let state = AppState {
        auth: store.clone().map(Auth::new),
        store: store.clone(),
        bus: bus.clone(),
        blobs: opts.blobs,
        chat: opts.chat,
        hub: opts.hub.unwrap_or_default(),
        wake: opts.wake,
        plain_http: opts.plain_http,
        version: opts.version,
        mcp: opts.mcp,
        apps: opts.apps,
    };
    // Liveness only, and deliberately so: see readyz for why this one must
    // not learn to check dependencies.
    let mut app = Router::new()
        .route("/healthz", get(healthz))
        // Unauthenticated on purpose. A readiness probe runs before any
        // credential exists, and it reports only whether this process can
        // serve, never anything about the data it would serve.
        .route("/readyz", get(readyz::readyz));

    if store.is_some() && bus.is_some() {
        app = app.route("/events", get(events));
    }
    if store.is_some() {
        app = app
            .route("/whoami", get(credentials::whoami))
            .route("/credentials", post(credentials::enroll))
            // The browser's login. Unauthenticated in the extractor sense
            // because the token it exchanges arrives in the header it
            // validates itself.
            .route("/session", post(session::start).delete(session::end));
        if state.blobs.is_some() {
            // Reads resolve through the caller's refs, exactly as the guest
            // capability does. HEAD shares the handler so a client can size an
            // object before pulling it.
            app = app.route("/blobs/{hash}", get(blobs::read).head(blobs::read));
        }
        if state.mcp.is_some() {
            // Stateless JSON-RPC. No GET: this server keeps no per-client
            // stream, so there is nothing to subscribe to.
            app = app.route("/mcp", post(mcp::rpc));
        }
        if state.apps.is_some() {
            // Any method: the manifest decides which ones a route answers, and
            // a method the app did not mount is a 404 from the router, not a
            // 405 from here, because "does this route exist" is not a question
            // this layer answers for a stranger.
            app = app
                .route("/apps/{app}", axum::routing::any(apps::root))
                .route("/apps/{app}/{*rest}", axum::routing::any(apps::route));
        }
        if state.chat.is_some() {
            // The browser client: pages and fragments rendered here, static
            // assets from hive-webui, both under the same policy (D32).
            app = app
                .route("/", get(ui::index))
                .route("/ui/conversations", get(ui::list).post(ui::create))
                .route("/ui/conversations/{id}", get(ui::open))
                .route(
                    "/ui/conversations/{id}/messages",
                    get(ui::messages).post(ui::post_message),
                )
                .route("/ui/logout", post(ui::logout));
            app = app
                .route("/conversations", post(chat::create).get(chat::list))
                .route("/conversations/{id}", get(chat::get_one))
                .route(
                    "/conversations/{id}/messages",
                    get(chat::list_messages).post(chat::post_message),
                )
                .route("/conversations/{id}/stream", get(chatstream::stream));
        }
    }
    app.with_state(state)
}

async fn healthz(State(s): State<AppState>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        format!(
            "{{\"status\":\"ok\",\"version\":{}}}\n",
            serde_json::to_string(&s.version).unwrap_or_default()
        ),
    )
        .into_response()
}

async fn events(State(s): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let (Some(bus), Some(store), Some(auth)) = (&s.bus, &s.store, &s.auth) else {
        return fail(StatusCode::NOT_FOUND, "not found");
    };
    bus.serve_events(
        store,
        auth,
        headers,
        uri.query().map(String::from),
        hive_bus::SseOptions::default(),
    )
    .await
}

/// One JSON response. Every handler answers in JSON or not at all.
pub(crate) fn json(status: StatusCode, v: &impl serde::Serialize) -> Response {
    let mut body = serde_json::to_vec(v).unwrap_or_default();
    body.push(b'\n');
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// A machine-readable error with a body chosen from a closed set. The bodies
/// never embed request details: an error message that echoes input is both an
/// oracle and a log-injection vector.
pub(crate) fn fail(status: StatusCode, code: &str) -> Response {
    json(status, &serde_json::json!({ "error": code }))
}

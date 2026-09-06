//! The browser client, server-rendered. Every page and fragment the shell
//! shows is HTML the daemon composes from the same store the JSON API reads,
//! and htmx swaps the fragments in (D32). Two small scripts do what a page
//! cannot: present the credential once over the Authorization header
//! (`login.js`), and hold a conversation's SSE stream open (`stream.js`).
//!
//! Everything a person typed or an agent said reaches the page through the
//! template engine's escaping, so a message body is text and never markup,
//! and the CSP on every response makes that a property rather than a habit.
//!
//! The JSON API stays what it was; these routes are a second consumer of the
//! chat layer, not a second policy. Every read and write below goes through
//! `Chat`, which resolves access against the credential the cookie carried.
//!
//! Mutating fragment routes require htmx's `HX-Request` header. The session
//! cookie is `SameSite=Strict`, which already keeps a cross-site form from
//! carrying it; the header is the belt to that brace, and a form a browser
//! submits on its own cannot set it.

use askama::Template;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use hive_httpauth::Authed;
use hive_identity::Credential;
use hive_store::{Message, TURN_CLAIMED};
use hive_trust::Level;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::Deserialize;
use uuid::Uuid;

use crate::chat::{
    MAX_MESSAGE_LENGTH, MAX_MODEL_LENGTH, MAX_TITLE_LENGTH, chat_error, conversation_id,
};
use crate::session::cookie;
use crate::{AppState, fail};

/// A form is a person typing; the same ceiling the JSON route has.
const MAX_FORM: usize = 256 << 10;
const CONVERSATION_LIMIT: i64 = 100;
const MESSAGE_LIMIT: i64 = 200;

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    identity: String,
    signed_in: bool,
}

#[derive(Template)]
#[template(path = "app.html")]
struct AppPage {
    identity: String,
    signed_in: bool,
    conversations: String,
}

#[derive(Template)]
#[template(path = "conversations.html")]
struct ConversationsFrag {
    items: Vec<ConvItem>,
    oob: bool,
}

struct ConvItem {
    id: Uuid,
    title: String,
    meta: String,
    active: bool,
}

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadFrag {
    id: Uuid,
    messages: String,
    composer: String,
    oob: bool,
}

#[derive(Template)]
#[template(path = "messages.html")]
struct MessagesFrag {
    items: Vec<MsgItem>,
    live: Vec<LiveItem>,
}

struct MsgItem {
    seq: i32,
    role: &'static str,
    who: &'static str,
    body: String,
}

struct LiveItem {
    request_seq: i32,
    label: &'static str,
}

#[derive(Template)]
#[template(path = "composer.html")]
struct ComposerFrag {
    id: Uuid,
    oob: bool,
}

/// Every HTML response carries the same policy the asset server sends, so a
/// fragment is as inert as the page it lands in.
fn html(status: StatusCode, body: String) -> Response {
    let mut r = (status, body).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(hive_webui::POLICY),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn render(status: StatusCode, t: &impl Template) -> Response {
    match t.render() {
        Ok(body) => html(status, body),
        Err(e) => {
            tracing::error!(err = %e, "template");
            fail(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

fn rendered(t: &impl Template) -> Result<String, Box<Response>> {
    t.render().map_err(|e| {
        tracing::error!(err = %e, "template");
        Box::new(fail(StatusCode::INTERNAL_SERVER_ERROR, "internal"))
    })
}

/// A cross-site form cannot set a custom header; htmx always does.
fn require_htmx(headers: &HeaderMap) -> Result<(), Box<Response>> {
    match headers.get("hx-request").and_then(|v| v.to_str().ok()) {
        Some("true") => Ok(()),
        _ => Err(Box::new(fail(StatusCode::BAD_REQUEST, "bad_request"))),
    }
}

async fn form<T: for<'de> Deserialize<'de>>(body: Body) -> Result<T, Box<Response>> {
    let bytes = axum::body::to_bytes(body, MAX_FORM)
        .await
        .map_err(|_| Box::new(fail(StatusCode::BAD_REQUEST, "bad_request")))?;
    serde_urlencoded::from_bytes(&bytes)
        .map_err(|_| Box::new(fail(StatusCode::BAD_REQUEST, "bad_request")))
}

fn label(state: &str) -> &'static str {
    if state == TURN_CLAIMED {
        "agent is thinking"
    } else {
        "waiting for an agent"
    }
}

fn msg_item(m: Message) -> MsgItem {
    // The role is a class name on the page, so it is one of three words this
    // module chooses, never the stored string itself.
    let (role, who) = match m.role.as_str() {
        "user" => ("user", "you"),
        "agent" => ("agent", "agent"),
        _ => ("system", "system"),
    };
    MsgItem {
        seq: m.seq,
        role,
        who,
        body: m.body,
    }
}

async fn conversations_frag(
    s: &AppState,
    cred: &Credential,
    active: Option<Uuid>,
    oob: bool,
) -> Result<String, Box<Response>> {
    let convs = s
        .chat()
        .conversations(cred, CONVERSATION_LIMIT)
        .await
        .map_err(|e| Box::new(chat_error(e, "list conversations")))?;
    let items = convs
        .into_iter()
        .map(|c| {
            let mut meta = c.runtime.clone();
            if !c.model.is_empty() {
                meta.push_str(" · ");
                meta.push_str(&c.model);
            }
            meta.push_str(" · ");
            meta.push_str(&c.updated_at.format("%Y-%m-%d %H:%M").to_string());
            ConvItem {
                id: c.id,
                title: if c.title.is_empty() {
                    "(untitled)".into()
                } else {
                    c.title
                },
                meta,
                active: active == Some(c.id),
            }
        })
        .collect();
    rendered(&ConversationsFrag { items, oob })
}

async fn thread_frag(
    s: &AppState,
    cred: &Credential,
    id: Uuid,
    oob: bool,
) -> Result<String, Box<Response>> {
    let msgs = s
        .chat()
        .messages(cred, id, 0, MESSAGE_LIMIT)
        .await
        .map_err(|e| Box::new(chat_error(e, "messages")))?;
    let open = s
        .chat()
        .open_turns(cred, id)
        .await
        .map_err(|e| Box::new(chat_error(e, "open turns")))?;
    let messages = rendered(&MessagesFrag {
        items: msgs.into_iter().map(msg_item).collect(),
        live: open
            .iter()
            .map(|t| LiveItem {
                request_seq: t.request_seq,
                label: label(&t.state),
            })
            .collect(),
    })?;
    let composer = rendered(&ComposerFrag { id, oob: false })?;
    rendered(&ThreadFrag {
        id,
        messages,
        composer,
        oob,
    })
}

/// `GET /`: the app for a browser that holds a session, the sign-in card for
/// one that does not. Both are one page; nothing about the shell is fetched
/// after the fact.
pub(crate) async fn index(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let cred = match &s.auth {
        Some(a) => a.resolve(&headers, None).await.ok(),
        None => None,
    };
    let Some(cred) = cred else {
        return render(
            StatusCode::OK,
            &LoginPage {
                identity: String::new(),
                signed_in: false,
            },
        );
    };
    let actor = match hive_store::actor_by_id(s.store().pool(), cred.actor_id).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(err = %e, actor = %cred.actor_id, "index actor read");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
    };
    let identity = format!(
        "{} · {} · v{}",
        actor.display_name,
        cred.principal_kind.as_str(),
        s.version
    );
    let conversations = match conversations_frag(&s, &cred, None, false).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    render(
        StatusCode::OK,
        &AppPage {
            identity,
            signed_in: true,
            conversations,
        },
    )
}

/// `GET /ui/conversations`: the sidebar list.
pub(crate) async fn list(State(s): State<AppState>, Authed(cred): Authed) -> Response {
    match conversations_frag(&s, &cred, None, false).await {
        Ok(body) => html(StatusCode::OK, body),
        Err(r) => *r,
    }
}

#[derive(Deserialize, Default)]
struct CreateForm {
    #[serde(default)]
    runtime: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    title: String,
}

/// `POST /ui/conversations`: creates one and answers with the list, the new
/// thread swapped in out of band, so one round trip does what the person
/// meant.
pub(crate) async fn create(
    State(s): State<AppState>,
    Authed(cred): Authed,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Err(r) = require_htmx(&headers) {
        return *r;
    }
    let f: CreateForm = match form(body).await {
        Ok(f) => f,
        Err(r) => return *r,
    };
    let (runtime, model, title) = (f.runtime.trim(), f.model.trim(), f.title.trim());
    if hive_harness::Runtime::parse(runtime).is_none() {
        return fail(StatusCode::BAD_REQUEST, "unknown runtime");
    }
    if title.len() > MAX_TITLE_LENGTH || model.len() > MAX_MODEL_LENGTH {
        return fail(StatusCode::BAD_REQUEST, "invalid");
    }
    let c = match s
        .chat()
        .create_conversation(&cred, runtime, model, title)
        .await
    {
        Ok(c) => c,
        Err(e) => return chat_error(e, "create conversation"),
    };
    let list = match conversations_frag(&s, &cred, Some(c.id), false).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let thread = match thread_frag(&s, &cred, c.id, true).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    html(StatusCode::OK, list + &thread)
}

/// `GET /ui/conversations/{id}`: the thread, with the list re-rendered out of
/// band so the active row moves.
pub(crate) async fn open(
    State(s): State<AppState>,
    Authed(cred): Authed,
    Path(id): Path<String>,
) -> Response {
    let id = match conversation_id(&id) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let thread = match thread_frag(&s, &cred, id, false).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let list = match conversations_frag(&s, &cred, Some(id), true).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    html(StatusCode::OK, thread + &list)
}

#[derive(Deserialize, Default)]
pub(crate) struct AfterQuery {
    #[serde(default)]
    after: Option<String>,
}

/// `GET /ui/conversations/{id}/messages?after=N`: the bubbles after a
/// sequence number, which is how the stream script catches up when a turn
/// closes.
pub(crate) async fn messages(
    State(s): State<AppState>,
    Authed(cred): Authed,
    Path(id): Path<String>,
    Query(q): Query<AfterQuery>,
) -> Response {
    let id = match conversation_id(&id) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let after: i32 = q.after.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0);
    let msgs = match s.chat().messages(&cred, id, after, MESSAGE_LIMIT).await {
        Ok(m) => m,
        Err(e) => return chat_error(e, "messages"),
    };
    render(
        StatusCode::OK,
        &MessagesFrag {
            items: msgs.into_iter().map(msg_item).collect(),
            live: Vec::new(),
        },
    )
}

#[derive(Deserialize, Default)]
struct MessageForm {
    #[serde(default)]
    body: String,
}

/// `POST /ui/conversations/{id}/messages`: the person's bubble and the turn
/// that answers it, plus a fresh composer out of band. The role and the trust
/// are fixed here, as on the JSON route: a form does not get to say it is the
/// agent.
pub(crate) async fn post_message(
    State(s): State<AppState>,
    Authed(cred): Authed,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Err(r) = require_htmx(&headers) {
        return *r;
    }
    let id = match conversation_id(&id) {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let f: MessageForm = match form(body).await {
        Ok(f) => f,
        Err(r) => return *r,
    };
    if f.body.trim().is_empty() || f.body.len() > MAX_MESSAGE_LENGTH {
        return fail(StatusCode::BAD_REQUEST, "invalid");
    }
    let (msg, turn) = match s
        .chat()
        .post_message(&cred, id, "user", &f.body, Level::Trusted, None)
        .await
    {
        Ok(x) => x,
        Err(e) => return chat_error(e, "post message"),
    };
    if let Some(wake) = &s.wake {
        wake();
    }
    let bubbles = match rendered(&MessagesFrag {
        items: vec![msg_item(msg)],
        live: turn
            .map(|t| LiveItem {
                request_seq: t.request_seq,
                label: "waiting for an agent",
            })
            .into_iter()
            .collect(),
    }) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let composer = match rendered(&ComposerFrag { id, oob: true }) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    html(StatusCode::OK, bubbles + &composer)
}

/// `POST /ui/logout`: clears the cookie and tells htmx to reload, which lands
/// on the sign-in card because the cookie is gone.
pub(crate) async fn logout(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_htmx(&headers) {
        return *r;
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    let h = r.headers_mut();
    h.insert(header::SET_COOKIE, cookie("", true, s.plain_http));
    h.insert(
        http::HeaderName::from_static("hx-refresh"),
        HeaderValue::from_static("true"),
    );
    r
}

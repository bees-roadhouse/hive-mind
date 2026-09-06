//! The server-rendered client: what a browser gets at `/` with and without a
//! session, the fragments htmx swaps, the header a cross-site form cannot
//! send, and the property that a message body is text and never markup.
//!
//! The Playwright suite drives the same pages in a real browser; these assert
//! the HTTP mapping and the escaping without one.

mod common;

use common::{Api, Setup, do_req, text};
use hive_httpauth::SESSION_COOKIE;

/// A request carrying the session cookie the way a browser would.
async fn as_browser(
    method: &str,
    url: &str,
    token: &str,
    htmx: bool,
    form: Option<&str>,
) -> (u16, String, reqwest::header::HeaderMap) {
    let client = reqwest::Client::new();
    let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let mut req = client
        .request(m, url)
        .header("Cookie", format!("{SESSION_COOKIE}={token}"));
    if htmx {
        req = req.header("HX-Request", "true");
    }
    if let Some(f) = form {
        req = req
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(f.to_string());
    }
    let res = req
        .send()
        .await
        .unwrap_or_else(|e| panic!("{method} {url}: {e}"));
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = res.text().await.unwrap();
    (status, body, headers)
}

fn header<'a>(h: &'a reqwest::header::HeaderMap, name: &str) -> &'a str {
    h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// Every `<script` on the page loads a file; none is inline.
fn scripts_are_external(page: &str) {
    for (i, _) in page.match_indices("<script") {
        let tag = &page[i..page[i..].find('>').map(|j| i + j).unwrap_or(page.len())];
        assert!(
            tag.contains(" src=\"/assets/"),
            "inline or foreign script: {tag}"
        );
    }
    for forbidden in [
        "<style",
        " onclick=",
        " onload=",
        "javascript:",
        "style=\"",
        "hx-on",
    ] {
        assert!(!page.contains(forbidden), "page contains {forbidden:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_root_is_the_sign_in_card_without_a_session_and_the_app_with_one() {
    let Some(a) = Api::with(
        "ui_root",
        Setup {
            chat: true,
            plain_http: true,
            ..Default::default()
        },
    )
    .await
    else {
        return;
    };

    let (status, page, h) = as_browser("GET", &format!("{}/", a.url), "", false, None).await;
    assert_eq!(status, 200);
    assert!(header(&h, "content-type").starts_with("text/html"));
    assert!(header(&h, "content-security-policy").contains("default-src 'none'"));
    assert!(header(&h, "content-security-policy").contains("script-src 'self'"));
    assert_eq!(header(&h, "x-content-type-options"), "nosniff");
    assert!(page.contains("id=\"login\""), "{page}");
    assert!(
        !page.contains("id=\"app\""),
        "the app must not render for a stranger"
    );
    assert!(page.contains("src=\"/assets/login.js\""));
    scripts_are_external(&page);

    // A wrong cookie is a stranger too.
    let (status, page, _) =
        as_browser("GET", &format!("{}/", a.url), "not-a-token", false, None).await;
    assert_eq!(status, 200);
    assert!(page.contains("id=\"login\""));

    let (status, page, _) =
        as_browser("GET", &format!("{}/", a.url), &a.root_token, false, None).await;
    assert_eq!(status, 200);
    assert!(page.contains("id=\"app\""), "{page}");
    assert!(!page.contains("id=\"login\""));
    assert!(page.contains("id=\"identity\""));
    assert!(page.contains("Root · user · vtest-v1"), "{page}");
    assert!(page.contains("id=\"conversations\""));
    assert!(page.contains("id=\"new-conversation\""));
    assert!(page.contains("id=\"logout\""));
    assert!(page.contains("src=\"/assets/stream.js\""));
    assert!(page.contains("src=\"/assets/htmx.min.js\""));
    scripts_are_external(&page);
    a.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fragments_compose_a_thread_and_escape_everything() {
    let Some(a) = Api::with(
        "ui_fragments",
        Setup {
            chat: true,
            plain_http: true,
            ..Default::default()
        },
    )
    .await
    else {
        return;
    };
    let t = &a.root_token;

    // Create: the list comes back with the new row active, and the thread
    // rides along out of band.
    let (status, body, _) = as_browser(
        "POST",
        &format!("{}/ui/conversations", a.url),
        t,
        true,
        Some("runtime=claude&model=&title=first+%3Cb%3Ethread%3C%2Fb%3E"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("id=\"conversations\""), "{body}");
    assert!(body.contains("class=\"active\""), "{body}");
    assert!(
        body.contains("first &#60;b&#62;thread&#60;/b&#62;"),
        "title was not escaped: {body}"
    );
    assert!(!body.contains("<b>thread</b>"));
    assert!(
        body.contains("id=\"thread\" class=\"thread\" hx-swap-oob=\"true\""),
        "{body}"
    );
    assert!(body.contains("id=\"composer\""), "{body}");
    assert!(body.contains("id=\"body\""), "{body}");
    let id = body
        .split("data-id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("a conversation id on the row")
        .to_string();

    // Post a message: the person's bubble, a waiting turn, and a fresh composer.
    let (status, body, _) = as_browser(
        "POST",
        &format!("{}/ui/conversations/{id}/messages", a.url),
        t,
        true,
        Some("body=hello+%3Cscript%3Ealert(1)%3C%2Fscript%3E"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("class=\"msg user\""), "{body}");
    assert!(
        body.contains("hello &#60;script&#62;alert(1)&#60;/script&#62;"),
        "body was not escaped: {body}"
    );
    assert!(!body.contains("<script>alert"));
    assert!(body.contains("class=\"msg agent live\""), "{body}");
    assert!(body.contains("waiting for an agent"), "{body}");
    assert!(
        body.contains("id=\"composer\" class=\"composer\""),
        "{body}"
    );
    assert!(body.contains("hx-swap-oob=\"true\""), "{body}");
    assert!(body.contains("hx-post=\"/ui/conversations/") && body.contains("/messages\""));
    assert!(body.contains("data-seq=\"1\""), "{body}");

    // Open: the thread again, from the store, with the open turn still there,
    // and the list re-rendered out of band with the row active.
    let (status, body, _) = as_browser(
        "GET",
        &format!("{}/ui/conversations/{id}", a.url),
        t,
        false,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(&format!("data-conversation=\"{id}\"")),
        "{body}"
    );
    assert!(body.contains("hello &#60;script&#62;"));
    assert!(body.contains("waiting for an agent"));
    assert!(
        body.contains("id=\"conversations\" class=\"list\" hx-swap-oob=\"true\""),
        "{body}"
    );
    assert!(body.contains("class=\"active\""));

    // Catch-up after a sequence number: nothing after 1, the message after 0.
    let (status, body, _) = as_browser(
        "GET",
        &format!("{}/ui/conversations/{id}/messages?after=1", a.url),
        t,
        false,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(!body.contains("class=\"msg"), "{body}");
    let (_, body, _) = as_browser(
        "GET",
        &format!("{}/ui/conversations/{id}/messages?after=0", a.url),
        t,
        false,
        None,
    )
    .await;
    assert!(body.contains("class=\"msg user\""));

    // A stranger's conversation is not found, exactly as on the JSON route.
    let (status, _, _) = as_browser(
        "GET",
        &format!("{}/ui/conversations/{}", a.url, uuid::Uuid::new_v4()),
        t,
        false,
        None,
    )
    .await;
    assert_eq!(status, 404);
    a.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutations_need_the_htmx_header_and_a_session() {
    let Some(a) = Api::with(
        "ui_guards",
        Setup {
            chat: true,
            plain_http: true,
            ..Default::default()
        },
    )
    .await
    else {
        return;
    };
    let t = &a.root_token;

    // A plain form post, the shape a cross-site page can produce, is refused.
    let (status, body, _) = as_browser(
        "POST",
        &format!("{}/ui/conversations", a.url),
        t,
        false,
        Some("runtime=claude"),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body.trim(), r#"{"error":"bad_request"}"#);

    // No session: the one 401, same as every other route.
    let (s1, b1, _) = do_req(
        "POST",
        &format!("{}/ui/conversations", a.url),
        "",
        None,
        false,
    )
    .await;
    let (s2, b2, _) = do_req("GET", &format!("{}/whoami", a.url), "", None, false).await;
    assert_eq!((s1, s2), (401, 401));
    assert_eq!(text(&b1), text(&b2));

    // An unknown runtime and an over-long title are the caller's mistakes.
    let (status, _, _) = as_browser(
        "POST",
        &format!("{}/ui/conversations", a.url),
        t,
        true,
        Some("runtime=nope"),
    )
    .await;
    assert_eq!(status, 400);
    let long = format!("runtime=claude&title={}", "x".repeat(201));
    let (status, _, _) = as_browser(
        "POST",
        &format!("{}/ui/conversations", a.url),
        t,
        true,
        Some(&long),
    )
    .await;
    assert_eq!(status, 400);

    // Logout clears the cookie and asks htmx to reload.
    let (status, _, h) = as_browser("POST", &format!("{}/ui/logout", a.url), t, true, None).await;
    assert_eq!(status, 204);
    assert_eq!(header(&h, "hx-refresh"), "true");
    let cookie = header(&h, "set-cookie");
    assert!(
        cookie.contains(&format!("{SESSION_COOKIE}=;")) && cookie.contains("Max-Age=0"),
        "{cookie}"
    );
    let (status, _, _) = as_browser("POST", &format!("{}/ui/logout", a.url), t, false, None).await;
    assert_eq!(status, 400, "logout without the header");
    a.stop().await;
}

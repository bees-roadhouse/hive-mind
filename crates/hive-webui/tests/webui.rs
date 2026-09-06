//! The asset server: the right bytes, under the policy, and nothing an API
//! route owns. The pages are rendered by hive-httpapi and tested there.

use axum::Router;
use axum::routing::get;
use http::StatusCode;
use tokio_util::sync::CancellationToken;

async fn serve(app: Router) -> (String, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { c.cancelled().await })
            .await;
    });
    (format!("http://{addr}"), cancel)
}

#[tokio::test]
async fn assets_are_served_under_policy_and_shadow_nothing() {
    let app = hive_webui::router()
        .route("/conversations", get(|| async { StatusCode::IM_A_TEAPOT }))
        .route("/", get(|| async { StatusCode::IM_A_TEAPOT }));
    let (url, cancel) = serve(app).await;
    let client = reqwest::Client::new();
    for (path, status, content_type, contains) in [
        ("/assets/htmx.min.js", 200, "text/javascript", "htmx"),
        ("/assets/login.js", 200, "text/javascript", "/session"),
        ("/assets/stream.js", 200, "text/javascript", "EventSource"),
        ("/assets/styles.css", 200, "text/css", "body"),
        ("/assets/nope.js", 404, "", ""),
        ("/assets/styles.css/../Cargo.toml", 404, "", ""),
        ("/conversations", 418, "", ""),
        ("/", 418, "", ""),
    ] {
        let res = client.get(format!("{url}{path}")).send().await.unwrap();
        assert_eq!(res.status().as_u16(), status, "{path}");
        if status != 200 {
            continue;
        }
        let ct = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.starts_with(content_type),
            "{path}: content type {ct:?}, want {content_type}"
        );
        let csp = res
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            csp.contains("default-src 'none'") && csp.contains("script-src 'self'"),
            "{path}: policy = {csp:?}"
        );
        assert_eq!(
            res.headers()
                .get("x-content-type-options")
                .and_then(|v| v.to_str().ok()),
            Some("nosniff"),
            "{path}: no nosniff"
        );
        let body = res.text().await.unwrap();
        assert!(body.contains(contains), "{path}: body lacks {contains:?}");
    }
    cancel.cancel();
}

/// The scripts never put a token in a URL or in script-readable storage: the
/// sign-in script exchanges it once for the cookie. Asserted against the
/// embedded bytes, so a change that starts remembering a credential fails
/// here.
#[test]
fn scripts_keep_the_credential_out_of_storage() {
    for name in ["login.js", "stream.js"] {
        let js = String::from_utf8(hive_webui::asset_bytes(name).expect("embedded")).unwrap();
        for forbidden in ["localStorage", "sessionStorage", "access_token=", "eval("] {
            assert!(!js.contains(forbidden), "{name} contains {forbidden:?}");
        }
    }
    let login = String::from_utf8(hive_webui::asset_bytes("login.js").unwrap()).unwrap();
    assert!(
        login.contains("'/session'") && login.contains("Authorization"),
        "login.js does not exchange the token over the header"
    );
}

/// htmx is pinned by content, not by a CDN: the page's policy allows scripts
/// from 'self' only, and a vendored copy is what makes that true.
#[test]
fn htmx_is_vendored_and_pinned() {
    let js = String::from_utf8(hive_webui::asset_bytes("htmx.min.js").unwrap()).unwrap();
    assert!(
        js.contains("version:\"2.0.6\""),
        "htmx version changed; update this test and the doc"
    );
}

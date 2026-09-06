//! MCP is reachable over the unix socket, not only the port (invariant 13). A
//! harness container runs `--network=none` with the socket bind-mounted, so a
//! tool call from inside a run has exactly this way in.
//!
//! Needs a database, because the endpoint authenticates like every other; the
//! MCP server behind it is a fake with no tools, since what is under test is
//! the transport.

use std::sync::Arc;

use async_trait::async_trait;
use hive_identity::Credential;
use hive_mcp::{CrudCall, Dispatcher, Guard, GuestCall, Install, Installs, Server, ToolResult};
use hive_sandbox::unix_listener;
use hive_store::{BootstrapConfig, Store};
use hive_testdb::TestDb;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

struct Nothing;

#[async_trait]
impl Installs for Nothing {
    async fn active_installs(&self, _: &Credential) -> Result<Vec<Install>, String> {
        Ok(Vec::new())
    }
}
#[async_trait]
impl Guard for Nothing {
    async fn tool_reason(&self, _: &Credential, _: Uuid, _: &str) -> Result<bool, String> {
        Ok(false)
    }
}
#[async_trait]
impl Dispatcher for Nothing {
    async fn call_guest(&self, _: GuestCall) -> Result<ToolResult, String> {
        Err("nothing".into())
    }
    async fn call_crud(&self, _: CrudCall) -> Result<ToolResult, String> {
        Err("nothing".into())
    }
}

async fn post_over_socket(
    path: &std::path::Path,
    target: &str,
    token: &str,
    body: &str,
) -> (u16, String) {
    let mut s = tokio::net::UnixStream::connect(path)
        .await
        .expect("dial socket");
    let req = format!(
        "POST {target} HTTP/1.1\r\nHost: unix\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), s.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_answers_over_the_unix_socket() {
    let Some(db) = TestDb::new("mcp_over_socket").await else {
        return;
    };
    hive_store::migrate(db.pool()).await.expect("migrate");
    let store = Store::from_pool(db.pool().clone());
    let res = store
        .bootstrap_in_tx(&BootstrapConfig {
            root_handle: "root".into(),
            root_name: "Root".into(),
            ..Default::default()
        })
        .await
        .expect("bootstrap");
    let token = format!("root-token-{}", Uuid::new_v4());
    {
        let mut conn = store.conn().await.unwrap();
        hive_store::ensure_bootstrap_credential(&mut conn, res.root_actor_id, &token)
            .await
            .expect("bootstrap credential");
    }
    let mcp = Arc::new(Server::new(
        Arc::new(Nothing),
        Arc::new(Nothing),
        Arc::new(Nothing),
    ));
    let app = hive_httpapi::router(
        Some(store),
        None,
        hive_httpapi::Options {
            version: "sock-test".into(),
            mcp: Some(mcp),
            ..Default::default()
        },
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("api.sock");
    let mut sock = unix_listener(&path).await.expect("unix_listener");
    let listener = sock.take().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let (status, body) = post_over_socket(
        &path,
        "/mcp",
        &token,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["result"]["serverInfo"]["name"], "hive-sandbox");
    assert_eq!(v["result"]["serverInfo"]["version"], "sock-test");

    let (status, body) = post_over_socket(
        &path,
        "/mcp",
        &token,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["tools"], serde_json::json!([]));

    // And without a credential, the socket is no back door: the one 401.
    let (status, _) = post_over_socket(&path, "/mcp", "", "{}").await;
    assert_eq!(status, 401);
}

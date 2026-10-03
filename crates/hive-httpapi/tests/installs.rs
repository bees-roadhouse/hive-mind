//! `POST /blobs` and `POST /apps`: the two routes a person needs to put a
//! file and an app into the platform. Written before the handlers.

mod common;

use common::{Api, Setup, decode, do_req, get, post_json, text};
use hive_manifest::{Collection, Function, Kind, Manifest, Storage, ToolDef, Use, UseAccess};
use hive_store::{attach_owner, owner_table};
use sha2::{Digest, Sha256};

const HELLO: &[u8] = include_bytes!("../../hive-wasmhost/testdata/hello.wasm");

fn hosted() -> Setup {
    Setup {
        host: true,
        ..Default::default()
    }
}

async fn upload(a: &Api, token: &str, mime: &str, bytes: &[u8]) -> (u16, serde_json::Value) {
    let client = reqwest::Client::new();
    let res = client
        .post(format!("{}/blobs", a.url))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", mime)
        .body(bytes.to_vec())
        .send()
        .await
        .expect("post /blobs");
    let status = res.status().as_u16();
    let body = res.bytes().await.unwrap();
    let v = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, v)
}

fn host_only_app(name: &str) -> Manifest {
    Manifest {
        kind: Some(Kind::App),
        name: name.into(),
        version: 1,
        storage: Storage {
            collections: vec![Collection {
                name: "items".into(),
                crud: true,
                indexes: vec![],
            }],
            uses: vec![],
        },
        ..Default::default()
    }
}

/// Bytes go in under the caller's reference and come back to the caller
/// only: a stranger who learns the hash gets the same 404 as for nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_then_read_round_trip() {
    let a = Api::with("installs_upload", hosted()).await;
    let bytes = b"a scanned page, allegedly";
    let (status, v) = upload(&a, &a.root_token, "text/plain", bytes).await;
    assert_eq!(status, 201, "{v}");
    let want = hex::encode(Sha256::digest(bytes));
    assert_eq!(v["hash"], want);
    assert_eq!(v["size"], bytes.len());
    assert_eq!(v["mime"], "text/plain");

    let (status, body, headers) = do_req(
        "GET",
        &format!("{}/blobs/{want}", a.url),
        &a.root_token,
        None,
        false,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, bytes);
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("text/plain")
    );

    let (_, bob_token) = a.human("bob").await;
    let (status, _) = get(&format!("{}/blobs/{want}", a.url), &bob_token).await;
    assert_eq!(status, 404, "a stranger read bytes by their hash");

    // Unauthenticated is the one 401.
    let (status, _) = upload(&a, "", "text/plain", bytes).await;
    assert_eq!(status, 401);
    a.stop().await;
}

/// An app with no guest code installs from its manifest alone: a row, an
/// active state, and its collection table in the owner's file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_a_host_only_app() {
    let a = Api::with("installs_host_only", hosted()).await;
    let (status, body) = post_json(
        &format!("{}/apps", a.url),
        &a.root_token,
        &serde_json::json!({ "manifest": host_only_app("notes") }),
    )
    .await;
    assert_eq!(status, 201, "{}", text(&body));
    let v = decode(&body);
    assert_eq!(v["app"], "notes");
    assert_eq!(v["state"], "active");
    let schema = v["schema"].as_str().unwrap().to_string();

    let (status, body) = get(&format!("{}/apps", a.url), &a.root_token).await;
    assert_eq!(status, 200);
    let list = decode(&body);
    let mine: Vec<&str> = list["installs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["app"].as_str().unwrap())
        .collect();
    assert!(mine.contains(&"notes"), "{list}");
    assert!(
        mine.contains(&"core"),
        "the core install is missing from the listing: {list}"
    );

    let conn = a.store.conn().await.unwrap();
    let alias = attach_owner(&conn, hive_identity::Owner::user(a.root))
        .await
        .unwrap();
    let n: i64 = hive_db::query(&format!(
        "SELECT count(*) FROM {}",
        owner_table(&alias, &format!("{schema}__items"))
    ))
    .fetch_scalar(&conn)
    .await
    .expect("the collection table was not provisioned");
    assert_eq!(n, 0);
    a.stop().await;
}

/// The reference guest: upload the module, install a manifest that names
/// its functions, and the build records the module it will run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_the_reference_guest() {
    let a = Api::with("installs_guest", hosted()).await;
    let (status, v) = upload(&a, &a.root_token, "application/wasm", HELLO).await;
    assert_eq!(status, 201, "{v}");
    let hash = v["hash"].as_str().unwrap().to_string();

    let m = Manifest {
        kind: Some(Kind::App),
        name: "hello".into(),
        version: 1,
        storage: Storage {
            collections: vec![Collection {
                name: "entries".into(),
                ..Default::default()
            }],
            uses: vec![],
        },
        functions: vec![
            Function {
                name: "hello".into(),
                ..Default::default()
            },
            Function {
                name: "store_query".into(),
                ..Default::default()
            },
        ],
        tools: vec![ToolDef {
            name: "hello".into(),
            function: "hello".into(),
            description: "Greets by name.".into(),
            ..Default::default()
        }],
        capabilities: vec!["log".into(), "storage".into()],
        ..Default::default()
    };
    let (status, body) = post_json(
        &format!("{}/apps", a.url),
        &a.root_token,
        &serde_json::json!({ "manifest": m, "module": hash }),
    )
    .await;
    assert_eq!(status, 201, "{}", text(&body));
    let v = decode(&body);
    let build = v["build"].as_str().unwrap();
    let recorded: Option<String> =
        hive_db::query("SELECT module_sha256 FROM app_builds WHERE id = ?1")
            .bind(uuid::Uuid::parse_str(build).unwrap())
            .fetch_scalar(&a.store.conn().await.unwrap())
            .await
            .unwrap();
    assert_eq!(recorded.as_deref(), Some(hash.as_str()));
    a.stop().await;
}

/// A manifest that claims a function the module does not export is
/// refused, and nothing is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manifest_its_module_does_not_satisfy_is_refused() {
    let a = Api::with("installs_bad_claim", hosted()).await;
    let (_, v) = upload(&a, &a.root_token, "application/wasm", HELLO).await;
    let hash = v["hash"].as_str().unwrap().to_string();
    let m = Manifest {
        kind: Some(Kind::App),
        name: "liar".into(),
        version: 1,
        functions: vec![Function {
            name: "not_exported".into(),
            ..Default::default()
        }],
        // The capabilities the module really imports, so it LOADS and the
        // refusal under test is the registry's claim check, not the linker's
        // ("check which refusal").
        capabilities: vec!["log".into(), "storage".into()],
        ..Default::default()
    };
    let (status, body) = post_json(
        &format!("{}/apps", a.url),
        &a.root_token,
        &serde_json::json!({ "manifest": m, "module": hash }),
    )
    .await;
    assert_eq!(status, 422, "{}", text(&body));
    assert_eq!(decode(&body)["error"], "invalid_manifest");
    let n: i64 = hive_db::query("SELECT count(*) FROM installs WHERE slug = 'liar'")
        .fetch_scalar(&a.store.conn().await.unwrap())
        .await
        .unwrap();
    assert_eq!(n, 0);
    a.stop().await;
}

/// Naming a digest is not holding the bytes: a module somebody else
/// uploaded is "not found" to the installer, and nothing is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_module_the_caller_does_not_hold_is_not_found() {
    let a = Api::with("installs_unheld_module", hosted()).await;
    let (_, bob_token) = a.human("bob").await;
    let (status, v) = upload(&a, &bob_token, "application/wasm", HELLO).await;
    assert_eq!(status, 201, "{v}");
    let hash = v["hash"].as_str().unwrap().to_string();
    let (status, body) = post_json(
        &format!("{}/apps", a.url),
        &a.root_token,
        &serde_json::json!({ "manifest": host_only_app("sneaky"), "module": hash }),
    )
    .await;
    assert_eq!(status, 404, "{}", text(&body));
    let n: i64 = hive_db::query("SELECT count(*) FROM app_builds WHERE slug = 'sneaky'")
        .fetch_scalar(&a.store.conn().await.unwrap())
        .await
        .unwrap();
    assert_eq!(n, 0, "a refused install left a build behind");
    a.stop().await;
}

/// Installing is a person's act (D19.4): an AI acting for the same principal
/// is refused with the generic forbidden.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ai_cannot_install() {
    let a = Api::with("installs_ai", hosted()).await;
    let (_, ai_token) = a.ai("ava", "assistant", a.root).await;
    let (status, body) = post_json(
        &format!("{}/apps", a.url),
        &ai_token,
        &serde_json::json!({ "manifest": host_only_app("agentware") }),
    )
    .await;
    assert_eq!(status, 403, "{}", text(&body));
    let n: i64 = hive_db::query("SELECT count(*) FROM installs WHERE slug = 'agentware'")
        .fetch_scalar(&a.store.conn().await.unwrap())
        .await
        .unwrap();
    assert_eq!(n, 0);
    a.stop().await;
}

/// A manifest that uses an app the owner has not installed is refused as a
/// conflict, and the install is not left half-made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_use_of_an_app_the_owner_lacks_is_a_conflict() {
    let a = Api::with("installs_unmet_uses", hosted()).await;
    let mut m = host_only_app("mail");
    m.storage.uses = vec![Use {
        app: "calendar".into(),
        collection: "events".into(),
        access: UseAccess::Read,
    }];
    let (status, body) = post_json(
        &format!("{}/apps", a.url),
        &a.root_token,
        &serde_json::json!({ "manifest": m }),
    )
    .await;
    assert_eq!(status, 409, "{}", text(&body));
    assert_eq!(decode(&body)["error"], "unmet_uses");
    let n: i64 = hive_db::query("SELECT count(*) FROM installs WHERE slug = 'mail'")
        .fetch_scalar(&a.store.conn().await.unwrap())
        .await
        .unwrap();
    assert_eq!(n, 0, "a refused activation left the install staged");

    // And one that uses the core, which every owner has, goes through.
    let mut m = host_only_app("addressbook");
    m.storage.uses = vec![Use {
        app: "core".into(),
        collection: "contacts".into(),
        access: UseAccess::Write,
    }];
    let (status, body) = post_json(
        &format!("{}/apps", a.url),
        &a.root_token,
        &serde_json::json!({ "manifest": m }),
    )
    .await;
    assert_eq!(status, 201, "{}", text(&body));
    a.stop().await;
}

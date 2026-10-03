//! Full text and vectors in the engine (D41): `fts(path)` and
//! `vector(path, dim)` provision virtual tables in the owner's file, and
//! `storage.query` asks them with `search` and `near`. Written before the
//! code.

mod common;

use std::sync::Arc;

use common::{World, cred, user};
use hive_db::query;
use hive_identity::{Credential, PrincipalKind};
use hive_manifest::{Collection, Kind, Manifest, Storage};
use hive_store::{
    AppData, BuildSpec, InstallSpec, activate_install, attach_owner, drop_schema_plan,
    register_build, stage_install,
};
use hive_trust::Level;
use hive_wasmhost::{Caller, Request, Status, Storage as _};
use uuid::Uuid;

struct Fx {
    w: World,
    data: AppData,
    install: Uuid,
    schema: String,
    plan: hive_manifest::SchemaPlan,
    alice: Uuid,
    _dir: tempfile::TempDir,
}

/// An install of an app whose `entries` carry a full-text index on `body`
/// and a four-dimensional vector index on `embedding`.
async fn fixture(test: &str) -> Fx {
    let w = World::new(test).await;
    let alice = w.human("alice").await;
    let by = cred(alice, PrincipalKind::User, alice);
    let m = Manifest {
        kind: Some(Kind::App),
        name: "notes".into(),
        version: 1,
        storage: Storage {
            collections: vec![Collection {
                name: "entries".into(),
                crud: true,
                indexes: vec![
                    "btree(created)".into(),
                    "fts(body)".into(),
                    "vector(embedding, 4)".into(),
                ],
            }],
            uses: vec![],
        },
        ..Default::default()
    };
    let p = hive_registry::prepare(&m, &hive_wasmhost::Exports::none()).expect("prepare");
    let spec = p.install_spec("user", &alice.to_string()).expect("spec");
    let plan = spec.schema.clone();
    let tx = w.store.begin().await.unwrap();
    let reg = register_build(
        &tx,
        &BuildSpec {
            spec,
            owner: Some(user(alice)),
            trust: "builtin".into(),
        },
        &by,
    )
    .await
    .expect("register");
    tx.commit().await.unwrap();
    let conn = w.conn().await;
    let install = stage_install(
        &conn,
        &InstallSpec {
            build_id: reg.build_id,
            slug: "notes".into(),
            owner: user(alice),
        },
        &by,
    )
    .await
    .expect("stage");
    activate_install(&conn, install, &by)
        .await
        .expect("activate");
    drop(conn);
    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(hive_blob::Catalog::new(
        w.db().clone(),
        Box::new(hive_blob::DiskDriver::new(dir.path()).await.unwrap()),
    ));
    let data = AppData::new(w.store.clone(), catalog);
    Fx {
        w,
        data,
        install,
        schema: reg.schema_name,
        plan,
        alice,
        _dir: dir,
    }
}

impl Fx {
    fn cred(&self) -> Credential {
        cred(self.alice, PrincipalKind::User, self.alice)
    }

    fn req(&self, body: serde_json::Value) -> Request {
        Request {
            caller: Caller::new(self.cred(), self.install),
            app: String::new(),
            body: serde_json::to_vec(&body).unwrap(),
            trust: Level::Trusted,
            tainted_by: String::new(),
        }
    }

    async fn insert(&self, doc: serde_json::Value) -> Result<Uuid, hive_wasmhost::HostError> {
        let res = self
            .data
            .insert(self.req(serde_json::json!({"collection": "entries", "doc": doc})))
            .await?;
        let v: serde_json::Value = serde_json::from_slice(&res.data).unwrap();
        Ok(v["id"].as_str().unwrap().parse().unwrap())
    }

    async fn query(&self, extra: serde_json::Value) -> serde_json::Value {
        let mut body = serde_json::json!({"collection": "entries"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let res = self.data.query(self.req(body)).await.expect("query");
        serde_json::from_slice(&res.data).unwrap()
    }

    /// Every object in the owner's file under this install's prefix.
    async fn objects(&self) -> Vec<String> {
        let conn = self.w.conn().await;
        let alias = attach_owner(&conn, user(self.alice)).await.unwrap();
        query(&format!(
            "SELECT name FROM {}.sqlite_master WHERE name GLOB ?1 ORDER BY name",
            hive_db::quote_ident(&alias)
        ))
        .bind(format!("{}__*", self.schema))
        .fetch_scalars(&conn)
        .await
        .unwrap()
    }
}

fn ids(v: &serde_json::Value) -> Vec<String> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

/// `fts(body)` provisions an FTS5 table and `search` asks it with words.
/// FTS5's own syntax is not exposed: `OR` is a word here, not an operator.
#[tokio::test]
async fn search_finds_documents_by_words() {
    let f = fixture("search_words").await;
    let fox = f
        .insert(serde_json::json!({"body": "the quick brown fox", "created": 1}))
        .await
        .unwrap();
    let dog = f
        .insert(serde_json::json!({"body": "a lazy dog", "created": 2}))
        .await
        .unwrap();
    f.insert(serde_json::json!({"title": "no body at all", "created": 3}))
        .await
        .unwrap();

    let got = f.query(serde_json::json!({"search": "fox"})).await;
    assert_eq!(ids(&got), vec![fox.to_string()]);
    let got = f.query(serde_json::json!({"search": "quick fox"})).await;
    assert_eq!(ids(&got), vec![fox.to_string()], "two words are an AND");
    let got = f.query(serde_json::json!({"search": "dog"})).await;
    assert_eq!(ids(&got), vec![dog.to_string()]);
    let got = f.query(serde_json::json!({"search": "fox OR dog"})).await;
    assert!(
        ids(&got).is_empty(),
        "FTS5 syntax reached the engine: {got}"
    );
    let got = f
        .query(serde_json::json!({"search": "\"fox\" NOT dog) --"}))
        .await;
    assert!(ids(&got).is_empty(), "{got}");
    // And search composes with containment.
    let got = f
        .query(serde_json::json!({"search": "fox", "match": {"created": 2}}))
        .await;
    assert!(ids(&got).is_empty());
}

/// `vector(embedding, 4)` provisions a vec0 table and `near` ranks by
/// distance, `limit` being k; a document with no embedding is not in the
/// index and a `near` for a path with no vector index is invalid.
#[tokio::test]
async fn near_ranks_by_distance() {
    let f = fixture("near_ranks").await;
    let a = f
        .insert(serde_json::json!({"embedding": [1.0, 0.0, 0.0, 0.0], "n": "a"}))
        .await
        .unwrap();
    let b = f
        .insert(serde_json::json!({"embedding": [0.0, 1.0, 0.0, 0.0], "n": "b"}))
        .await
        .unwrap();
    let c = f
        .insert(serde_json::json!({"embedding": [0.0, 0.0, 1.0, 0.0], "n": "c"}))
        .await
        .unwrap();
    f.insert(serde_json::json!({"n": "no embedding"}))
        .await
        .unwrap();

    let got = f
        .query(serde_json::json!({
            "near": {"path": "embedding", "vector": [0.9, 0.1, 0.0, 0.0]},
            "limit": 2
        }))
        .await;
    assert_eq!(ids(&got), vec![a.to_string(), b.to_string()], "{got}");
    let d0 = got["rows"][0]["distance"].as_f64().unwrap();
    let d1 = got["rows"][1]["distance"].as_f64().unwrap();
    assert!(d0 < d1, "not ordered by distance: {got}");

    let got = f
        .query(serde_json::json!({
            "near": {"path": "embedding", "vector": [0.0, 0.0, 1.0, 0.0]},
            "limit": 10
        }))
        .await;
    assert_eq!(ids(&got)[0], c.to_string());
    assert_eq!(
        ids(&got).len(),
        3,
        "a document with no embedding was ranked"
    );

    // Updating the embedding moves the document.
    f.data
        .update(f.req(serde_json::json!({
            "collection": "entries", "id": a,
            "doc": {"embedding": [0.0, 0.0, 0.0, 1.0], "n": "a moved"}
        })))
        .await
        .expect("update");
    let got = f
        .query(serde_json::json!({
            "near": {"path": "embedding", "vector": [1.0, 0.0, 0.0, 0.0]},
            "limit": 1
        }))
        .await;
    assert_ne!(
        ids(&got)[0],
        a.to_string(),
        "the moved document still ranks first"
    );

    let err = f
        .data
        .query(f.req(serde_json::json!({
            "collection": "entries",
            "near": {"path": "nothing_here", "vector": [1.0, 0.0, 0.0, 0.0]}
        })))
        .await
        .expect_err("a near on a path with no vector index was answered");
    assert_eq!(err.status(), Status::Invalid, "{err}");
}

/// The dimension in the manifest is the dimension of every vector: a
/// document carrying the wrong one is refused as the caller's mistake, not
/// as the host's failure, and nothing of it is written.
#[tokio::test]
async fn a_vector_of_the_wrong_dimension_is_invalid() {
    let f = fixture("near_wrong_dimension").await;
    let err = f
        .insert(serde_json::json!({"embedding": [1.0, 2.0, 3.0]}))
        .await
        .expect_err("a three-dimensional vector went into a four-dimensional index");
    assert_eq!(err.status(), Status::Invalid, "{err}");
    let got = f.query(serde_json::json!({})).await;
    assert!(
        ids(&got).is_empty(),
        "the refused document was written: {got}"
    );
}

/// Uninstall takes the virtual tables and their shadow tables with the
/// collection tables, by name, because dropping the base table alone leaves
/// every one of them behind (D41 measurement 6).
#[tokio::test]
async fn uninstall_drops_the_virtual_tables_too() {
    let f = fixture("search_uninstall").await;
    f.insert(serde_json::json!({"body": "x", "embedding": [1.0, 0.0, 0.0, 0.0]}))
        .await
        .unwrap();
    let before = f.objects().await;
    assert!(
        before.iter().any(|n| n.ends_with("_fts")) && before.iter().any(|n| n.ends_with("_vec")),
        "the virtual tables were not provisioned: {before:?}"
    );
    let tx = f.w.store.begin().await.unwrap();
    drop_schema_plan(&tx, user(f.alice), &f.plan)
        .await
        .expect("drop");
    tx.commit().await.unwrap();
    let after = f.objects().await;
    assert!(after.is_empty(), "left behind: {after:?}");
}

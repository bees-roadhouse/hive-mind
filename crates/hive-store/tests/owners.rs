//! Owner files (D39): where an owner's documents land, how the file is
//! reached, and what the file refuses. Written before the layout existed.

mod common;

use common::{World, cred, user};
use hive_db::query;
use hive_identity::{Owner, PrincipalKind};
use hive_manifest::{Collection, Kind, Manifest, Storage};
use hive_store::{BuildSpec, attach_owner, owner_file, owners_dir, register_build};
use uuid::Uuid;

fn manifest(name: &str) -> Manifest {
    Manifest {
        kind: Some(Kind::App),
        name: name.into(),
        version: 1,
        storage: Storage {
            collections: vec![Collection {
                name: "entries".into(),
                crud: true,
                indexes: vec!["btree(created)".into()],
            }],
            uses: vec![],
        },
        ..Default::default()
    }
}

async fn register(w: &World, name: &str, owner: Owner, actor: Uuid) -> String {
    let m = manifest(name);
    let p = hive_registry::prepare(&m, &hive_wasmhost::Exports::none()).expect("prepare");
    let spec = p
        .install_spec(owner.kind.as_str(), &owner.id.to_string())
        .expect("install_spec");
    let tx = w.store.begin().await.unwrap();
    let out = register_build(
        &tx,
        &BuildSpec {
            spec,
            owner: Some(owner),
            trust: "builtin".into(),
        },
        &cred(actor, PrincipalKind::User, actor),
    )
    .await
    .expect("register");
    tx.commit().await.unwrap();
    out.schema_name
}

/// The names of the tables under a prefix in the file attached as `alias`.
async fn tables_in(conn: &hive_db::Connection, alias: &str, prefix: &str) -> Vec<String> {
    query(&format!(
        "SELECT name FROM {}.sqlite_master WHERE type = 'table' AND substr(name, 1, length(?1)) = ?1 ORDER BY name",
        hive_db::quote_ident(alias)
    ))
    .bind(prefix)
    .fetch_scalars(conn)
    .await
    .unwrap()
}

/// The tables an install provisions land in its owner's file and nowhere
/// in the control plane. Two owners of one app are two files.
#[tokio::test]
async fn provisioned_tables_land_in_the_owners_file() {
    let w = World::new("owner_files_land").await;
    let alice = w.human("alice").await;
    let bob = w.human("bob").await;
    let schema_a = register(&w, "journal", user(alice), alice).await;
    let schema_b = register(&w, "journal", user(bob), bob).await;

    let dir = owners_dir(w.db().path().expect("the sqlite engine has a path"));
    assert!(owner_file(&dir, user(alice)).is_file(), "alice has no file");
    assert!(owner_file(&dir, user(bob)).is_file(), "bob has no file");

    let conn = w.conn().await;
    let in_main: Vec<String> =
        query("SELECT name FROM main.sqlite_master WHERE type = 'table' AND name GLOB 'app_*__*'")
            .fetch_scalars(&conn)
            .await
            .unwrap();
    assert!(
        in_main.is_empty(),
        "collection tables in the control plane: {in_main:?}"
    );

    let a = attach_owner(&conn, user(alice))
        .await
        .expect("attach alice");
    let b = attach_owner(&conn, user(bob)).await.expect("attach bob");
    assert_eq!(
        tables_in(&conn, &a, &schema_a).await,
        vec![format!("{schema_a}__entries")]
    );
    assert_eq!(
        tables_in(&conn, &b, &schema_b).await,
        vec![format!("{schema_b}__entries")]
    );
    assert!(
        tables_in(&conn, &a, &schema_b).await.is_empty(),
        "bob's tables are in alice's file"
    );
}

/// The alias is one per owner per connection: a second attach of the same
/// owner is the first one, and two owners are two attachments.
#[tokio::test]
async fn attaching_an_owner_twice_is_one_attachment() {
    let w = World::new("owner_files_idempotent").await;
    let alice = w.human("alice").await;
    let bob = w.human("bob").await;
    let conn = w.conn().await;
    let first = attach_owner(&conn, user(alice)).await.unwrap();
    let again = attach_owner(&conn, user(alice)).await.unwrap();
    assert_eq!(first, again);
    let other = attach_owner(&conn, user(bob)).await.unwrap();
    assert_ne!(first, other);
    let mut attached = conn.attached();
    attached.sort();
    let mut want = vec![first, other];
    want.sort();
    assert_eq!(attached, want);
}

/// A connection goes back to the pool with nothing attached: the next
/// caller sees only `main`.
#[tokio::test]
async fn a_pooled_connection_returns_without_its_attachments() {
    let w = World::new("owner_files_detach_on_return").await;
    let alice = w.human("alice").await;
    {
        let conn = w.conn().await;
        attach_owner(&conn, user(alice)).await.unwrap();
        assert_eq!(conn.attached().len(), 1);
    }
    let conn = w.conn().await;
    assert!(
        conn.attached().is_empty(),
        "the alias list survived the checkout"
    );
    let names: Vec<String> = query("PRAGMA database_list")
        .fetch_all(&conn)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String>("name"))
        .collect();
    assert_eq!(
        names,
        vec!["main".to_string()],
        "an attachment survived the checkout"
    );
}

/// Inside a write transaction the attach rides the transaction: a table
/// created through the alias and then rolled back is not there afterwards.
#[tokio::test]
async fn an_attach_inside_a_transaction_rolls_back_with_it() {
    let w = World::new("owner_files_rollback").await;
    let alice = w.human("alice").await;
    let tx = w.store.begin().await.unwrap();
    let alias = attach_owner(&tx, user(alice)).await.unwrap();
    query(&format!(
        "CREATE TABLE {}.probe (n INTEGER)",
        hive_db::quote_ident(&alias)
    ))
    .execute(&tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();

    let conn = w.conn().await;
    let alias = attach_owner(&conn, user(alice)).await.unwrap();
    let n: i64 = query(&format!(
        "SELECT count(*) FROM {}.sqlite_master WHERE name = 'probe'",
        hive_db::quote_ident(&alias)
    ))
    .fetch_scalar(&conn)
    .await
    .unwrap();
    assert_eq!(n, 0, "DDL in the owner file survived the rollback");
}

/// A file under one owner's name that says inside that it is somebody
/// else's is refused at the attach. The name is the key and the file
/// carries the dimension the name could lose (invariant 14).
#[tokio::test]
async fn a_file_that_names_another_owner_is_refused() {
    let w = World::new("owner_files_marker").await;
    let alice = w.human("alice").await;
    let bob = w.human("bob").await;
    {
        let conn = w.conn().await;
        attach_owner(&conn, user(alice)).await.unwrap();
    }
    // Rename alice's file to bob's, the way a careless restore would.
    let dir = owners_dir(w.db().path().expect("the sqlite engine has a path"));
    let alices = owner_file(&dir, user(alice));
    let bobs = owner_file(&dir, user(bob));
    for suffix in ["", "-wal", "-shm"] {
        let from = format!("{}{suffix}", alices.display());
        let to = format!("{}{suffix}", bobs.display());
        if std::path::Path::new(&from).exists() {
            std::fs::rename(&from, &to).unwrap();
        }
    }
    let conn = w.conn().await;
    let err = attach_owner(&conn, user(bob))
        .await
        .expect_err("a file carrying alice's marker was attached as bob's");
    let msg = err.to_string();
    assert!(
        msg.contains("belongs to user") && msg.contains(&alice.to_string()),
        "the refusal does not say whose file it is: {msg}"
    );
    assert!(
        conn.attached().is_empty(),
        "a refused file stayed attached: {:?}",
        conn.attached()
    );
}

/// A document written through the data layer is in the owner's file, and a
/// grantee's read attaches the OWNER's file, not the reader's.
#[tokio::test]
async fn a_grantees_read_attaches_the_owners_file() {
    use hive_store::{
        Access, GrantSpec, InstallSpec, Subject, activate_install, stage_install, write_grant,
    };
    use hive_wasmhost::{Caller, Request, Storage as _};

    let w = World::new("owner_files_grantee").await;
    let alice = w.human("alice").await;
    let bob = w.human("bob").await;
    let by = cred(alice, PrincipalKind::User, alice);

    // Alice's journal, built the way the registry builds it.
    let m = manifest("journal");
    let p = hive_registry::prepare(&m, &hive_wasmhost::Exports::none()).unwrap();
    let spec = p.install_spec("user", &alice.to_string()).unwrap();
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
    .unwrap();
    tx.commit().await.unwrap();
    let conn = w.conn().await;
    let install = stage_install(
        &conn,
        &InstallSpec {
            build_id: reg.build_id,
            slug: "journal".into(),
            owner: user(alice),
        },
        &by,
    )
    .await
    .unwrap();
    activate_install(&conn, install, &by).await.unwrap();
    drop(conn);

    let dir = tempfile::tempdir().unwrap();
    let catalog = std::sync::Arc::new(hive_blob::Catalog::new(
        w.db().clone(),
        Box::new(hive_blob::DiskDriver::new(dir.path()).await.unwrap()),
    ));
    let data = hive_store::AppData::new(w.store.clone(), catalog);
    let req = |c: hive_identity::Credential, body: serde_json::Value| Request {
        caller: Caller::new(c, install),
        app: String::new(),
        body: serde_json::to_vec(&body).unwrap(),
        trust: hive_trust::Level::Trusted,
        tainted_by: String::new(),
    };
    let res = data
        .insert(req(
            by,
            serde_json::json!({"collection": "entries", "doc": {"title": "mine"}}),
        ))
        .await
        .expect("insert");
    let id: Uuid = serde_json::from_slice::<serde_json::Value>(&res.data).unwrap()["id"]
        .as_str()
        .map(|s| Uuid::parse_str(s).unwrap())
        .unwrap();

    // The row is in alice's file, under her prefix.
    let conn = w.conn().await;
    let a = attach_owner(&conn, user(alice)).await.unwrap();
    let n: i64 = query(&format!(
        "SELECT count(*) FROM {} WHERE id = ?1",
        hive_store::owner_table(&a, &format!("{}__entries", reg.schema_name))
    ))
    .bind(id)
    .fetch_scalar(&conn)
    .await
    .unwrap();
    assert_eq!(n, 1, "the document is not in alice's file");
    drop(conn);

    // Bob, granted the row, reads it; bob's own file is never created.
    write_grant(
        &*w.conn().await,
        &GrantSpec::direct(Subject::entity(id), user(bob), Access::Read, by),
    )
    .await
    .unwrap();
    let bc = cred(bob, PrincipalKind::User, bob);
    let got = data
        .get(req(
            bc,
            serde_json::json!({"collection": "entries", "id": id}),
        ))
        .await
        .expect("a grantee could not read");
    let doc: serde_json::Value = serde_json::from_slice(&got.data).unwrap();
    assert_eq!(doc["doc"]["title"], "mine");
    assert!(
        !owner_file(
            &owners_dir(w.db().path().expect("the sqlite engine has a path")),
            user(bob)
        )
        .exists(),
        "reading alice's document created a file for bob"
    );
}

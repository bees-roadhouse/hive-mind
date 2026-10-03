//! The core install per owner and the grants an app's `uses` derive at
//! activation (D32, the rest of #86). Written before the code.

mod common;

use common::{World, cred, user};
use hive_db::query;
use hive_identity::{Owner, PrincipalKind};
use hive_manifest::{Collection, Kind, Manifest, Storage, Use, UseAccess};
use hive_store::{
    BootstrapConfig, BuildSpec, InstallSpec, StoreError, activate_install, attach_owner,
    core_install_id, grant_install_authority, register_build, stage_install,
};
use hive_trust::Level;
use hive_wasmhost::{Caller, Request, Storage as _};
use uuid::Uuid;

/// An app that asks for one of the owner's core collections.
fn app_using(name: &str, app: &str, collection: &str, access: UseAccess) -> Manifest {
    Manifest {
        kind: Some(Kind::App),
        name: name.into(),
        version: 1,
        storage: Storage {
            collections: vec![Collection {
                name: "notes".into(),
                crud: true,
                indexes: vec![],
            }],
            uses: vec![Use {
                app: app.into(),
                collection: collection.into(),
                access,
            }],
        },
        ..Default::default()
    }
}

/// Registers and stages `m` for `owner`, returning the disabled install.
async fn stage(w: &World, m: &Manifest, owner: Owner, actor: Uuid) -> Uuid {
    let by = cred(actor, owner.kind, owner.id);
    let p = hive_registry::prepare(m, &hive_wasmhost::Exports::none()).expect("prepare");
    let spec = p
        .install_spec(owner.kind.as_str(), &owner.id.to_string())
        .expect("install_spec");
    let tx = w.store.begin().await.unwrap();
    let reg = register_build(
        &tx,
        &BuildSpec {
            spec,
            owner: Some(owner),
            trust: "builtin".into(),
        },
        &by,
    )
    .await
    .expect("register");
    tx.commit().await.unwrap();
    let conn = w.conn().await;
    stage_install(
        &conn,
        &InstallSpec {
            build_id: reg.build_id,
            slug: m.name.clone(),
            owner,
        },
        &by,
    )
    .await
    .expect("stage")
}

async fn grants_for(w: &World, install: Uuid) -> Vec<(Uuid, String, String)> {
    query(
        "SELECT subject_id, subject_name, access FROM grants
          WHERE target_kind = 'install' AND target_install_id = ?1 AND revoked_at IS NULL
          ORDER BY subject_name",
    )
    .bind(install)
    .fetch_all(&*w.conn().await)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get("subject_id"), r.get("subject_name"), r.get("access")))
    .collect()
}

/// Bootstrapping creates an ACTIVE core install for the root user and for the
/// root org, with the five core collections provisioned in each owner's
/// file; bootstrapping again creates nothing more.
#[tokio::test]
async fn bootstrap_creates_a_core_install_per_principal() {
    let w = World::bare("core_bootstrap").await;
    let cfg = BootstrapConfig {
        root_handle: "root".into(),
        root_name: "Root".into(),
        org_handle: "home".into(),
        org_name: "Home".into(),
    };
    let res = w.store.bootstrap_in_tx(&cfg).await.expect("bootstrap");
    let root = user(res.root_actor_id);
    let org = Owner::new(PrincipalKind::Org, res.org_actor_id.expect("org"));

    let conn = w.conn().await;
    for owner in [root, org] {
        let id = core_install_id(&conn, owner)
            .await
            .expect("look up core")
            .unwrap_or_else(|| panic!("no core install for {owner:?}"));
        let state: String = query("SELECT state FROM installs WHERE id = ?1")
            .bind(id)
            .fetch_scalar(&conn)
            .await
            .unwrap();
        assert_eq!(
            state, "active",
            "the core install of {owner:?} is not active"
        );
        let schema: String = query("SELECT schema_name FROM installs WHERE id = ?1")
            .bind(id)
            .fetch_scalar(&conn)
            .await
            .unwrap();
        let alias = attach_owner(&conn, owner).await.unwrap();
        let tables: Vec<String> = query(&format!(
            "SELECT name FROM {}.sqlite_master WHERE type = 'table' AND name GLOB ?1 ORDER BY name",
            hive_db::quote_ident(&alias)
        ))
        .bind(format!("{schema}__*"))
        .fetch_scalars(&conn)
        .await
        .unwrap();
        let want: Vec<String> = ["contacts", "decisions", "entries", "lists", "tasks"]
            .iter()
            .map(|c| format!("{schema}__{c}"))
            .collect();
        assert_eq!(tables, want, "core collections of {owner:?}");
    }
    drop(conn);

    w.store
        .bootstrap_in_tx(&cfg)
        .await
        .expect("bootstrap again");
    let cores: i64 = query("SELECT count(*) FROM installs WHERE slug = 'core'")
        .fetch_scalar(&*w.conn().await)
        .await
        .unwrap();
    assert_eq!(cores, 2, "a second bootstrap made more core installs");
}

/// Activating an app whose manifest `uses` a core collection, by the human
/// owner, writes the install grant; the app then reaches `core/contacts`,
/// and the row lands in the owner's core install. Activating again writes
/// nothing more.
#[tokio::test]
async fn activation_derives_the_grants_an_app_uses() {
    let w = World::new("core_uses_grants").await;
    let alice = w.human("alice").await;
    let by = cred(alice, PrincipalKind::User, alice);
    let core = hive_store::ensure_core_install(&*w.conn().await, user(alice), &by)
        .await
        .expect("core for alice");

    let m = app_using("addressbook", "core", "contacts", UseAccess::Write);
    let install = stage(&w, &m, user(alice), alice).await;
    assert!(
        grants_for(&w, install).await.is_empty(),
        "staging granted something"
    );

    let conn = w.conn().await;
    activate_install(&conn, install, &by)
        .await
        .expect("activate");
    let grants = grants_for(&w, install).await;
    assert_eq!(
        grants,
        vec![(core, "contacts".to_string(), "write".to_string())]
    );
    activate_install(&conn, install, &by)
        .await
        .expect("activate again");
    assert_eq!(
        grants_for(&w, install).await.len(),
        1,
        "a second activation duplicated the grant"
    );
    drop(conn);

    let dir = tempfile::tempdir().unwrap();
    let catalog = std::sync::Arc::new(hive_blob::Catalog::new(
        w.db().clone(),
        Box::new(hive_blob::DiskDriver::new(dir.path()).await.unwrap()),
    ));
    let data = hive_store::AppData::new(w.store.clone(), catalog);
    let res = data
        .insert(Request {
            caller: Caller::new(by, install),
            app: String::new(),
            body: serde_json::to_vec(&serde_json::json!({
                "collection": "core/contacts", "doc": {"name": "Bob"}
            }))
            .unwrap(),
            trust: Level::Trusted,
            tainted_by: String::new(),
        })
        .await
        .expect("the granted app could not write core/contacts");
    let out: serde_json::Value = serde_json::from_slice(&res.data).unwrap();
    let id: Uuid = out["id"].as_str().unwrap().parse().unwrap();
    let landed: Uuid = query("SELECT install_id FROM entities WHERE id = ?1")
        .bind(id)
        .fetch_scalar(&*w.conn().await)
        .await
        .unwrap();
    assert_eq!(landed, core, "the contact did not land in the core install");
}

/// Without the grant the same write is refused: the derived grant is what
/// opens the door, not the declaration.
#[tokio::test]
async fn a_use_without_activation_still_denies() {
    let w = World::new("core_uses_denies_until_activated").await;
    let alice = w.human("alice").await;
    let by = cred(alice, PrincipalKind::User, alice);
    hive_store::ensure_core_install(&*w.conn().await, user(alice), &by)
        .await
        .unwrap();
    let m = app_using("addressbook", "core", "contacts", UseAccess::Write);
    let install = stage(&w, &m, user(alice), alice).await;
    // Activate without the derivation, the way the store did before #86
    // finished: a direct state flip. The grant must not exist on its own.
    query("UPDATE installs SET state = 'active', activated_by_actor = ?2 WHERE id = ?1")
        .bind(install)
        .bind(alice)
        .execute(&*w.conn().await)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let catalog = std::sync::Arc::new(hive_blob::Catalog::new(
        w.db().clone(),
        Box::new(hive_blob::DiskDriver::new(dir.path()).await.unwrap()),
    ));
    let data = hive_store::AppData::new(w.store.clone(), catalog);
    let err = data
        .insert(Request {
            caller: Caller::new(by, install),
            app: String::new(),
            body: serde_json::to_vec(&serde_json::json!({
                "collection": "core/contacts", "doc": {"name": "Bob"}
            }))
            .unwrap(),
            trust: Level::Trusted,
            tainted_by: String::new(),
        })
        .await
        .expect_err("an app reached core/contacts with no grant");
    assert_eq!(err.status(), hive_wasmhost::Status::Denied, "{err}");
}

/// An app that asks for access to other apps is activated by a person. The
/// standing-authority route an AI uses cannot write those grants (D13.14),
/// so it refuses rather than activating an app whose declaration would then
/// mean nothing.
#[tokio::test]
async fn an_app_with_uses_needs_a_human_to_activate() {
    let w = World::new("core_uses_needs_human").await;
    let alice = w.human("alice").await;
    let by = cred(alice, PrincipalKind::User, alice);
    hive_store::ensure_core_install(&*w.conn().await, user(alice), &by)
        .await
        .unwrap();
    let ava = w.ai("ava", "assistant", user(alice), alice).await;
    let m = app_using("addressbook", "core", "contacts", UseAccess::Read);
    let install = stage(&w, &m, user(alice), alice).await;
    let conn = w.conn().await;
    grant_install_authority(&conn, install, user(alice), "activate", &by, "test", None)
        .await
        .expect("delegate");
    let err = activate_install(&conn, install, &cred(ava, PrincipalKind::User, alice))
        .await
        .expect_err("an AI activated an app that uses another app's collection");
    assert!(matches!(err, StoreError::NotHuman(_)), "{err}");
    let state: String = query("SELECT state FROM installs WHERE id = ?1")
        .bind(install)
        .fetch_scalar(&conn)
        .await
        .unwrap();
    assert_eq!(state, "disabled", "the install was activated anyway");
    assert!(grants_for(&w, install).await.is_empty());
}

/// A `uses` that names an app the owner has not installed refuses the
/// activation and names the app, rather than activating an install whose
/// every cross-app read would deny with nothing in the logs to say why.
#[tokio::test]
async fn a_use_of_an_app_the_owner_lacks_refuses_activation() {
    let w = World::new("core_uses_missing_app").await;
    let alice = w.human("alice").await;
    let by = cred(alice, PrincipalKind::User, alice);
    let m = app_using("mail", "calendar", "events", UseAccess::Read);
    let install = stage(&w, &m, user(alice), alice).await;
    let conn = w.conn().await;
    let err = activate_install(&conn, install, &by)
        .await
        .expect_err("activated with a use that resolves to nothing");
    assert!(
        err.to_string().contains("calendar"),
        "the refusal does not name the missing app: {err}"
    );
    let state: String = query("SELECT state FROM installs WHERE id = ?1")
        .bind(install)
        .fetch_scalar(&conn)
        .await
        .unwrap();
    assert_eq!(state, "disabled");
}

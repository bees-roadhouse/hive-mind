//! `Store::open` is the daemon's path and nothing else's: every other suite
//! builds a store from files the fixture opened. The e2e suite found the
//! daemon refusing to boot on a ping that was written as a statement, so the
//! boot path gets a test of its own here.

use hive_store::{BootstrapConfig, Store};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join("hive-sandbox-tests")
        .join(format!("open_{name}_{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Open an empty directory, migrate both files, bootstrap, close, and open
/// the same directory again: the second open must find what the first wrote.
#[tokio::test]
async fn open_creates_migrates_and_reopens() {
    let dir = scratch("reopen");
    let store = Store::open(&dir).await.expect("first open");
    assert!(dir.join(hive_store::DB_FILE).is_file());
    assert!(dir.join(hive_store::AUDIT_FILE).is_file());
    hive_store::migrate(store.db()).await.expect("migrate");
    hive_store::migrate_audit(store.audit())
        .await
        .expect("migrate audit");
    let res = store
        .bootstrap_in_tx(&BootstrapConfig {
            root_handle: "root".into(),
            root_name: "Root".into(),
            ..Default::default()
        })
        .await
        .expect("bootstrap");
    store.close().await;
    drop(store);

    let again = Store::open(&dir).await.expect("second open");
    let applied = hive_store::migrate(again.db()).await.expect("re-migrate");
    assert!(
        applied.is_empty(),
        "a migrated file was migrated again: {applied:?}"
    );
    let conn = again.conn().await.expect("conn");
    let root = hive_store::actor_by_id(&conn, res.root_actor_id)
        .await
        .expect("the root actor survived the reopen");
    assert_eq!(root.handle, "root");
    drop(conn);
    again.close().await;
    drop(again);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory that does not exist yet is created, the way a first boot on a
/// fresh volume needs.
#[tokio::test]
async fn open_creates_the_directory() {
    let dir = scratch("mkdir").join("nested").join("data");
    assert!(!dir.exists());
    let store = Store::open(&dir).await.expect("open");
    assert!(dir.join(hive_store::DB_FILE).is_file());
    store.close().await;
    drop(store);
    let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
}

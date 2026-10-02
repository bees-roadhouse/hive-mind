//! Forward-only migrations.
//!
//! The SQL files under `migrations/` are embedded and applied in order to the
//! one file the daemon opens (D38). The history restarts at 0001 with the
//! engine change, because no hive-mind database was ever deployed on the
//! Postgres schema; the machinery is the one the Go tree and the Postgres
//! port used: a `schema_migrations` table, a SHA-256 over the bytes, and a
//! file that was applied differently refused rather than reapplied.

use hive_db::{Db, Transaction, query};
use sha2::{Digest, Sha256};

/// One forward-only step. There are no down migrations: rolling back a schema
/// on live data is a restore, not a migration.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: &'static str,
    pub name: &'static str,
    pub sql: &'static str,
}

impl Migration {
    /// SHA-256 of the file bytes, hex. What the Go tree recorded.
    pub fn checksum(&self) -> String {
        hex::encode(Sha256::digest(self.sql.as_bytes()))
    }
}

/// Every migration this binary carries, in order. Adding a file to the shared
/// directory means adding a line here, and a test fails until it is.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: "0001",
    name: "init",
    sql: include_str!("../migrations/0001_init.sql"),
}];

/// The override audit's own file (D38 §3): evidence that must survive any
/// caller's transaction, which on one file per writer means its own file.
pub const AUDIT_MIGRATIONS: &[Migration] = &[Migration {
    version: "0001",
    name: "override_audit",
    sql: include_str!("../migrations-audit/0001_override_audit.sql"),
}];

/// An owner's file (D39): the marker that says whose it is. The collection
/// tables in it are provisioned from manifests, never by a migration.
pub const OWNER_MIGRATIONS: &[Migration] = &[Migration {
    version: "0001",
    name: "owner_file",
    sql: include_str!("../migrations-owner/0001_owner_file.sql"),
}];

/// The shared directories, for the tests that keep the lists honest.
pub const SHARED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
pub const AUDIT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations-audit");
pub const OWNER_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations-owner");

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error(
        "migration {version} ({name}) changed after it was applied: recorded {recorded}, embedded {embedded}"
    )]
    Changed {
        version: String,
        name: String,
        recorded: String,
        embedded: String,
    },
    #[error("database has migration {0} applied but this binary does not carry it")]
    Unknown(String),
    #[error("migration {version} ({name}): {source}")]
    Apply {
        version: String,
        name: String,
        #[source]
        source: hive_db::Error,
    },
    #[error(transparent)]
    Db(#[from] hive_db::Error),
}

/// Applies every embedded migration that has not been applied yet and returns
/// the versions it applied. Safe to call concurrently from any number of
/// processes: the whole run is one `BEGIN IMMEDIATE` transaction, so the
/// engine's write lock is the mutex and the loser waits, then finds nothing to
/// do. (The Postgres port used an advisory lock for the same reason.)
pub async fn migrate(db: &Db) -> Result<Vec<String>, MigrateError> {
    apply_all(db, MIGRATIONS).await
}

/// The audit file's migrations, same machinery.
pub async fn migrate_audit(db: &Db) -> Result<Vec<String>, MigrateError> {
    apply_all(db, AUDIT_MIGRATIONS).await
}

/// An owner file's migrations, same machinery (D39).
pub async fn migrate_owner(db: &Db) -> Result<Vec<String>, MigrateError> {
    apply_all(db, OWNER_MIGRATIONS).await
}

async fn apply_all(db: &Db, list: &[Migration]) -> Result<Vec<String>, MigrateError> {
    let tx = db.begin().await?;
    query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    TEXT PRIMARY KEY,
            name       TEXT NOT NULL,
            checksum   TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        )",
    )
    .execute(&tx)
    .await?;

    let applied: Vec<(String, String)> = query("SELECT version, checksum FROM schema_migrations")
        .fetch_all(&tx)
        .await?
        .iter()
        .map(|r| (r.get("version"), r.get("checksum")))
        .collect();

    let mut ran = Vec::new();
    for m in list {
        let embedded = m.checksum();
        if let Some((_, recorded)) = applied.iter().find(|(v, _)| v == m.version) {
            // Migrations are immutable once applied. A silent edit means two
            // databases claiming the same version have different schemas,
            // which is the drift nobody notices until a query fails in
            // production only.
            if *recorded != embedded {
                return Err(MigrateError::Changed {
                    version: m.version.to_string(),
                    name: m.name.to_string(),
                    recorded: recorded.clone(),
                    embedded,
                });
            }
            continue;
        }
        apply(&tx, m, &embedded).await?;
        ran.push(m.version.to_string());
    }

    // An applied version this binary does not carry means someone deleted a
    // migration file. The schema in front of us is not one this binary knows
    // how to talk to, so say that rather than proceeding hopefully.
    for (version, _) in &applied {
        if !list.iter().any(|m| m.version == version) {
            return Err(MigrateError::Unknown(version.clone()));
        }
    }
    tx.commit().await?;
    Ok(ran)
}

async fn apply(tx: &Transaction, m: &Migration, checksum: &str) -> Result<(), MigrateError> {
    let wrap = |source: hive_db::Error| MigrateError::Apply {
        version: m.version.to_string(),
        name: m.name.to_string(),
        source,
    };
    // A migration is many statements; the batch runs them in the enclosing
    // transaction, so a file that fails halfway leaves nothing behind.
    Db::batch(tx, m.sql).await.map_err(wrap)?;
    query("INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (?1, ?2, ?3, ?4)")
        .bind(m.version)
        .bind(m.name)
        .bind(checksum)
        .bind(hive_db::now())
        .execute(tx)
        .await
        .map_err(wrap)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The directory and the embedded list must agree. A file dropped into
    /// `migrations/` that this list does not carry would never run, and a
    /// listed file that is not on disk fails at compile time; this catches the
    /// first shape.
    #[test]
    fn embedded_migrations_match_the_shared_directory() {
        for (dir, list, what) in [
            (SHARED_DIR, MIGRATIONS, "MIGRATIONS"),
            (AUDIT_DIR, AUDIT_MIGRATIONS, "AUDIT_MIGRATIONS"),
            (OWNER_DIR, OWNER_MIGRATIONS, "OWNER_MIGRATIONS"),
        ] {
            let mut on_disk: Vec<String> = std::fs::read_dir(dir)
                .expect("shared migrations directory")
                .map(|e| {
                    e.expect("dir entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .filter(|n| n.ends_with(".sql"))
                .collect();
            on_disk.sort();
            let embedded: Vec<String> = list
                .iter()
                .map(|m| format!("{}_{}.sql", m.version, m.name))
                .collect();
            assert_eq!(on_disk, embedded, "{dir} and {what} disagree");
        }
    }

    #[tokio::test]
    async fn the_audit_file_migrates_on_its_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("hive-audit.db"))
            .await
            .expect("open");
        assert_eq!(
            migrate_audit(&db).await.expect("migrate"),
            vec!["0001".to_string()]
        );
        assert!(migrate_audit(&db).await.expect("again").is_empty());
        let c = db.conn().await.unwrap();
        let n: i64 = query("SELECT count(*) FROM grant_override_audit")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn versions_are_ordered_and_unique() {
        let versions: Vec<&str> = MIGRATIONS.iter().map(|m| m.version).collect();
        let mut sorted = versions.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(versions, sorted);
    }

    #[test]
    fn checksum_is_sha256_hex_of_the_bytes() {
        let m = Migration {
            version: "9999",
            name: "probe",
            sql: "SELECT 1;\n",
        };
        assert_eq!(m.checksum().len(), 64);
        assert_eq!(m.checksum(), hex::encode(Sha256::digest(b"SELECT 1;\n")));
    }

    async fn fresh() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("hive.db")).await.expect("open");
        (dir, db)
    }

    /// The file parses and applies. This is the test that catches a SQL
    /// mistake in migration one before any store test does, and it is the
    /// first thing to run after editing the file.
    #[tokio::test]
    async fn migration_one_applies_to_a_fresh_file_and_is_idempotent() {
        let (_d, db) = fresh().await;
        let ran = migrate(&db).await.expect("first migrate");
        assert_eq!(ran, vec!["0001".to_string()]);
        let again = migrate(&db).await.expect("second migrate");
        assert!(again.is_empty(), "nothing to apply the second time");
        let c = db.conn().await.unwrap();
        let tables: Vec<String> =
            query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_scalars(&c)
                .await
                .unwrap();
        for want in [
            "actors",
            "grants",
            "events",
            "installs",
            "chat_turns",
            "schema_migrations",
        ] {
            assert!(
                tables.iter().any(|t| t == want),
                "missing table {want}: {tables:?}"
            );
        }
        let views: Vec<String> = query("SELECT name FROM sqlite_master WHERE type = 'view'")
            .fetch_scalars(&c)
            .await
            .unwrap();
        assert!(views.iter().any(|v| v == "subject_owners"), "{views:?}");
        assert!(
            views.iter().any(|v| v == "builds_awaiting_promotion"),
            "{views:?}"
        );
    }

    #[tokio::test]
    async fn an_edited_migration_is_refused() {
        let (_d, db) = fresh().await;
        migrate(&db).await.expect("migrate");
        let c = db.conn().await.unwrap();
        query("UPDATE schema_migrations SET checksum = 'not-the-bytes' WHERE version = '0001'")
            .execute(&c)
            .await
            .unwrap();
        let err = migrate(&db).await.unwrap_err();
        assert!(matches!(err, MigrateError::Changed { .. }), "{err}");
    }

    #[tokio::test]
    async fn an_unknown_applied_version_is_refused() {
        let (_d, db) = fresh().await;
        migrate(&db).await.expect("migrate");
        let c = db.conn().await.unwrap();
        query("INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES ('0999', 'future', 'x', 0)")
            .execute(&c)
            .await
            .unwrap();
        let err = migrate(&db).await.unwrap_err();
        assert!(
            matches!(err, MigrateError::Unknown(ref v) if v == "0999"),
            "{err}"
        );
    }
}

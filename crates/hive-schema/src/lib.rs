//! Forward-only migrations.
//!
//! The SQL files under `migrations/` are embedded and applied in order to the
//! one file the daemon opens (D38). The history restarts at 0001 with the
//! engine change, because no hive-mind database was ever deployed on the
//! Postgres schema; the machinery is the one the Go tree and the Postgres
//! port used: a `schema_migrations` table, a SHA-256 over the bytes, and a
//! file that was applied differently refused rather than reapplied.

use hive_db::{Connection, Db, query};
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

/// The shared directory, for the test that keeps `MIGRATIONS` honest.
pub const SHARED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");

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
    let conn = db.conn().await?;
    migrate_on(&conn).await
}

/// The same, on a connection the caller holds.
pub async fn migrate_on(conn: &Connection) -> Result<Vec<String>, MigrateError> {
    let tx = Db::begin_on(conn).await?;
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
    for m in MIGRATIONS {
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
        if !MIGRATIONS.iter().any(|m| m.version == version) {
            return Err(MigrateError::Unknown(version.clone()));
        }
    }
    tx.commit().await.map_err(hive_db::Error::from)?;
    Ok(ran)
}

async fn apply(tx: &Connection, m: &Migration, checksum: &str) -> Result<(), MigrateError> {
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
        let mut on_disk: Vec<String> = std::fs::read_dir(SHARED_DIR)
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
        let embedded: Vec<String> = MIGRATIONS
            .iter()
            .map(|m| format!("{}_{}.sql", m.version, m.name))
            .collect();
        assert_eq!(
            on_disk, embedded,
            "crates/hive-schema/migrations and MIGRATIONS disagree"
        );
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
        for want in ["actors", "grants", "events", "installs", "chat_turns", "schema_migrations"] {
            assert!(tables.iter().any(|t| t == want), "missing table {want}: {tables:?}");
        }
        let views: Vec<String> = query("SELECT name FROM sqlite_master WHERE type = 'view'")
            .fetch_scalars(&c)
            .await
            .unwrap();
        assert!(views.iter().any(|v| v == "subject_owners"), "{views:?}");
        assert!(views.iter().any(|v| v == "builds_awaiting_promotion"), "{views:?}");
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
        assert!(matches!(err, MigrateError::Unknown(ref v) if v == "0999"), "{err}");
    }
}

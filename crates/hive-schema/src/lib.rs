//! Forward-only migrations, in two dialects.
//!
//! The SQL files under `migrations/` are the SQLite set, applied to the file
//! the daemon opens (D38); the files under `migrations-pg/` are the Postgres
//! set, applied to an organization's database (D43). One schema, two texts:
//! the suite that runs against both engines is what keeps them the same
//! schema. `migrate` picks the set by the engine of the `Db` it is given.
//! The history restarted at 0001 with the engine change, because no
//! hive-mind database was ever deployed on the earlier Postgres schema; the
//! machinery is the one the Go tree and the first Postgres port used: a
//! `schema_migrations` table, a SHA-256 over the bytes, and a file that was
//! applied differently refused rather than reapplied.

use hive_db::{Db, Engine, Transaction, query};
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

/// Every SQLite migration this binary carries, in order. Adding a file to the
/// shared directory means adding a line here, and a test fails until it is.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: "0001",
        name: "init",
        sql: include_str!("../migrations/0001_init.sql"),
    },
    Migration {
        version: "0002",
        name: "model_jobs",
        sql: include_str!("../migrations/0002_model_jobs.sql"),
    },
];

/// Every Postgres migration this binary carries, in order (D43). Empty until
/// phase 1's second slice ports migration one; `migrate` refuses a Postgres
/// `Db` with [`MigrateError::NoMigrations`] until then, so nothing can read
/// an empty database as a migrated one.
pub const PG_MIGRATIONS: &[Migration] = &[];

/// The override audit's own file (D38 §3): evidence that must survive any
/// caller's transaction, which on one file per writer means its own file.
/// SQLite only: on Postgres the audit table is in [`PG_MIGRATIONS`] and a
/// second connection on the same database is what survives the rollback.
pub const AUDIT_MIGRATIONS: &[Migration] = &[Migration {
    version: "0001",
    name: "override_audit",
    sql: include_str!("../migrations-audit/0001_override_audit.sql"),
}];

/// An owner's file (D39): the marker that says whose it is. The collection
/// tables in it are provisioned from manifests, never by a migration. SQLite
/// only; D43 §5 says why there is no Postgres form.
pub const OWNER_MIGRATIONS: &[Migration] = &[Migration {
    version: "0001",
    name: "owner_file",
    sql: include_str!("../migrations-owner/0001_owner_file.sql"),
}];

/// The shared directories, for the tests that keep the lists honest.
pub const SHARED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
pub const PG_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations-pg");
pub const AUDIT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations-audit");
pub const OWNER_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations-owner");

/// The table the override audit lands in, which `migrate_audit` looks for
/// on Postgres rather than creating.
const AUDIT_TABLE: &str = "grant_override_audit";

/// The advisory lock every Postgres migration run takes for its transaction,
/// so two daemons migrating the same database at once serialise and the
/// loser finds nothing to do, the way `BEGIN IMMEDIATE` does it on a file.
/// One key for the whole platform: a migration run is rare and short.
const PG_MIGRATE_LOCK: i64 = 0x6869_7665;

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
    /// The set for this engine is empty, so there is nothing that could be
    /// called a migrated database.
    #[error("this binary carries no {0} migrations")]
    NoMigrations(Engine),
    /// A set that has no form on this engine: the owner files on Postgres.
    #[error("{1} has no {0} form")]
    Unsupported(Engine, &'static str),
    /// `migrate_audit` on Postgres found no audit table: `migrate` has not
    /// run on this database.
    #[error("{0} is not in this database; run migrate first")]
    AuditMissing(&'static str),
    #[error(transparent)]
    Db(#[from] hive_db::Error),
}

/// Applies every embedded migration for `db`'s engine that has not been
/// applied yet and returns the versions it applied. Safe to call concurrently
/// from any number of processes: the whole run is one transaction under the
/// engine's write lock (`BEGIN IMMEDIATE` on a file, an advisory lock on
/// Postgres), so the loser waits, then finds nothing to do.
pub async fn migrate(db: &Db) -> Result<Vec<String>, MigrateError> {
    match db.engine() {
        Engine::Sqlite => apply_all(db, MIGRATIONS).await,
        Engine::Postgres => apply_all(db, PG_MIGRATIONS).await,
    }
}

/// The audit file's migrations on SQLite, same machinery. On Postgres the
/// audit table is part of the main set, so this checks it is there.
pub async fn migrate_audit(db: &Db) -> Result<Vec<String>, MigrateError> {
    match db.engine() {
        Engine::Sqlite => apply_all(db, AUDIT_MIGRATIONS).await,
        Engine::Postgres => {
            let c = db.conn().await?;
            let present: bool = query("SELECT to_regclass(?1) IS NOT NULL")
                .bind(AUDIT_TABLE)
                .fetch_scalar(&c)
                .await?;
            if present {
                Ok(Vec::new())
            } else {
                Err(MigrateError::AuditMissing(AUDIT_TABLE))
            }
        }
    }
}

/// An owner file's migrations, same machinery (D39). SQLite only.
pub async fn migrate_owner(db: &Db) -> Result<Vec<String>, MigrateError> {
    match db.engine() {
        Engine::Sqlite => apply_all(db, OWNER_MIGRATIONS).await,
        Engine::Postgres => Err(MigrateError::Unsupported(
            Engine::Postgres,
            "an owner file (D39)",
        )),
    }
}

/// The `schema_migrations` table in the engine's types. The SQLite column is
/// the D38 §3 integer; the Postgres one is `timestamptz`, and
/// [`hive_db::now`] binds to either.
fn migrations_table_sql(engine: Engine) -> &'static str {
    match engine {
        Engine::Sqlite => {
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version    TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                checksum   TEXT NOT NULL,
                applied_at INTEGER NOT NULL
            )"
        }
        Engine::Postgres => {
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version    text PRIMARY KEY,
                name       text NOT NULL,
                checksum   text NOT NULL,
                applied_at timestamptz NOT NULL
            )"
        }
    }
}

async fn apply_all(db: &Db, list: &[Migration]) -> Result<Vec<String>, MigrateError> {
    let engine = db.engine();
    if list.is_empty() {
        return Err(MigrateError::NoMigrations(engine));
    }
    let tx = db.begin().await?;
    if engine == Engine::Postgres {
        query("SELECT pg_advisory_xact_lock(?1)")
            .bind(PG_MIGRATE_LOCK)
            .fetch_one(&tx)
            .await?;
    }
    query(migrations_table_sql(engine)).execute(&tx).await?;

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
    /// a migrations directory that its list does not carry would never run,
    /// and a listed file that is not on disk fails at compile time; this
    /// catches the first shape.
    #[test]
    fn embedded_migrations_match_the_shared_directory() {
        for (dir, list, what) in [
            (SHARED_DIR, MIGRATIONS, "MIGRATIONS"),
            (PG_DIR, PG_MIGRATIONS, "PG_MIGRATIONS"),
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
        for list in [MIGRATIONS, PG_MIGRATIONS] {
            let versions: Vec<&str> = list.iter().map(|m| m.version).collect();
            let mut sorted = versions.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(versions, sorted);
        }
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
        assert_eq!(ran, vec!["0001".to_string(), "0002".to_string()]);
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

    /// A private schema on the test server, with a `Db` whose every
    /// connection has it first on the search path, so the runner's own
    /// table lands there and tests on one database share nothing. Dropped
    /// on the way out. `None` means no server is configured and the skip was
    /// printed.
    struct PgSchema {
        db: Option<Db>,
        url: String,
        name: String,
    }

    impl PgSchema {
        async fn new(test: &str) -> Option<PgSchema> {
            let url = hive_db::test_postgres_url(test)?;
            let name = format!("t_{}", uuid::Uuid::new_v4().simple());
            {
                let admin = Db::connect(&url).await.expect("connect");
                let c = admin.conn().await.unwrap();
                Db::batch(&c, &format!("CREATE SCHEMA {name}"))
                    .await
                    .unwrap();
                drop(c);
                admin.close();
            }
            let sep = if url.contains('?') { '&' } else { '?' };
            let scoped = format!("{url}{sep}options=-c%20search_path%3D{name}");
            let db = Db::connect(&scoped).await.expect("connect scoped");
            Some(PgSchema {
                db: Some(db),
                url,
                name,
            })
        }

        fn db(&self) -> &Db {
            self.db.as_ref().expect("present until drop")
        }
    }

    impl Drop for PgSchema {
        /// Drops the schema from a thread with a runtime of its own, on a
        /// connection of its own. Not through a pool made on the test's
        /// runtime: a pooled connection's driver task lives on the runtime
        /// that created it, and `#[tokio::test]` is a single thread that is
        /// blocked right here in `join`, so a query on that connection would
        /// wait for a response nothing can deliver. The first version did
        /// exactly that and hung.
        fn drop(&mut self) {
            if let Some(db) = self.db.take() {
                db.close();
            }
            let url = self.url.clone();
            let name = self.name.clone();
            let _ = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async move {
                    let Ok(admin) = Db::connect(&url).await else {
                        eprintln!("could not reconnect to drop schema {name}");
                        return;
                    };
                    if let Ok(c) = admin.conn().await {
                        let _ = Db::batch(&c, "SET lock_timeout = '15s'").await;
                        if let Err(e) = Db::batch(&c, &format!("DROP SCHEMA {name} CASCADE")).await
                        {
                            eprintln!("drop schema {name}: {e}");
                        }
                    }
                    admin.close();
                });
            })
            .join();
        }
    }

    /// With no Postgres set yet, `migrate` on a Postgres `Db` refuses, and
    /// leaves no `schema_migrations` behind to make the database look
    /// migrated; the audit check and the owner set refuse by name too.
    #[tokio::test]
    async fn postgres_refuses_until_its_set_exists() {
        let Some(s) = PgSchema::new("hive-schema::postgres_refuses_until_its_set_exists").await
        else {
            return;
        };
        let err = migrate(s.db()).await.unwrap_err();
        assert!(
            matches!(err, MigrateError::NoMigrations(Engine::Postgres)),
            "{err}"
        );
        let c = s.db().conn().await.unwrap();
        let present: bool = query("SELECT to_regclass('schema_migrations') IS NOT NULL")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert!(!present, "a refused migrate created schema_migrations");
        let err = migrate_audit(s.db()).await.unwrap_err();
        assert!(matches!(err, MigrateError::AuditMissing(_)), "{err}");
        let err = migrate_owner(s.db()).await.unwrap_err();
        assert!(
            matches!(err, MigrateError::Unsupported(Engine::Postgres, _)),
            "{err}"
        );
    }

    /// The runner's machinery on Postgres, against a list of its own:
    /// applies in order, idempotent, an edit refused, an unknown version
    /// refused, the advisory lock taken and released.
    #[tokio::test]
    async fn postgres_runner_applies_checks_and_refuses() {
        let Some(s) =
            PgSchema::new("hive-schema::postgres_runner_applies_checks_and_refuses").await
        else {
            return;
        };
        const PROBE: &[Migration] = &[
            Migration {
                version: "0001",
                name: "probe",
                sql: "CREATE TABLE probe (n integer);",
            },
            Migration {
                version: "0002",
                name: "probe_row",
                sql: "INSERT INTO probe VALUES (1);",
            },
        ];
        let ran = apply_all(s.db(), PROBE).await.expect("first apply");
        assert_eq!(ran, vec!["0001".to_string(), "0002".to_string()]);
        assert!(apply_all(s.db(), PROBE).await.expect("again").is_empty());
        let c = s.db().conn().await.unwrap();
        let n: i64 = query("SELECT count(*) FROM probe")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(n, 1, "the second run re-applied a migration");
        let at: chrono::DateTime<chrono::Utc> =
            query("SELECT applied_at FROM schema_migrations WHERE version = '0001'")
                .fetch_scalar(&c)
                .await
                .unwrap();
        assert!((hive_db::now() - at).num_seconds().abs() < 60);
        let held: i64 = query("SELECT count(*) FROM pg_locks WHERE locktype = 'advisory'")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(held, 0, "the migration lock outlived its transaction");

        query("UPDATE schema_migrations SET checksum = 'not-the-bytes' WHERE version = '0001'")
            .execute(&c)
            .await
            .unwrap();
        let err = apply_all(s.db(), PROBE).await.unwrap_err();
        assert!(matches!(err, MigrateError::Changed { .. }), "{err}");
        query("UPDATE schema_migrations SET checksum = ?1 WHERE version = '0001'")
            .bind(PROBE[0].checksum())
            .execute(&c)
            .await
            .unwrap();
        query("INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES ('0999', 'future', 'x', now())")
            .execute(&c)
            .await
            .unwrap();
        let err = apply_all(s.db(), PROBE).await.unwrap_err();
        assert!(
            matches!(err, MigrateError::Unknown(ref v) if v == "0999"),
            "{err}"
        );
    }
}

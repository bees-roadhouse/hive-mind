//! A private database per test, for the integration tests, on either engine.
//!
//! Every test gets a store of its own, migrated to the current schema and
//! deleted on the way out, so there is no shared mutable fixture and no
//! ordering between tests: run one, run them in parallel, run them in any
//! order. Which engine is an environment fact:
//!
//! - **SQLite** (the default): a file under the system temp directory and the
//!   override audit's file beside it, exactly as the daemon lays them out
//!   (D38). Nothing has to be running. `HIVE_SANDBOX_TEST_DB_DIR` moves the
//!   files for a machine whose temp is a slow or small disk.
//! - **Postgres** (`HIVE_SANDBOX_TEST_DATABASE_URL` set, D43): a schema of
//!   its own on that server, first on every connection's `search_path`, so
//!   the runner's `schema_migrations` and every table land there and tests
//!   on one database share nothing. The Go tree's `internal/testdb` and the
//!   first Postgres port did it this way. The audit is the same database
//!   (D43 §5: a second connection is what survives the caller's rollback
//!   there, not a second file).
//!
//! Without the URL a test runs on SQLite and does not skip: the SQLite run is
//! the gate's run. `HIVE_SANDBOX_REQUIRE_DATABASE_TESTS=1`, which CI's
//! `postgres` job sets, makes the absence of the URL a panic instead, so a
//! job that promised a server cannot quietly test the file engine.

use std::path::{Path, PathBuf};

use hive_db::{Db, Engine};

/// Overrides where SQLite test files are created. Optional.
pub const DIR_ENV: &str = "HIVE_SANDBOX_TEST_DB_DIR";

/// One test's private store, migrated.
pub struct TestDb {
    inner: Inner,
}

enum Inner {
    Sqlite {
        db: Option<Db>,
        audit: Option<Db>,
        path: PathBuf,
        audit_path: PathBuf,
    },
    Postgres {
        db: Option<Db>,
        url: String,
        schema: String,
    },
}

impl TestDb {
    /// Creates and migrates a private store for `test_name`. The name is in
    /// the file or schema name so a leftover from a test killed from outside
    /// says which test it was.
    pub async fn new(test_name: &str) -> Self {
        match postgres_url() {
            Some(url) => Self::postgres(test_name, url).await,
            None => Self::sqlite(test_name).await,
        }
    }

    async fn sqlite(test_name: &str) -> Self {
        let dir = match std::env::var(DIR_ENV) {
            Ok(d) if !d.trim().is_empty() => PathBuf::from(d.trim()),
            _ => std::env::temp_dir().join("hive-sandbox-tests"),
        };
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
        let stem = private_name(test_name);
        let path = dir.join(format!("{stem}.db"));
        let audit_path = dir.join(format!("{stem}-audit.db"));
        let db = Db::open(&path)
            .await
            .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
        hive_schema::migrate(&db)
            .await
            .unwrap_or_else(|e| panic!("migrate {}: {e}", path.display()));
        let audit = Db::open(&audit_path)
            .await
            .unwrap_or_else(|e| panic!("open {}: {e}", audit_path.display()));
        hive_schema::migrate_audit(&audit)
            .await
            .unwrap_or_else(|e| panic!("migrate {}: {e}", audit_path.display()));
        Self {
            inner: Inner::Sqlite {
                db: Some(db),
                audit: Some(audit),
                path,
                audit_path,
            },
        }
    }

    async fn postgres(test_name: &str, url: String) -> Self {
        let schema = private_name(test_name);
        {
            // The admin connection is used once and closed: its pool must
            // not outlive the test's runtime (see `Drop`).
            let admin = Db::connect(&url)
                .await
                .unwrap_or_else(|e| panic!("connect to {}: {e}", hive_db::TEST_URL_ENV));
            let c = admin.conn().await.expect("admin connection");
            Db::batch(
                &c,
                &format!("CREATE SCHEMA {}", hive_db::quote_ident(&schema)),
            )
            .await
            .unwrap_or_else(|e| panic!("create schema {schema}: {e}"));
            drop(c);
            admin.close();
        }
        // search_path as a startup parameter, so every pooled connection has
        // it from its first statement. application_name is the schema, so
        // the drop below can find every session this test opened without
        // trusting the pool to have closed them. The name is [a-z0-9_] only,
        // so it needs no quoting and survives the URL unencoded.
        let sep = if url.contains('?') { '&' } else { '?' };
        let scoped =
            format!("{url}{sep}options=-c%20search_path%3D{schema}&application_name={schema}");
        let db = Db::connect(&scoped)
            .await
            .unwrap_or_else(|e| panic!("connect to schema {schema}: {e}"));
        hive_schema::migrate(&db)
            .await
            .unwrap_or_else(|e| panic!("migrate schema {schema}: {e}"));
        hive_schema::migrate_audit(&db)
            .await
            .unwrap_or_else(|e| panic!("audit table in schema {schema}: {e}"));
        Self {
            inner: Inner::Postgres {
                db: Some(db),
                url,
                schema,
            },
        }
    }

    pub fn engine(&self) -> Engine {
        match self.inner {
            Inner::Sqlite { .. } => Engine::Sqlite,
            Inner::Postgres { .. } => Engine::Postgres,
        }
    }

    /// The platform store, for everything that opens connections on it.
    pub fn db(&self) -> &Db {
        match &self.inner {
            Inner::Sqlite { db, .. } | Inner::Postgres { db, .. } => {
                db.as_ref().expect("present until drop")
            }
        }
    }

    /// The override audit's store: its own file on SQLite, the same database
    /// on Postgres.
    pub fn audit(&self) -> &Db {
        match &self.inner {
            Inner::Sqlite { audit, .. } => audit.as_ref().expect("present until drop"),
            Inner::Postgres { db, .. } => db.as_ref().expect("present until drop"),
        }
    }

    /// Where the SQLite file is, for a test that wants to open it a second
    /// way. `None` on Postgres.
    pub fn path(&self) -> Option<&Path> {
        match &self.inner {
            Inner::Sqlite { path, .. } => Some(path),
            Inner::Postgres { .. } => None,
        }
    }

    /// The private schema's name on Postgres, for a test that reads catalogs.
    /// `None` on SQLite.
    pub fn schema(&self) -> Option<&str> {
        match &self.inner {
            Inner::Sqlite { .. } => None,
            Inner::Postgres { schema, .. } => Some(schema),
        }
    }
}

impl Drop for TestDb {
    /// SQLite: closes the pooled connections first, then deletes the files
    /// and their WAL companions. Best effort: a connection the test leaked (a
    /// store cloned into a task still running) keeps the file open on
    /// Windows, and the unique name means the leftover collides with nothing.
    /// It is reported rather than hidden, so a test that leaks shows up as a
    /// message instead of as a full temp directory.
    ///
    /// Postgres: drops the schema from a thread with a runtime of its own, on
    /// a connection of its own. Not through the test's pool: a pooled
    /// connection's driver task lives on the runtime that created it, and a
    /// `#[tokio::test]` runtime is one thread that is blocked right here in
    /// `join`, so a query on that pool would wait for a response nothing can
    /// deliver. (Found by the hive-schema fixture hanging for a minute.)
    fn drop(&mut self) {
        match &mut self.inner {
            Inner::Sqlite {
                db,
                audit,
                path,
                audit_path,
            } => {
                drop(db.take());
                drop(audit.take());
                // The owner files beside the control plane (D39) go with it.
                // Their directory is derived from the control plane's path
                // the same way hive-store derives it, so there is nothing to
                // look up.
                let owners = PathBuf::from(format!("{}-owners", path.with_extension("").display()));
                if owners.is_dir()
                    && let Err(e) = std::fs::remove_dir_all(&owners)
                {
                    eprintln!("testdb: could not delete {}: {e}", owners.display());
                }
                for base in [&*path, &*audit_path] {
                    for suffix in ["", "-wal", "-shm"] {
                        let p = PathBuf::from(format!("{}{suffix}", base.display()));
                        if p.exists()
                            && let Err(e) = std::fs::remove_file(&p)
                        {
                            eprintln!("testdb: could not delete {}: {e}", p.display());
                        }
                    }
                }
            }
            Inner::Postgres { db, url, schema } => {
                if let Some(db) = db.take() {
                    db.close();
                }
                let url = url.clone();
                let schema = schema.clone();
                let _ = std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime for schema drop");
                    rt.block_on(async move {
                        let Ok(admin) = Db::connect(&url).await else {
                            eprintln!("testdb: could not reconnect to drop schema {schema}");
                            return;
                        };
                        if let Ok(c) = admin.conn().await {
                            // The test's own sessions first. A transaction a
                            // test dropped without commit is still open on the
                            // server, holding its locks, until the socket
                            // closes, and the task that closes it lives on the
                            // runtime blocked in `join` below; the DROP would
                            // wait on those locks until its timeout. Found as
                            // a 15 s drop in CI, with the schema left behind.
                            let _ = hive_db::query(
                                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                                  WHERE application_name = ?1 AND pid <> pg_backend_pid()",
                            )
                            .bind(&schema)
                            .fetch_all(&c)
                            .await;
                            // Bounded, so a lock the terminate did not clear
                            // reads as an error naming the schema rather than
                            // a test that never ends.
                            let _ = Db::batch(&c, "SET lock_timeout = '15s'").await;
                            if let Err(e) = Db::batch(
                                &c,
                                &format!("DROP SCHEMA {} CASCADE", hive_db::quote_ident(&schema)),
                            )
                            .await
                            {
                                eprintln!("testdb: drop schema {schema}: {e}");
                            }
                        }
                        admin.close();
                    });
                })
                .join();
            }
        }
    }
}

/// The test Postgres URL when one is set; `None` means SQLite. Under
/// `HIVE_SANDBOX_REQUIRE_DATABASE_TESTS=1` an unset URL is a panic: the
/// environment promised a server, and running the file engine instead would
/// be a green that answered a different question.
fn postgres_url() -> Option<String> {
    match std::env::var(hive_db::TEST_URL_ENV) {
        Ok(u) if !u.trim().is_empty() => Some(u.trim().to_string()),
        _ => {
            if std::env::var(hive_db::TEST_REQUIRE_ENV).is_ok_and(|v| v == "1") {
                panic!(
                    "{} is unset and {}=1 forbids falling back to SQLite",
                    hive_db::TEST_URL_ENV,
                    hive_db::TEST_REQUIRE_ENV
                );
            }
            None
        }
    }
}

/// `t_<test name, lowercased, non-alphanumerics as _>_<8 hex>`, capped so a
/// path stays short and a Postgres identifier stays under 63 bytes. Same
/// shape as the schema names the Go helper made, so a leftover reads the
/// same whichever tree made it.
fn private_name(test_name: &str) -> String {
    let mut prefix = String::from("t_");
    for c in test_name.to_lowercase().chars() {
        prefix.push(if c.is_ascii_alphanumeric() { c } else { '_' });
    }
    prefix.truncate(46);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &suffix[..8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_db::query;

    #[test]
    fn private_names_are_bounded_identifiers() {
        let name = private_name(
            "A Very Long Test Name With Spaces And Punctuation!!! That Keeps Going On",
        );
        assert!(name.len() <= 63, "{name} is {} bytes", name.len());
        assert!(name.starts_with("t_a_very_long_test_name"));
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
    }

    /// Whichever engine the environment names, a fresh store is migrated,
    /// private, and gone afterwards. On Postgres the schema is first on the
    /// search path and nothing landed in `public`.
    #[tokio::test]
    async fn a_fresh_store_is_migrated_and_private() {
        let t = TestDb::new("testdb::a_fresh_store_is_migrated_and_private").await;
        let c = t.db().conn().await.unwrap();
        let versions: Vec<String> = query("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_scalars(&c)
            .await
            .unwrap();
        assert_eq!(versions, vec!["0001".to_string(), "0002".to_string()]);
        let n: i64 = query("SELECT count(*) FROM actors")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let a = t.audit().conn().await.unwrap();
        let n: i64 = query("SELECT count(*) FROM grant_override_audit")
            .fetch_scalar(&a)
            .await
            .unwrap();
        assert_eq!(n, 0);
        match t.engine() {
            Engine::Sqlite => {
                assert!(t.path().unwrap().is_file());
                assert!(t.schema().is_none());
            }
            Engine::Postgres => {
                assert!(t.path().is_none());
                let schema = t.schema().unwrap().to_string();
                let current: String = query("SELECT current_schema()")
                    .fetch_scalar(&c)
                    .await
                    .unwrap();
                assert_eq!(current, schema);
                let in_public: i64 = query(
                    "SELECT count(*) FROM information_schema.tables
                      WHERE table_schema = 'public' AND table_name = 'actors'",
                )
                .fetch_scalar(&c)
                .await
                .unwrap();
                assert_eq!(in_public, 0, "the migration escaped the private schema");
            }
        }
    }

    /// The Postgres fixture leaves nothing behind: the schema is gone once
    /// the `TestDb` is dropped, checked from a second connection.
    #[tokio::test]
    async fn postgres_schema_is_dropped_with_the_fixture() {
        let Some(url) =
            hive_db::test_postgres_url("testdb::postgres_schema_is_dropped_with_the_fixture")
        else {
            return;
        };
        let schema = {
            let t = TestDb::new("testdb::postgres_schema_is_dropped_with_the_fixture").await;
            t.schema().unwrap().to_string()
        };
        let admin = Db::connect(&url).await.unwrap();
        let c = admin.conn().await.unwrap();
        let present: i64 = query("SELECT count(*) FROM pg_namespace WHERE nspname = ?1")
            .bind(&schema)
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(present, 0, "schema {schema} outlived its TestDb");
    }
}

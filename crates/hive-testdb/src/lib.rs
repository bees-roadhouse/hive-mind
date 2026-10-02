//! A private database file per test, for the integration tests.
//!
//! Every test gets its own file under the system temp directory, migrated to
//! the current schema, and deletes it on the way out, so there is no shared
//! mutable fixture and no ordering between tests: run one, run them in
//! parallel, run them in any order. The Go tree's `internal/testdb` and the
//! Postgres port did this with a schema per test on one shared server; with
//! the engine in-process (D38) the file IS the server, so nothing has to be
//! running and nothing has to be exported before `cargo test` means what it
//! says. The `SKIPPED:` line the Postgres fixture printed without its
//! connection string is gone with the precondition it named.
//!
//! The one thing a caller can still set: `HIVE_SANDBOX_TEST_DB_DIR` puts the
//! files somewhere other than the temp directory, for a machine whose temp is
//! a slow or small disk.

use std::path::{Path, PathBuf};

use hive_db::Db;

/// Overrides where test files are created. Optional.
pub const DIR_ENV: &str = "HIVE_SANDBOX_TEST_DB_DIR";

/// One test's private files, migrated: the platform file and the override
/// audit's file beside it, exactly as the daemon lays them out.
pub struct TestDb {
    db: Option<Db>,
    audit: Option<Db>,
    path: PathBuf,
    audit_path: PathBuf,
}

impl TestDb {
    /// Creates and migrates a private file for `test_name`. The name is in the
    /// file name so a leftover from a test killed from outside says which
    /// test it was.
    pub async fn new(test_name: &str) -> Self {
        let dir = match std::env::var(DIR_ENV) {
            Ok(d) if !d.trim().is_empty() => PathBuf::from(d.trim()),
            _ => std::env::temp_dir().join("hive-sandbox-tests"),
        };
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
        let stem = file_stem(test_name);
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
            db: Some(db),
            audit: Some(audit),
            path,
            audit_path,
        }
    }

    /// The platform file, for everything that opens connections on it.
    pub fn db(&self) -> &Db {
        self.db.as_ref().expect("present until drop")
    }

    /// The override audit's file.
    pub fn audit(&self) -> &Db {
        self.audit.as_ref().expect("present until drop")
    }

    /// Where the file is, for a test that wants to open it a second way.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDb {
    /// Closes the pooled connections first, then deletes the files and their
    /// WAL companions. Best effort: a connection the test leaked (a store
    /// cloned into a task still running) keeps the file open on Windows, and
    /// the unique name means the leftover collides with nothing. It is
    /// reported rather than hidden, so a test that leaks shows up as a message
    /// instead of as a full temp directory.
    fn drop(&mut self) {
        drop(self.db.take());
        drop(self.audit.take());
        for base in [&self.path, &self.audit_path] {
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
}

/// `t_<test name, lowercased, non-alphanumerics as _>_<8 hex>`, capped so the
/// path stays short. Same shape as the schema names the Go helper made, so a
/// leftover reads the same whichever tree made it.
fn file_stem(test_name: &str) -> String {
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

    #[test]
    fn file_stems_are_bounded_identifiers() {
        let name =
            file_stem("A Very Long Test Name With Spaces And Punctuation!!! That Keeps Going On");
        assert!(name.len() <= 63, "{name} is {} bytes", name.len());
        assert!(name.starts_with("t_a_very_long_test_name"));
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
    }
}

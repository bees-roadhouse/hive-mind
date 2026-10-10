//! The SQLite backend: `rusqlite`, bundled and vanilla (D38 §1).
//!
//! Everything engine-specific about a file lives here: the pragmas a
//! connection is opened with, `ATTACH` (D39), the checkout pool, and the
//! mapping between [`Value`] and the engine's own value type. The public API
//! in `lib.rs` dispatches here for a [`Db`](crate::Db) opened on a path.
//!
//! The engine is in-process and synchronous, so every call here blocks the
//! calling thread for the engine's duration; the `async` on the public
//! methods is the shape the Postgres backend needs and costs nothing here.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{Error, Result, Row, Value, is_identifier, quote_ident, vec};

/// How long a connection waits on the write lock before `SQLITE_BUSY`. Long
/// enough that a migration or a batch of events in another process is waited
/// out rather than reported; short enough that a wedged writer is noticed.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// How many connections one file may have open at once, counting the ones
/// checked out. Past this a caller waits for a return.
pub(crate) const MAX_OPEN: usize = 32;

/// How many idle connections the pool keeps. More than this and a returned
/// connection is closed instead.
pub(crate) const MAX_IDLE: usize = 8;

/// One engine connection, behind a lock so a reference to it can cross an
/// await point. A statement takes the lock for exactly its own duration;
/// nothing holds it across a wait.
pub(crate) struct SqliteConn {
    inner: Mutex<rusqlite::Connection>,
    path: PathBuf,
    /// The aliases attached on this connection, in attach order. What the
    /// pool detaches before reusing it; what a caller reads to attach once.
    attached: Mutex<Vec<String>>,
}

impl SqliteConn {
    pub(crate) fn open(path: &Path) -> Result<SqliteConn> {
        // Before the first connection in this process, and a no-op after.
        vec::register();
        let c = rusqlite::Connection::open(path)?;
        c.busy_timeout(BUSY_TIMEOUT)?;
        // Foreign keys are per connection in SQLite and default to off, so a
        // connection that skipped this would silently ignore every
        // `REFERENCES` in the schema; that is why there is no other way to
        // open one.
        c.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")?;
        Ok(SqliteConn {
            inner: Mutex::new(c),
            path: path.to_path_buf(),
            attached: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn attach(&self, path: &Path, alias: &str) -> Result<()> {
        if !is_identifier(alias) {
            return Err(Error::Other(format!("{alias:?} is not an identifier")));
        }
        {
            let held = self.attached.lock();
            if held.iter().any(|a| a == alias) {
                return Ok(());
            }
        }
        let sql = format!("ATTACH DATABASE ?1 AS {}", quote_ident(alias));
        self.inner
            .lock()
            .execute(&sql, [path.to_string_lossy().as_ref()])?;
        self.attached.lock().push(alias.to_string());
        Ok(())
    }

    pub(crate) fn attached(&self) -> Vec<String> {
        self.attached.lock().clone()
    }

    pub(crate) fn detach(&self, alias: &str) -> Result<()> {
        let sql = format!("DETACH DATABASE {}", quote_ident(alias));
        self.inner.lock().execute_batch(&sql)?;
        self.attached.lock().retain(|a| a != alias);
        Ok(())
    }

    pub(crate) fn detach_all(&self) -> Result<()> {
        for a in self.attached() {
            self.detach(&a)?;
        }
        Ok(())
    }

    pub(crate) fn is_autocommit(&self) -> bool {
        self.inner.lock().is_autocommit()
    }

    pub(crate) fn execute_batch(&self, sql: &str) -> Result<()> {
        self.inner.lock().execute_batch(sql)?;
        Ok(())
    }

    pub(crate) fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        let conn = self.inner.lock();
        let mut stmt = conn.prepare(sql)?;
        let bound: Vec<rusqlite::types::Value> = params.iter().map(to_engine).collect();
        let n = stmt.execute(rusqlite::params_from_iter(bound.iter()))?;
        Ok(n as u64)
    }

    pub(crate) fn fetch(
        &self,
        sql: &str,
        params: &[Value],
        limit: Option<usize>,
    ) -> Result<Vec<Row>> {
        let conn = self.inner.lock();
        let mut stmt = conn.prepare(sql)?;
        let columns: Arc<Vec<String>> =
            Arc::new(stmt.column_names().into_iter().map(String::from).collect());
        let n = columns.len();
        let bound: Vec<rusqlite::types::Value> = params.iter().map(to_engine).collect();
        let mut rows = stmt.query(rusqlite::params_from_iter(bound.iter()))?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let mut values = Vec::with_capacity(n);
            for i in 0..n {
                values.push(from_engine(r.get::<_, rusqlite::types::Value>(i)?));
            }
            out.push(Row::new(columns.clone(), values));
            if limit.is_some_and(|l| out.len() >= l) {
                break;
            }
        }
        Ok(out)
    }
}

/// How a [`Value`] is bound on this engine: the typed variants collapse to
/// the D38 §3 column representations (a uuid is text, a time is integer
/// microseconds, a bool is 0/1, json is text), because that is what the
/// schema's columns are.
fn to_engine(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as E;
    match v {
        Value::Null => E::Null,
        Value::Integer(i) => E::Integer(*i),
        Value::Real(f) => E::Real(*f),
        Value::Text(s) => E::Text(s.clone()),
        Value::Blob(b) => E::Blob(b.clone()),
        Value::Bool(b) => E::Integer(*b as i64),
        Value::Uuid(u) => E::Text(u.to_string()),
        Value::Timestamp(t) => E::Integer(crate::micros(*t)),
        Value::Json(j) => E::Text(j.to_string()),
    }
}

/// The engine's value as read: SQLite has no typed uuid, time, bool or json,
/// so these come back as the text and integers the columns hold, and the
/// typed [`FromSql`](crate::FromSql) impls accept both shapes.
fn from_engine(v: rusqlite::types::Value) -> Value {
    use rusqlite::types::Value as E;
    match v {
        E::Null => Value::Null,
        E::Integer(i) => Value::Integer(i),
        E::Real(f) => Value::Real(f),
        E::Text(s) => Value::Text(s),
        E::Blob(b) => Value::Blob(b),
    }
}

/// One file's checkout pool.
pub(crate) struct Pool {
    path: PathBuf,
    idle: Mutex<Vec<SqliteConn>>,
    slots: Arc<Semaphore>,
}

impl Pool {
    /// Opens (creating if needed) the file at `path`, creating its parent
    /// directory, and switches it to WAL so readers never block the writer.
    /// WAL is a property of the file and persists; the per-connection pragmas
    /// are set when a connection is opened for the pool.
    pub(crate) async fn open(path: PathBuf) -> Result<Pool> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let pool = Pool {
            path,
            idle: Mutex::new(Vec::new()),
            slots: Arc::new(Semaphore::new(MAX_OPEN)),
        };
        let (c, _permit) = pool.checkout().await?;
        // PRAGMA journal_mode returns a row, so it is a query rather than an
        // execute; the row says which mode the file ended up in.
        let mode: String = c
            .fetch("PRAGMA journal_mode = WAL", &[], Some(1))?
            .into_iter()
            .next()
            .ok_or(Error::NoRows)?
            .try_get_at(0)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Other(format!(
                "{}: journal_mode is {mode:?}, wanted wal",
                pool.path.display()
            )));
        }
        pool.put_back(c);
        Ok(pool)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// A connection from the pool, or a fresh one, with the permit that holds
    /// its slot. Waits when the file's cap of open connections is reached.
    pub(crate) async fn checkout(&self) -> Result<(SqliteConn, OwnedSemaphorePermit)> {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Pool("connection pool closed".into()))?;
        let reused = self.idle.lock().pop();
        let c = match reused {
            Some(c) => c,
            None => SqliteConn::open(&self.path)?,
        };
        Ok((c, permit))
    }

    /// Returns a connection to the idle list, or closes it when the list is
    /// full. The caller has already decided the connection is clean.
    pub(crate) fn put_back(&self, c: SqliteConn) {
        let mut idle = self.idle.lock();
        if idle.len() < MAX_IDLE {
            idle.push(c);
        }
    }

    /// Refuses every later checkout and drops the idle connections.
    pub(crate) fn close(&self) {
        self.slots.close();
        self.idle.lock().clear();
    }

    #[cfg(test)]
    pub(crate) fn idle_len(&self) -> usize {
        self.idle.lock().len()
    }

    #[cfg(test)]
    pub(crate) fn idle_all_clean(&self) -> bool {
        self.idle.lock().iter().all(|c| c.attached().is_empty())
    }

    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize {
        self.slots.available_permits()
    }
}

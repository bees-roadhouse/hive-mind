//! The one way the host opens a database file (D38).
//!
//! libSQL is SQLite with additions, reached through the `libsql` crate. This
//! crate wraps it just enough that the rest of the workspace speaks one shape:
//! a [`Db`] is a file, a [`Conn`] is a checked-out connection on it with the
//! pragmas the schema assumes already set, a [`Transaction`] is `BEGIN
//! IMMEDIATE` on one, and [`query`] binds typed values and reads typed columns
//! by name. There is deliberately no query helper that knows about grants: the
//! predicate lives in `hive-store`, and nothing here reads policy.
//!
//! Three facts every caller relies on and should know it relies on:
//!
//! - **One writer at a time per file.** A write transaction is `BEGIN
//!   IMMEDIATE`, so two writers serialise in the engine rather than racing;
//!   `busy_timeout` makes the loser wait rather than fail. The claim patterns
//!   the Postgres tree wrote with `SKIP LOCKED` are `UPDATE ... RETURNING`
//!   here and are correct for that reason alone.
//! - **Time is an integer.** Every timestamp column is microseconds since the
//!   Unix epoch, UTC. [`now`] is the clock the host binds; [`NOW_SQL`] is the
//!   expression a column default or a trigger uses. They read the same clock
//!   at different resolutions (microseconds and milliseconds), and nothing
//!   orders rows across the two.
//! - **Connections are pooled and few.** A [`Conn`] goes back to its file's
//!   pool when dropped, and the number open at once is capped. Measured, not
//!   assumed: the engine as built crashes the process once a few hundred
//!   connections are open on one WAL file (reproduced on Windows with bare
//!   `libsql`, no pragmas, 600 connections; 200 was fine), and opening one per
//!   statement crashed intermittently on the way there. The cap keeps the
//!   daemon far from that cliff and the reuse keeps a statement from paying
//!   an open. A connection is keyed on nothing a caller could forget
//!   (invariant 14): every one has the same pragmas and no state outlives a
//!   checkout, because one mid-transaction is closed rather than returned.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
pub use libsql::{Connection, TransactionBehavior, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

/// The SQL expression for "now" in the schema's unit. Millisecond resolution,
/// which is what `julianday('now')` gives; a host write binds [`now`] instead.
pub const NOW_SQL: &str = "(CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))";

/// The SQL expression that mints a v4 UUID as lowercase hyphenated text, for
/// column defaults. The host binds [`Uuid::new_v4`] on its own inserts; this
/// exists so a raw insert from a test or a `sqlite3` session gets a real id.
pub const UUID_SQL: &str = "(lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' \
    || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) \
    || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6))))";

/// How long a connection waits on the write lock before `SQLITE_BUSY`. Long
/// enough that a migration or a batch of events in another process is waited
/// out rather than reported; short enough that a wedged writer is noticed.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// How many connections one file may have open at once, counting the ones
/// checked out. Past this a caller waits for a return. Well under the few
/// hundred where the engine falls over, and well over what one daemon's
/// handlers and workers hold at a time.
const MAX_OPEN: usize = 32;

/// How many idle connections the pool keeps. More than this and a returned
/// connection is closed instead.
const MAX_IDLE: usize = 8;

/// Everything this crate can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Sqlite(#[from] libsql::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("no rows")]
    NoRows,
    #[error("no such column {0:?}")]
    NoColumn(String),
    #[error("column {0:?}: {1}")]
    Decode(String, String),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// The engine's message, for callers that match on which trigger or
    /// constraint refused a write. Tests assert on these; the daemon never
    /// shows one to a client.
    pub fn message(&self) -> String {
        match self {
            Error::Sqlite(libsql::Error::SqliteFailure(_, msg)) => msg.clone(),
            other => other.to_string(),
        }
    }

    /// The primary SQLite result code, when the engine produced one.
    pub fn sqlite_code(&self) -> Option<i32> {
        match self {
            Error::Sqlite(libsql::Error::SqliteFailure(code, _)) => Some(*code & 0xff),
            _ => None,
        }
    }

    /// Whether a constraint, a `RAISE(ABORT)` in a trigger included, refused
    /// the write. Both are `SQLITE_CONSTRAINT`; the message says which.
    pub fn is_constraint(&self) -> bool {
        self.sqlite_code() == Some(19)
    }

    /// Whether a `UNIQUE` or primary-key constraint specifically refused it.
    pub fn is_unique_violation(&self) -> bool {
        self.is_constraint() && self.message().contains("UNIQUE constraint failed")
    }

    /// Whether the engine reported `SQLITE_BUSY`: the write lock was held past
    /// the busy timeout.
    pub fn is_busy(&self) -> bool {
        self.sqlite_code() == Some(5)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

struct Inner {
    db: libsql::Database,
    path: PathBuf,
    idle: parking_lot::Mutex<Vec<Connection>>,
    slots: Arc<Semaphore>,
}

/// One database file. Cheap to clone; every clone is the same file and the
/// same pool.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("path", &self.inner.path).finish()
    }
}

impl Db {
    /// Opens (creating if needed) the file at `path`, creating its parent
    /// directory, and switches it to WAL so readers never block the writer.
    /// WAL is a property of the file and persists; the per-connection pragmas
    /// are set when a connection is opened for the pool.
    pub async fn open(path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let inner = libsql::Builder::new_local(&path).build().await?;
        let db = Db {
            inner: Arc::new(Inner {
                db: inner,
                path,
                idle: parking_lot::Mutex::new(Vec::new()),
                slots: Arc::new(Semaphore::new(MAX_OPEN)),
            }),
        };
        let c = db.conn().await?;
        // PRAGMA journal_mode returns a row, so it is a query rather than an
        // execute; the row says which mode the file ended up in.
        let mode: String = query("PRAGMA journal_mode = WAL").fetch_scalar(&c).await?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Other(format!(
                "{}: journal_mode is {mode:?}, wanted wal",
                db.inner.path.display()
            )));
        }
        Ok(db)
    }

    /// The file this is.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// A connection from the pool, or a fresh one with foreign keys enforced
    /// and the busy timeout set. Foreign keys are per connection in SQLite and
    /// default to off, so a connection that skipped this would silently ignore
    /// every `REFERENCES` in the schema; that is why there is no other way to
    /// get one. Waits when the file's cap of open connections is reached.
    pub async fn conn(&self) -> Result<Conn> {
        let permit = self
            .inner
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Other("connection pool closed".into()))?;
        let reused = self.inner.idle.lock().pop();
        let c = match reused {
            Some(c) => c,
            None => {
                let c = self.inner.db.connect()?;
                c.busy_timeout(BUSY_TIMEOUT)?;
                c.execute("PRAGMA foreign_keys = ON", ()).await?;
                c.execute("PRAGMA synchronous = NORMAL", ()).await?;
                c
            }
        };
        Ok(Conn {
            c: Some(c),
            pool: self.inner.clone(),
            _permit: permit,
        })
    }

    /// A write transaction on a pooled connection: `BEGIN IMMEDIATE`, so the
    /// write lock is taken now rather than at the first write, and two writers
    /// queue in the engine instead of one failing mid-transaction with BUSY.
    /// Dropping the transaction without committing rolls it back.
    pub async fn begin(&self) -> Result<Transaction> {
        let conn = self.conn().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        Ok(Transaction { tx: Some(tx), conn })
    }

    /// Runs many statements. Migrations and DDL only; nothing here binds.
    pub async fn batch(c: &Connection, sql: &str) -> Result<()> {
        c.execute_batch(sql).await?;
        Ok(())
    }
}

/// A checked-out connection. Derefs to the engine's connection; goes back to
/// the pool when dropped unless it is mid-transaction, in which case it is
/// closed, because a connection with state is not interchangeable with one
/// without.
pub struct Conn {
    c: Option<Connection>,
    pool: Arc<Inner>,
    _permit: OwnedSemaphorePermit,
}

impl Deref for Conn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.c.as_ref().expect("connection present until drop")
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let Some(c) = self.c.take() else {
            return;
        };
        if !c.is_autocommit() {
            return;
        }
        let mut idle = self.pool.idle.lock();
        if idle.len() < MAX_IDLE {
            idle.push(c);
        }
    }
}

/// `BEGIN IMMEDIATE` on a pooled connection. Derefs to the connection so
/// statements run inside it; `commit` ends it; dropping it rolls it back and
/// returns the connection to the pool afterwards (fields drop in order).
pub struct Transaction {
    tx: Option<libsql::Transaction>,
    conn: Conn,
}

impl Deref for Transaction {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl Transaction {
    pub async fn commit(mut self) -> Result<()> {
        if let Some(tx) = self.tx.take() {
            tx.commit().await?;
        }
        Ok(())
    }

    pub async fn rollback(mut self) -> Result<()> {
        if let Some(tx) = self.tx.take() {
            tx.rollback().await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Time.
// ---------------------------------------------------------------------------

/// The host clock in the schema's unit and resolution: UTC, truncated to the
/// microsecond so a value that went through a column comes back equal.
pub fn now() -> DateTime<Utc> {
    let t = Utc::now();
    Utc.timestamp_micros(t.timestamp_micros())
        .single()
        .unwrap_or(t)
}

/// Microseconds since the epoch, the column representation.
pub fn micros(t: DateTime<Utc>) -> i64 {
    t.timestamp_micros()
}

/// The column representation back to a time. `None` only for a value outside
/// chrono's range, which no clock produces.
pub fn from_micros(us: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_micros(us).single()
}

// ---------------------------------------------------------------------------
// Binding.
// ---------------------------------------------------------------------------

/// A value that can be bound to a `?N` placeholder.
pub trait ToSql {
    fn to_sql(self) -> Value;
}

impl ToSql for Value {
    fn to_sql(self) -> Value {
        self
    }
}
impl ToSql for &str {
    fn to_sql(self) -> Value {
        Value::Text(self.to_string())
    }
}
impl ToSql for String {
    fn to_sql(self) -> Value {
        Value::Text(self)
    }
}
impl ToSql for &String {
    fn to_sql(self) -> Value {
        Value::Text(self.clone())
    }
}
impl ToSql for i64 {
    fn to_sql(self) -> Value {
        Value::Integer(self)
    }
}
impl ToSql for i32 {
    fn to_sql(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl ToSql for i16 {
    fn to_sql(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl ToSql for u32 {
    fn to_sql(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl ToSql for u64 {
    fn to_sql(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl ToSql for usize {
    fn to_sql(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl ToSql for f64 {
    fn to_sql(self) -> Value {
        Value::Real(self)
    }
}
impl ToSql for bool {
    fn to_sql(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl ToSql for Uuid {
    fn to_sql(self) -> Value {
        Value::Text(self.to_string())
    }
}
impl ToSql for &Uuid {
    fn to_sql(self) -> Value {
        Value::Text(self.to_string())
    }
}
impl ToSql for DateTime<Utc> {
    fn to_sql(self) -> Value {
        Value::Integer(micros(self))
    }
}
impl ToSql for &DateTime<Utc> {
    fn to_sql(self) -> Value {
        Value::Integer(micros(*self))
    }
}
impl ToSql for serde_json::Value {
    fn to_sql(self) -> Value {
        Value::Text(self.to_string())
    }
}
impl ToSql for &serde_json::Value {
    fn to_sql(self) -> Value {
        Value::Text(self.to_string())
    }
}
impl ToSql for Vec<u8> {
    fn to_sql(self) -> Value {
        Value::Blob(self)
    }
}
impl ToSql for &[u8] {
    fn to_sql(self) -> Value {
        Value::Blob(self.to_vec())
    }
}
impl ToSql for &Vec<u8> {
    fn to_sql(self) -> Value {
        Value::Blob(self.clone())
    }
}
impl<T: ToSql> ToSql for Option<T> {
    fn to_sql(self) -> Value {
        match self {
            Some(v) => v.to_sql(),
            None => Value::Null,
        }
    }
}
impl<T: ToSql + Clone> ToSql for &Option<T> {
    fn to_sql(self) -> Value {
        match self {
            Some(v) => v.clone().to_sql(),
            None => Value::Null,
        }
    }
}

// ---------------------------------------------------------------------------
// Reading.
// ---------------------------------------------------------------------------

/// A value read out of a column.
pub trait FromSql: Sized {
    fn from_sql(v: &Value) -> std::result::Result<Self, String>;
}

fn wrong(expected: &str, v: &Value) -> String {
    let got = match v {
        Value::Null => "NULL",
        Value::Integer(_) => "an integer",
        Value::Real(_) => "a real",
        Value::Text(_) => "text",
        Value::Blob(_) => "a blob",
    };
    format!("expected {expected}, got {got}")
}

impl FromSql for Value {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        Ok(v.clone())
    }
}
impl FromSql for i64 {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Integer(i) => Ok(*i),
            other => Err(wrong("an integer", other)),
        }
    }
}
impl FromSql for i32 {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        let i = i64::from_sql(v)?;
        i32::try_from(i).map_err(|_| format!("{i} does not fit an i32"))
    }
}
impl FromSql for u64 {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        let i = i64::from_sql(v)?;
        u64::try_from(i).map_err(|_| format!("{i} is negative"))
    }
}
impl FromSql for u32 {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        let i = i64::from_sql(v)?;
        u32::try_from(i).map_err(|_| format!("{i} does not fit a u32"))
    }
}
impl FromSql for f64 {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Real(f) => Ok(*f),
            Value::Integer(i) => Ok(*i as f64),
            other => Err(wrong("a number", other)),
        }
    }
}
impl FromSql for bool {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Integer(i) => Ok(*i != 0),
            other => Err(wrong("a 0/1 integer", other)),
        }
    }
}
impl FromSql for String {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            other => Err(wrong("text", other)),
        }
    }
}
impl FromSql for Vec<u8> {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Blob(b) => Ok(b.clone()),
            Value::Text(s) => Ok(s.as_bytes().to_vec()),
            other => Err(wrong("a blob", other)),
        }
    }
}
impl FromSql for Uuid {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        let s = String::from_sql(v)?;
        Uuid::parse_str(&s).map_err(|e| format!("{s:?} is not a uuid: {e}"))
    }
}
impl FromSql for DateTime<Utc> {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        let us = i64::from_sql(v)?;
        from_micros(us).ok_or_else(|| format!("{us} is not a timestamp"))
    }
}
impl FromSql for serde_json::Value {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Text(s) => serde_json::from_str(s).map_err(|e| format!("not json: {e}")),
            Value::Blob(b) => serde_json::from_slice(b).map_err(|e| format!("not json: {e}")),
            other => Err(wrong("json text", other)),
        }
    }
}
impl<T: FromSql> FromSql for Option<T> {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Null => Ok(None),
            other => T::from_sql(other).map(Some),
        }
    }
}

/// One row, materialised, with its column names. Reads are by name so a query
/// that gains a column does not shift every read after it.
#[derive(Clone, Debug)]
pub struct Row {
    columns: Arc<Vec<String>>,
    values: Vec<Value>,
}

impl Row {
    fn index(&self, col: &str) -> Result<usize> {
        self.columns
            .iter()
            .position(|c| c == col)
            .ok_or_else(|| Error::NoColumn(col.to_string()))
    }

    /// Reads a column by name. Panics on a missing column or a value of the
    /// wrong shape, exactly as a typed read of a known query should: both are
    /// bugs in the query, not conditions a caller can handle.
    pub fn get<T: FromSql>(&self, col: &str) -> T {
        match self.try_get(col) {
            Ok(v) => v,
            Err(e) => panic!("{e}"),
        }
    }

    pub fn try_get<T: FromSql>(&self, col: &str) -> Result<T> {
        let i = self.index(col)?;
        T::from_sql(&self.values[i]).map_err(|e| Error::Decode(col.to_string(), e))
    }

    /// Reads a column by position.
    pub fn get_at<T: FromSql>(&self, i: usize) -> T {
        match self.try_get_at(i) {
            Ok(v) => v,
            Err(e) => panic!("{e}"),
        }
    }

    pub fn try_get_at<T: FromSql>(&self, i: usize) -> Result<T> {
        let v = self
            .values
            .get(i)
            .ok_or_else(|| Error::NoColumn(format!("#{i}")))?;
        T::from_sql(v).map_err(|e| Error::Decode(format!("#{i}"), e))
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// A statement with its bound values. Placeholders are `?1`, `?2`, ... and
/// bind in that order; `bind` is called once per placeholder in sequence.
pub struct Query<'q> {
    sql: &'q str,
    params: Vec<Value>,
}

/// Starts a statement.
pub fn query(sql: &str) -> Query<'_> {
    Query {
        sql,
        params: Vec::new(),
    }
}

impl<'q> Query<'q> {
    pub fn bind<T: ToSql>(mut self, v: T) -> Self {
        self.params.push(v.to_sql());
        self
    }

    fn params(self) -> (&'q str, libsql::params::Params) {
        let p = if self.params.is_empty() {
            libsql::params::Params::None
        } else {
            libsql::params::Params::Positional(self.params)
        };
        (self.sql, p)
    }

    /// Runs a statement that returns no rows and reports how many it changed.
    /// A statement with `RETURNING` wants `fetch_*`; the engine refuses it here.
    pub async fn execute(self, c: &Connection) -> Result<u64> {
        let (sql, p) = self.params();
        Ok(c.execute(sql, p).await?)
    }

    pub async fn fetch_all(self, c: &Connection) -> Result<Vec<Row>> {
        let (sql, p) = self.params();
        let mut rows = c.query(sql, p).await?;
        let n = rows.column_count();
        let columns: Arc<Vec<String>> = Arc::new(
            (0..n)
                .map(|i| rows.column_name(i).unwrap_or("").to_string())
                .collect(),
        );
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            let mut values = Vec::with_capacity(n as usize);
            for i in 0..n {
                values.push(r.get_value(i)?);
            }
            out.push(Row {
                columns: columns.clone(),
                values,
            });
        }
        Ok(out)
    }

    pub async fn fetch_optional(self, c: &Connection) -> Result<Option<Row>> {
        let (sql, p) = self.params();
        let mut rows = c.query(sql, p).await?;
        let n = rows.column_count();
        let columns: Arc<Vec<String>> = Arc::new(
            (0..n)
                .map(|i| rows.column_name(i).unwrap_or("").to_string())
                .collect(),
        );
        let Some(r) = rows.next().await? else {
            return Ok(None);
        };
        let mut values = Vec::with_capacity(n as usize);
        for i in 0..n {
            values.push(r.get_value(i)?);
        }
        Ok(Some(Row { columns, values }))
    }

    pub async fn fetch_one(self, c: &Connection) -> Result<Row> {
        self.fetch_optional(c).await?.ok_or(Error::NoRows)
    }

    /// The first column of the first row.
    pub async fn fetch_scalar<T: FromSql>(self, c: &Connection) -> Result<T> {
        self.fetch_one(c).await?.try_get_at(0)
    }

    pub async fn fetch_scalar_optional<T: FromSql>(self, c: &Connection) -> Result<Option<T>> {
        match self.fetch_optional(c).await? {
            Some(r) => Ok(Some(r.try_get_at(0)?)),
            None => Ok(None),
        }
    }

    /// The first column of every row.
    pub async fn fetch_scalars<T: FromSql>(self, c: &Connection) -> Result<Vec<T>> {
        self.fetch_all(c)
            .await?
            .iter()
            .map(|r| r.try_get_at(0))
            .collect()
    }
}

/// Double-quotes an identifier for DDL. Callers validate the name first; the
/// doubling is the last line, not the only one.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Single-quotes a string literal for embedding in an expression.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("t.db")).await.expect("open");
        (dir, db)
    }

    #[tokio::test]
    async fn opens_in_wal_with_foreign_keys_on() {
        let (_d, db) = temp().await;
        let c = db.conn().await.unwrap();
        let fk: i64 = query("PRAGMA foreign_keys").fetch_scalar(&c).await.unwrap();
        assert_eq!(fk, 1);
        let mode: String = query("PRAGMA journal_mode").fetch_scalar(&c).await.unwrap();
        assert_eq!(mode, "wal");
    }

    #[tokio::test]
    async fn binds_and_reads_every_type_by_name() {
        let (_d, db) = temp().await;
        let c = db.conn().await.unwrap();
        Db::batch(
            &c,
            "CREATE TABLE t (id TEXT, n INTEGER, f REAL, b INTEGER, j TEXT, at INTEGER, raw BLOB, maybe TEXT)",
        )
        .await
        .unwrap();
        let id = Uuid::new_v4();
        let at = now();
        let j = serde_json::json!({"a": [1, 2]});
        query("INSERT INTO t VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")
            .bind(id)
            .bind(7i64)
            .bind(1.5f64)
            .bind(true)
            .bind(&j)
            .bind(at)
            .bind(vec![1u8, 2, 3])
            .bind(None::<String>)
            .execute(&c)
            .await
            .unwrap();
        let r = query("SELECT * FROM t").fetch_one(&c).await.unwrap();
        assert_eq!(r.get::<Uuid>("id"), id);
        assert_eq!(r.get::<i64>("n"), 7);
        assert_eq!(r.get::<i32>("n"), 7);
        assert_eq!(r.get::<f64>("f"), 1.5);
        assert!(r.get::<bool>("b"));
        assert_eq!(r.get::<serde_json::Value>("j"), j);
        assert_eq!(r.get::<DateTime<Utc>>("at"), at);
        assert_eq!(r.get::<Vec<u8>>("raw"), vec![1, 2, 3]);
        assert_eq!(r.get::<Option<String>>("maybe"), None);
        assert!(r.try_get::<String>("nope").is_err());
        assert!(r.try_get::<String>("n").is_err(), "an integer is not text");
    }

    #[tokio::test]
    async fn returning_goes_through_fetch_and_execute_counts() {
        let (_d, db) = temp().await;
        let c = db.conn().await.unwrap();
        Db::batch(&c, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .await
            .unwrap();
        let id: i64 = query("INSERT INTO t (v) VALUES (?1) RETURNING id")
            .bind("x")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(id, 1);
        let n = query("UPDATE t SET v = ?1").bind("y").execute(&c).await.unwrap();
        assert_eq!(n, 1);
        assert!(matches!(
            query("SELECT v FROM t WHERE id = 99").fetch_one(&c).await,
            Err(Error::NoRows)
        ));
    }

    #[tokio::test]
    async fn a_trigger_refusal_is_a_constraint_with_its_message() {
        let (_d, db) = temp().await;
        let c = db.conn().await.unwrap();
        Db::batch(
            &c,
            "CREATE TABLE t (n INTEGER);
             CREATE TRIGGER no_neg BEFORE INSERT ON t BEGIN
                 SELECT RAISE(ABORT, 'n must not be negative') WHERE NEW.n < 0;
             END;",
        )
        .await
        .unwrap();
        let err = query("INSERT INTO t VALUES (-1)")
            .execute(&c)
            .await
            .unwrap_err();
        assert!(err.is_constraint(), "{err:?}");
        assert!(!err.is_unique_violation());
        assert_eq!(err.message(), "n must not be negative");
    }

    #[tokio::test]
    async fn dropped_transaction_rolls_back_and_immediate_serialises() {
        let (_d, db) = temp().await;
        let c = db.conn().await.unwrap();
        Db::batch(&c, "CREATE TABLE t (n INTEGER)").await.unwrap();
        {
            let tx = db.begin().await.unwrap();
            query("INSERT INTO t VALUES (1)").execute(&tx).await.unwrap();
            // dropped, not committed
        }
        let n: i64 = query("SELECT count(*) FROM t").fetch_scalar(&c).await.unwrap();
        assert_eq!(n, 0);
        let tx = db.begin().await.unwrap();
        query("INSERT INTO t VALUES (1)").execute(&tx).await.unwrap();
        tx.commit().await.unwrap();
        let n: i64 = query("SELECT count(*) FROM t").fetch_scalar(&c).await.unwrap();
        assert_eq!(n, 1);
    }

    /// The pool: a returned connection is the one handed out next, a
    /// connection dropped mid-transaction is not, and the cap holds. The
    /// crash this guards against needs hundreds of connections; the test
    /// checks the mechanism rather than reproducing the crash.
    #[tokio::test]
    async fn connections_are_reused_and_capped() {
        let (_d, db) = temp().await;
        {
            let c = db.conn().await.unwrap();
            query("CREATE TABLE t (n INTEGER)").execute(&c).await.unwrap();
        }
        assert_eq!(db.inner.idle.lock().len(), 1, "one idle after return");
        {
            let _a = db.conn().await.unwrap();
            assert_eq!(db.inner.idle.lock().len(), 0, "the idle one was reused");
            let _b = db.conn().await.unwrap();
            assert_eq!(db.inner.slots.available_permits(), MAX_OPEN - 2);
        }
        assert_eq!(db.inner.idle.lock().len(), 2);
        assert_eq!(db.inner.slots.available_permits(), MAX_OPEN);
        // A connection left inside a transaction is closed, not pooled.
        {
            let c = db.conn().await.unwrap();
            c.execute("BEGIN", ()).await.unwrap();
            query("INSERT INTO t VALUES (1)").execute(&c).await.unwrap();
        }
        assert_eq!(db.inner.idle.lock().len(), 1, "a mid-transaction connection was pooled");
        let c = db.conn().await.unwrap();
        let n: i64 = query("SELECT count(*) FROM t").fetch_scalar(&c).await.unwrap();
        assert_eq!(n, 0, "the abandoned transaction leaked a row");
        // Many sequential checkouts never exceed the idle cap.
        drop(c);
        let mut held = Vec::new();
        for _ in 0..(MAX_IDLE + 4) {
            held.push(db.conn().await.unwrap());
        }
        drop(held);
        assert!(db.inner.idle.lock().len() <= MAX_IDLE);
    }

    #[tokio::test]
    async fn defaults_mint_a_uuid_and_a_time() {
        let (_d, db) = temp().await;
        let c = db.conn().await.unwrap();
        Db::batch(
            &c,
            &format!("CREATE TABLE t (id TEXT PRIMARY KEY DEFAULT {UUID_SQL}, at INTEGER NOT NULL DEFAULT {NOW_SQL})"),
        )
        .await
        .unwrap();
        query("INSERT INTO t DEFAULT VALUES").execute(&c).await.unwrap();
        let r = query("SELECT id, at FROM t").fetch_one(&c).await.unwrap();
        let id: Uuid = r.get("id");
        assert_eq!(id.get_version_num(), 4);
        let at: DateTime<Utc> = r.get("at");
        let skew = (now() - at).num_seconds().abs();
        assert!(skew < 5, "default clock is {skew}s off the host clock");
    }

    #[test]
    fn now_round_trips_through_micros() {
        let t = now();
        assert_eq!(from_micros(micros(t)), Some(t));
    }
}

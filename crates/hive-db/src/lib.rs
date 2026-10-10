//! The one way the host opens a database (D38, D43).
//!
//! Two engines behind one shape. A [`Db`] is a SQLite file or a Postgres
//! database; a [`Conn`] is a checked-out connection on it; a [`Transaction`]
//! is `BEGIN IMMEDIATE` on the file or `BEGIN` on the server; and [`query`]
//! binds typed values and reads typed columns by name on either. There is
//! deliberately no query helper that knows about grants: the predicate lives
//! in `hive-store`, and nothing here reads policy.
//!
//! The SQLite engine is vanilla, bundled, through `rusqlite`, and that is a
//! measured change rather than a preference: the libSQL fork's crate
//! (0.9.30 and 0.10.0-pre.4, MSVC and GNU builds) corrupts the heap on
//! Windows once roughly ten connections are open in one process. The
//! Postgres engine is `tokio-postgres` behind `deadpool-postgres` (D43 §7);
//! `postgres.rs` says what the seam translates and what it cannot. Which
//! engine a `Db` is comes off how it was opened: [`Db::open`] takes a path,
//! [`Db::connect`] a URL, and [`Db::engine`] says which, for the few callers
//! that have to compose engine-specific SQL ([`Engine::now_sql`]).
//!
//! Three facts every caller relies on and should know it relies on:
//!
//! - **A connection mid-transaction is never pooled.** On both engines a
//!   checkout that goes back with a transaction open is closed instead
//!   (invariant 14: the next caller did not ask for that state). On SQLite a
//!   write transaction is `BEGIN IMMEDIATE`, one writer at a time per file,
//!   `busy_timeout` making the loser wait rather than fail. On Postgres
//!   writers run concurrently and a failed statement aborts the transaction
//!   it is in, which SQLite does not do; `postgres.rs` names that.
//! - **Time is a timestamp.** Every timestamp column holds microseconds since
//!   the Unix epoch, UTC: as an integer on SQLite, as `timestamptz` on
//!   Postgres. [`now`] is the clock the host binds, truncated to the
//!   microsecond so a value that went through a column comes back equal;
//!   [`Engine::now_sql`] is the expression a column default or a trigger
//!   uses. A `DateTime` binds and reads as one on either engine.
//! - **Connections are pooled and few.** SQLite's are in-process and every
//!   call blocks the calling thread; Postgres's are sockets and every call
//!   awaits. A [`Conn`] goes back to its pool when dropped and the number
//!   open at once is capped per `Db`.

mod postgres;
mod sqlite;
mod vec;

use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

/// The SQLite expression for "now" in the schema's unit. Millisecond
/// resolution, which is what `julianday('now')` gives; a host write binds
/// [`now`] instead. [`Engine::now_sql`] is what a caller composing DDL for
/// either engine should use; this constant is the SQLite text it returns.
pub const NOW_SQL: &str = "(CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))";

/// The SQLite expression that mints a v4 UUID as lowercase hyphenated text,
/// for column defaults. The host binds [`Uuid::new_v4`] on its own inserts;
/// this exists so a raw insert from a test or a `sqlite3` session gets a
/// real id. [`Engine::uuid_sql`] is the engine-neutral way to ask for it.
pub const UUID_SQL: &str = "(lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' \
    || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) \
    || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6))))";

/// Which engine a [`Db`] is. Most callers never ask; the ones that compose
/// engine-specific SQL (a column default, a trigger body) ask once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Sqlite,
    Postgres,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Sqlite => "sqlite",
            Engine::Postgres => "postgres",
        }
    }

    /// The expression for "now" in the engine's timestamp representation.
    pub fn now_sql(self) -> &'static str {
        match self {
            Engine::Sqlite => NOW_SQL,
            Engine::Postgres => "now()",
        }
    }

    /// The expression that mints a v4 UUID in the engine's uuid
    /// representation.
    pub fn uuid_sql(self) -> &'static str {
        match self {
            Engine::Sqlite => UUID_SQL,
            Engine::Postgres => "gen_random_uuid()",
        }
    }

    /// The statement [`Db::begin`] runs. SQLite takes the write lock up front
    /// so two writers queue instead of one failing mid-transaction; Postgres
    /// has no such mode and does not need one.
    pub fn begin_sql(self) -> &'static str {
        match self {
            Engine::Sqlite => "BEGIN IMMEDIATE",
            Engine::Postgres => "BEGIN",
        }
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything this crate can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The driver's error prints only its outer layer ("error serializing
    /// parameter 0"); the cause, a bind conversion refused with the target
    /// type named, is its source, so the text here walks the chain.
    #[error("{}", postgres::describe_error(.0))]
    Postgres(#[from] tokio_postgres::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Pool(String),
    #[error("no rows")]
    NoRows,
    #[error("no such column {0:?}")]
    NoColumn(String),
    #[error("column {0:?}: {1}")]
    Decode(String, String),
    /// An operation one engine has and the other does not, asked of the
    /// wrong one: `ATTACH` on Postgres, say.
    #[error("{1} is not available on the {0} engine")]
    Unsupported(Engine, &'static str),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// The engine's message, for callers that match on which trigger or
    /// constraint refused a write. Tests assert on these; the daemon never
    /// shows one to a client. A Postgres trigger's `RAISE EXCEPTION` text
    /// and a SQLite trigger's `RAISE(ABORT, ...)` text both come back here.
    pub fn message(&self) -> String {
        match self {
            Error::Sqlite(rusqlite::Error::SqliteFailure(_, Some(msg))) => msg.clone(),
            Error::Postgres(e) => match postgres::db_error(e) {
                Some((_, msg)) => msg,
                None => e.to_string(),
            },
            other => other.to_string(),
        }
    }

    /// The primary SQLite result code, when the SQLite engine produced one.
    pub fn sqlite_code(&self) -> Option<i32> {
        match self {
            Error::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => Some(e.extended_code & 0xff),
            _ => None,
        }
    }

    /// The SQLSTATE, when the Postgres server produced one.
    pub fn sqlstate(&self) -> Option<String> {
        match self {
            Error::Postgres(e) => postgres::db_error(e).map(|(code, _)| code),
            _ => None,
        }
    }

    /// Whether a constraint, a trigger's refusal included, refused the write.
    /// SQLite reports both as `SQLITE_CONSTRAINT`; Postgres reports a
    /// constraint as class 23 and a trigger's `RAISE EXCEPTION` as `P0001`.
    pub fn is_constraint(&self) -> bool {
        if self.sqlite_code() == Some(19) {
            return true;
        }
        match self.sqlstate() {
            Some(code) => code.starts_with("23") || code == "P0001",
            None => false,
        }
    }

    /// Whether a `UNIQUE` or primary-key constraint specifically refused it.
    pub fn is_unique_violation(&self) -> bool {
        (self.sqlite_code() == Some(19) && self.message().contains("UNIQUE constraint failed"))
            || self.sqlstate().as_deref() == Some("23505")
    }

    /// Whether the engine reported the write lock held past the busy
    /// timeout (`SQLITE_BUSY`), or a lock the server would not wait for.
    pub fn is_busy(&self) -> bool {
        self.sqlite_code() == Some(5)
            || matches!(
                self.sqlstate().as_deref(),
                Some("55P03" | "40001" | "40P01")
            )
    }

    /// Whether the pool refused the checkout because [`Db::close`] ran.
    pub fn is_pool_closed(&self) -> bool {
        matches!(self, Error::Pool(m) if m == "connection pool closed")
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One engine connection. Statements run on it through [`Query`]; a
/// checked-out [`Conn`] is never shared, the pool hands each one to one
/// holder at a time.
pub struct Connection {
    backend: Backend,
}

enum Backend {
    Sqlite(sqlite::SqliteConn),
    Postgres(postgres::PgConn),
}

impl Connection {
    pub fn engine(&self) -> Engine {
        match self.backend {
            Backend::Sqlite(_) => Engine::Sqlite,
            Backend::Postgres(_) => Engine::Postgres,
        }
    }

    /// The file this connection is `main` on. `None` on Postgres, where a
    /// connection has no path; the callers that need one (the D39 owner
    /// files) exist only for the SQLite engine.
    pub fn path(&self) -> Option<&Path> {
        match &self.backend {
            Backend::Sqlite(c) => Some(c.path()),
            Backend::Postgres(_) => None,
        }
    }

    /// `ATTACH DATABASE <path> AS <alias>` (SQLite). Allowed inside a
    /// transaction, where the attached file joins it. Attaching an alias
    /// this connection already has is a no-op when it is the same file and
    /// an error when it is not: an alias is a name for one file at a time.
    pub fn attach(&self, path: &Path, alias: &str) -> Result<()> {
        match &self.backend {
            Backend::Sqlite(c) => c.attach(path, alias),
            Backend::Postgres(_) => Err(Error::Unsupported(Engine::Postgres, "ATTACH")),
        }
    }

    /// The aliases currently attached, in attach order. Always empty on
    /// Postgres.
    pub fn attached(&self) -> Vec<String> {
        match &self.backend {
            Backend::Sqlite(c) => c.attached(),
            Backend::Postgres(_) => Vec::new(),
        }
    }

    /// `DETACH` one alias (SQLite). Refused by the engine while an open
    /// transaction holds the file; the alias then stays recorded, so the
    /// pool knows the connection is not clean.
    pub fn detach(&self, alias: &str) -> Result<()> {
        match &self.backend {
            Backend::Sqlite(c) => c.detach(alias),
            Backend::Postgres(_) => Err(Error::Unsupported(Engine::Postgres, "DETACH")),
        }
    }

    /// `DETACH` everything this connection attached. Stops at the first
    /// refusal, which is why the pool closes a connection it cannot clean
    /// rather than returning it. A no-op on Postgres.
    pub fn detach_all(&self) -> Result<()> {
        match &self.backend {
            Backend::Sqlite(c) => c.detach_all(),
            Backend::Postgres(_) => Ok(()),
        }
    }

    /// Whether no transaction is open on this connection. SQLite answers
    /// from the engine; Postgres from what this connection has been asked
    /// to run, so open transactions through [`Db::begin`].
    pub fn is_autocommit(&self) -> bool {
        match &self.backend {
            Backend::Sqlite(c) => c.is_autocommit(),
            Backend::Postgres(c) => c.is_autocommit(),
        }
    }

    /// Runs many statements. Migrations and DDL only; nothing here binds.
    pub async fn execute_batch(&self, sql: &str) -> Result<()> {
        match &self.backend {
            Backend::Sqlite(c) => c.execute_batch(sql),
            Backend::Postgres(c) => c.execute_batch(sql).await,
        }
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        match &self.backend {
            Backend::Sqlite(c) => c.execute(sql, params),
            Backend::Postgres(c) => c.execute(sql, params).await,
        }
    }

    async fn fetch(&self, sql: &str, params: &[Value], limit: Option<usize>) -> Result<Vec<Row>> {
        match &self.backend {
            Backend::Sqlite(c) => c.fetch(sql, params, limit),
            Backend::Postgres(c) => c.fetch(sql, params, limit).await,
        }
    }
}

enum DbInner {
    Sqlite(sqlite::Pool),
    Postgres(postgres::Pool),
}

/// One database: a SQLite file or a Postgres database. Cheap to clone; every
/// clone is the same database and the same pool.
#[derive(Clone)]
pub struct Db {
    inner: Arc<DbInner>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.inner {
            DbInner::Sqlite(p) => f.debug_struct("Db").field("path", &p.path()).finish(),
            DbInner::Postgres(p) => f.debug_struct("Db").field("url", &p.describe()).finish(),
        }
    }
}

impl Db {
    /// Opens (creating if needed) the SQLite file at `path`, creating its
    /// parent directory, and switches it to WAL so readers never block the
    /// writer.
    pub async fn open(path: impl AsRef<Path>) -> Result<Db> {
        let pool = sqlite::Pool::open(path.as_ref().to_path_buf()).await?;
        Ok(Db {
            inner: Arc::new(DbInner::Sqlite(pool)),
        })
    }

    /// Connects to the Postgres database at `url`
    /// (`postgres://user:password@host:port/database?sslmode=...`), and
    /// proves one connection answers before returning. `sslrootcert=<pem>`
    /// names the CA the server must chain to; without it the public roots
    /// are trusted; `sslmode=disable` is the only plaintext.
    pub async fn connect(url: &str) -> Result<Db> {
        let pool = postgres::Pool::connect(url).await?;
        Ok(Db {
            inner: Arc::new(DbInner::Postgres(pool)),
        })
    }

    pub fn engine(&self) -> Engine {
        match &*self.inner {
            DbInner::Sqlite(_) => Engine::Sqlite,
            DbInner::Postgres(_) => Engine::Postgres,
        }
    }

    /// The file this is, on SQLite. `None` on Postgres.
    pub fn path(&self) -> Option<&Path> {
        match &*self.inner {
            DbInner::Sqlite(p) => Some(p.path()),
            DbInner::Postgres(_) => None,
        }
    }

    /// A connection from the pool, or a fresh one. Waits when the cap of
    /// open connections is reached.
    pub async fn conn(&self) -> Result<Conn> {
        match &*self.inner {
            DbInner::Sqlite(p) => {
                let (c, permit) = p.checkout().await?;
                Ok(Conn {
                    c: Some(Connection {
                        backend: Backend::Sqlite(c),
                    }),
                    db: self.inner.clone(),
                    _permit: Some(permit),
                })
            }
            DbInner::Postgres(p) => {
                let c = p.checkout().await?;
                Ok(Conn {
                    c: Some(Connection {
                        backend: Backend::Postgres(c),
                    }),
                    db: self.inner.clone(),
                    _permit: None,
                })
            }
        }
    }

    /// A write transaction on a pooled connection: [`Engine::begin_sql`].
    /// Dropping the transaction without committing rolls it back.
    pub async fn begin(&self) -> Result<Transaction> {
        let conn = self.conn().await?;
        conn.execute_batch(self.engine().begin_sql()).await?;
        Ok(Transaction { open: true, conn })
    }

    /// Runs many statements. Migrations and DDL only; nothing here binds.
    pub async fn batch(c: &Connection, sql: &str) -> Result<()> {
        c.execute_batch(sql).await
    }

    /// Refuses every later checkout and drops the idle connections. What a
    /// daemon does on the way out, and what a test does to stand in for the
    /// store being gone: after this every `conn` fails with "connection pool
    /// closed", which is the failure the readiness probe and the credential
    /// resolver have to turn into "not ready" and 401.
    pub fn close(&self) {
        match &*self.inner {
            DbInner::Sqlite(p) => p.close(),
            DbInner::Postgres(p) => p.close(),
        }
    }
}

/// A checked-out connection. Derefs to the connection; goes back to the pool
/// when dropped unless it is mid-transaction, in which case it is closed,
/// because a connection with state is not interchangeable with one without.
pub struct Conn {
    c: Option<Connection>,
    db: Arc<DbInner>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl Deref for Conn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.c.as_ref().expect("connection present until drop")
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("engine", &self.engine())
            .field("path", &self.path())
            .field("autocommit", &self.is_autocommit())
            .finish()
    }
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Conn").field(&self.c).finish()
    }
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("open", &self.open)
            .field("conn", &self.conn)
            .finish()
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let Some(c) = self.c.take() else {
            return;
        };
        match (c.backend, &*self.db) {
            (Backend::Sqlite(c), DbInner::Sqlite(pool)) => {
                if !c.is_autocommit() {
                    return;
                }
                // An attachment is state a checkout must not inherit: the
                // next caller did not ask for that file (invariant 14). A
                // connection that cannot shed it is closed instead of pooled.
                if c.detach_all().is_err() {
                    return;
                }
                pool.put_back(c);
            }
            (Backend::Postgres(mut c), DbInner::Postgres(_)) => {
                if !c.is_autocommit() {
                    c.close_now();
                }
                // Otherwise the pooled object drops here and the pool takes
                // it back.
            }
            _ => unreachable!("a connection belongs to the pool that made it"),
        }
    }
}

/// A transaction on a pooled connection. Derefs to the connection so
/// statements run inside it; `commit` ends it; dropping it rolls it back and
/// returns the connection to the pool afterwards (SQLite) or closes the
/// connection, which rolls it back server-side (Postgres).
pub struct Transaction {
    open: bool,
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
        if self.open {
            self.open = false;
            self.conn.execute_batch("COMMIT").await?;
        }
        Ok(())
    }

    pub async fn rollback(mut self) -> Result<()> {
        if self.open {
            self.open = false;
            self.conn.execute_batch("ROLLBACK").await?;
        }
        Ok(())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        // Best effort on SQLite, where a rollback is a synchronous call: one
        // that fails leaves the connection mid-transaction, and `Conn::drop`
        // then closes it rather than returning it, so the failure cannot
        // leak into another caller. On Postgres nothing can be awaited here;
        // the connection is still marked in-transaction, so `Conn::drop`
        // closes it and the server rolls the transaction back.
        if let Some(Connection {
            backend: Backend::Sqlite(c),
        }) = self.conn.c.as_ref()
            && !c.is_autocommit()
        {
            let _ = c.execute_batch("ROLLBACK");
        }
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

/// Microseconds since the epoch, the SQLite column representation.
pub fn micros(t: DateTime<Utc>) -> i64 {
    t.timestamp_micros()
}

/// The column representation back to a time. `None` only for a value outside
/// chrono's range, which no clock produces.
pub fn from_micros(us: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_micros(us).single()
}

// ---------------------------------------------------------------------------
// Values.
// ---------------------------------------------------------------------------

/// A value bound to a placeholder or read from a column. The first five are
/// SQLite's storage classes and are what a SQLite read produces; the typed
/// four are what the host binds and what a Postgres read produces for a
/// typed column. Each backend maps between them at the bind and the read,
/// so a caller sees the same `Value` whichever engine answered, up to the
/// SQLite shape of the typed ones (a uuid read from SQLite is `Text`), which
/// the typed [`FromSql`] impls accept.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    Bool(bool),
    Uuid(Uuid),
    Timestamp(DateTime<Utc>),
    Json(serde_json::Value),
}

impl Value {
    fn describe(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Integer(_) => "an integer",
            Value::Real(_) => "a real",
            Value::Text(_) => "text",
            Value::Blob(_) => "a blob",
            Value::Bool(_) => "a bool",
            Value::Uuid(_) => "a uuid",
            Value::Timestamp(_) => "a timestamp",
            Value::Json(_) => "json",
        }
    }
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
        Value::Bool(self)
    }
}
impl ToSql for Uuid {
    fn to_sql(self) -> Value {
        Value::Uuid(self)
    }
}
impl ToSql for &Uuid {
    fn to_sql(self) -> Value {
        Value::Uuid(*self)
    }
}
impl ToSql for DateTime<Utc> {
    fn to_sql(self) -> Value {
        Value::Timestamp(self)
    }
}
impl ToSql for &DateTime<Utc> {
    fn to_sql(self) -> Value {
        Value::Timestamp(*self)
    }
}
impl ToSql for serde_json::Value {
    fn to_sql(self) -> Value {
        Value::Json(self)
    }
}
impl ToSql for &serde_json::Value {
    fn to_sql(self) -> Value {
        Value::Json(self.clone())
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

/// A value read out of a column. Each typed impl accepts the SQLite shape
/// (text, integer) beside the typed one, because a column is the same column
/// whichever engine it came from.
pub trait FromSql: Sized {
    fn from_sql(v: &Value) -> std::result::Result<Self, String>;
}

fn wrong(expected: &str, v: &Value) -> String {
    format!("expected {expected}, got {}", v.describe())
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
            Value::Bool(b) => Ok(*b),
            Value::Integer(i) => Ok(*i != 0),
            other => Err(wrong("a bool or a 0/1 integer", other)),
        }
    }
}
impl FromSql for String {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            // A uuid column read as text is the SQLite shape of the same
            // column; a caller comparing ids as strings gets the same text
            // from either engine.
            Value::Uuid(u) => Ok(u.to_string()),
            other => Err(wrong("text", other)),
        }
    }
}
impl FromSql for Vec<u8> {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Blob(b) => Ok(b.clone()),
            Value::Text(s) => Ok(s.as_bytes().to_vec()),
            Value::Json(j) => Ok(j.to_string().into_bytes()),
            other => Err(wrong("a blob", other)),
        }
    }
}
impl FromSql for Uuid {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Uuid(u) => Ok(*u),
            Value::Text(s) => Uuid::parse_str(s).map_err(|e| format!("{s:?} is not a uuid: {e}")),
            other => Err(wrong("a uuid", other)),
        }
    }
}
impl FromSql for DateTime<Utc> {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Timestamp(t) => Ok(*t),
            Value::Integer(us) => {
                from_micros(*us).ok_or_else(|| format!("{us} is not a timestamp"))
            }
            other => Err(wrong("a timestamp", other)),
        }
    }
}
impl FromSql for serde_json::Value {
    fn from_sql(v: &Value) -> std::result::Result<Self, String> {
        match v {
            Value::Json(j) => Ok(j.clone()),
            Value::Text(s) => serde_json::from_str(s).map_err(|e| format!("not json: {e}")),
            Value::Blob(b) => serde_json::from_slice(b).map_err(|e| format!("not json: {e}")),
            other => Err(wrong("json", other)),
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
    pub(crate) fn new(columns: Arc<Vec<String>>, values: Vec<Value>) -> Row {
        Row { columns, values }
    }

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
/// On Postgres the text is rewritten to `$1`, `$2`, ... at execution.
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

impl Query<'_> {
    pub fn bind<T: ToSql>(mut self, v: T) -> Self {
        self.params.push(v.to_sql());
        self
    }

    /// Runs a statement that returns no rows and reports how many it changed.
    /// A statement with `RETURNING` wants `fetch_*`; SQLite refuses it here
    /// and Postgres silently discards the rows.
    pub async fn execute(self, c: &Connection) -> Result<u64> {
        c.execute(self.sql, &self.params).await
    }

    pub async fn fetch_all(self, c: &Connection) -> Result<Vec<Row>> {
        c.fetch(self.sql, &self.params, None).await
    }

    pub async fn fetch_optional(self, c: &Connection) -> Result<Option<Row>> {
        Ok(c.fetch(self.sql, &self.params, Some(1))
            .await?
            .into_iter()
            .next())
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

/// `[a-z_][a-z0-9_]*`: what an attach alias has to be.
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
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

// ---------------------------------------------------------------------------
// The test backend.
// ---------------------------------------------------------------------------

/// The connection string a test reads to run against Postgres. Unset, a
/// database test runs on SQLite, which needs nothing; set, the test fixtures
/// make their databases there instead.
pub const TEST_URL_ENV: &str = "HIVE_SANDBOX_TEST_DATABASE_URL";

/// Set in the environment that promised a Postgres (CI's `postgres` job), so
/// a test that would skip for want of [`TEST_URL_ENV`] fails instead. A job
/// that provisions a server and then skips the tests that need it reports
/// success for doing nothing.
pub const TEST_REQUIRE_ENV: &str = "HIVE_SANDBOX_REQUIRE_DATABASE_TESTS";

/// The test Postgres URL, for a test that only makes sense on that engine.
/// `None` means the variable is unset and a `SKIPPED:` line naming the test
/// has been printed; the caller returns. Under [`TEST_REQUIRE_ENV`] the
/// absence is a panic instead, so the skip cannot pass as green where a
/// server was promised.
pub fn test_postgres_url(test_name: &str) -> Option<String> {
    match std::env::var(TEST_URL_ENV) {
        Ok(u) if !u.trim().is_empty() => Some(u.trim().to_string()),
        _ => {
            if std::env::var(TEST_REQUIRE_ENV).is_ok_and(|v| v == "1") {
                panic!(
                    "{test_name} needs {TEST_URL_ENV} and {TEST_REQUIRE_ENV}=1 forbids the skip"
                );
            }
            eprintln!("SKIPPED: {test_name} needs {TEST_URL_ENV}; run scripts/db-up and export it");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("t.db")).await.expect("open");
        (dir, db)
    }

    fn sqlite_pool(db: &Db) -> &sqlite::Pool {
        match &*db.inner {
            DbInner::Sqlite(p) => p,
            DbInner::Postgres(_) => unreachable!(),
        }
    }

    /// The Postgres test database, when `HIVE_SANDBOX_TEST_DATABASE_URL`
    /// names one. Tests on it use a table name of their own and drop it,
    /// so they share the database without sharing state.
    async fn pg(test_name: &str) -> Option<Db> {
        let url = test_postgres_url(test_name)?;
        Some(
            Db::connect(&url)
                .await
                .expect("connect to the test postgres"),
        )
    }

    fn unique_table() -> String {
        format!("t_{}", Uuid::new_v4().simple())
    }

    #[tokio::test]
    async fn opens_in_wal_with_foreign_keys_on() {
        let (_d, db) = temp().await;
        assert_eq!(db.engine(), Engine::Sqlite);
        let c = db.conn().await.unwrap();
        let fk: i64 = query("PRAGMA foreign_keys").fetch_scalar(&c).await.unwrap();
        assert_eq!(fk, 1);
        let mode: String = query("PRAGMA journal_mode").fetch_scalar(&c).await.unwrap();
        assert_eq!(mode, "wal");
        let v: String = query("SELECT sqlite_version()")
            .fetch_scalar(&c)
            .await
            .unwrap();
        let major_minor: Vec<u32> = v
            .split('.')
            .take(2)
            .filter_map(|p| p.parse().ok())
            .collect();
        assert!(
            major_minor >= vec![3, 45],
            "the schema needs jsonb and the -> operators; bundled engine is {v}"
        );
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
        // The SQLite shapes: a uuid is text and a bool is an integer there.
        assert_eq!(r.get::<Value>("id"), Value::Text(id.to_string()));
        assert_eq!(r.get::<Value>("b"), Value::Integer(1));
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
        let n = query("UPDATE t SET v = ?1")
            .bind("y")
            .execute(&c)
            .await
            .unwrap();
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
            query("INSERT INTO t VALUES (1)")
                .execute(&tx)
                .await
                .unwrap();
            // dropped, not committed
        }
        let n: i64 = query("SELECT count(*) FROM t")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let tx = db.begin().await.unwrap();
        query("INSERT INTO t VALUES (1)")
            .execute(&tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let n: i64 = query("SELECT count(*) FROM t")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// The pool: a returned connection is the one handed out next, a
    /// connection dropped mid-transaction is not, and the cap holds.
    #[tokio::test]
    async fn connections_are_reused_and_capped() {
        let (_d, db) = temp().await;
        let pool = sqlite_pool(&db);
        {
            let c = db.conn().await.unwrap();
            query("CREATE TABLE t (n INTEGER)")
                .execute(&c)
                .await
                .unwrap();
        }
        assert_eq!(pool.idle_len(), 1, "one idle after return");
        {
            let _a = db.conn().await.unwrap();
            assert_eq!(pool.idle_len(), 0, "the idle one was reused");
            let _b = db.conn().await.unwrap();
            assert_eq!(pool.available_permits(), sqlite::MAX_OPEN - 2);
        }
        assert_eq!(pool.idle_len(), 2);
        assert_eq!(pool.available_permits(), sqlite::MAX_OPEN);
        // A connection left inside a transaction is closed, not pooled.
        {
            let c = db.conn().await.unwrap();
            c.execute_batch("BEGIN").await.unwrap();
            query("INSERT INTO t VALUES (1)").execute(&c).await.unwrap();
        }
        assert_eq!(
            pool.idle_len(),
            1,
            "a mid-transaction connection was pooled"
        );
        let c = db.conn().await.unwrap();
        let n: i64 = query("SELECT count(*) FROM t")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(n, 0, "the abandoned transaction leaked a row");
        drop(c);
        let mut held = Vec::new();
        for _ in 0..(sqlite::MAX_IDLE + 4) {
            held.push(db.conn().await.unwrap());
        }
        drop(held);
        assert!(pool.idle_len() <= sqlite::MAX_IDLE);
    }

    /// sqlite-vec is in every connection this process opens (D41).
    #[tokio::test]
    async fn the_vector_extension_is_registered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("vec.db")).await.unwrap();
        let c = db.conn().await.unwrap();
        let v: String = query("SELECT vec_version()")
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert!(v.starts_with('v'), "vec_version() = {v}");
        query("CREATE VIRTUAL TABLE probe USING vec0(id TEXT PRIMARY KEY, e float[2])")
            .execute(&c)
            .await
            .unwrap();
        query("INSERT INTO probe (id, e) VALUES ('a', vec_f32('[1,0]')), ('b', vec_f32('[0,1]'))")
            .execute(&c)
            .await
            .unwrap();
        let nearest: String =
            query("SELECT id FROM probe WHERE e MATCH vec_f32('[0.9,0.1]') AND k = 1")
                .fetch_scalar(&c)
                .await
                .unwrap();
        assert_eq!(nearest, "a");
    }

    /// An attachment made on a checkout is gone by the next checkout, and a
    /// connection that could not detach (one still mid-transaction) is not
    /// pooled at all.
    #[tokio::test]
    async fn attachments_do_not_survive_a_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("main.db")).await.unwrap();
        let other = dir.path().join("other.db");
        {
            let c = db.conn().await.unwrap();
            c.attach(&other, "o").unwrap();
            c.attach(&other, "o").unwrap();
            assert_eq!(c.attached(), vec!["o".to_string()]);
            assert!(
                c.attach(&other, "o; DROP").is_err(),
                "an alias is an identifier"
            );
            query("CREATE TABLE o.t (n INTEGER)")
                .execute(&c)
                .await
                .unwrap();
        }
        let c = db.conn().await.unwrap();
        assert!(c.attached().is_empty());
        let names: Vec<String> = query("PRAGMA database_list")
            .fetch_all(&c)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String>("name"))
            .collect();
        assert_eq!(names, vec!["main".to_string()]);
        // Mid-transaction with the file in use, the detach is refused and
        // the connection is closed rather than pooled. (An attached file the
        // transaction never touched detaches fine; the engine refuses only
        // what the transaction holds.)
        {
            c.execute_batch("BEGIN IMMEDIATE").await.unwrap();
            c.attach(&other, "o").unwrap();
            query("INSERT INTO o.t VALUES (1)")
                .execute(&c)
                .await
                .unwrap();
            assert!(
                c.detach_all().is_err(),
                "DETACH of a file the transaction wrote"
            );
            assert_eq!(
                c.attached(),
                vec!["o".to_string()],
                "a failed detach forgot the alias"
            );
        }
        drop(c);
        assert!(
            sqlite_pool(&db).idle_all_clean(),
            "a connection with an attachment was pooled"
        );
    }

    /// The pattern that took the libSQL fork down: many connections open at
    /// once on one file, then closed. Kept so the engine underneath this
    /// crate is held to it on every build.
    #[tokio::test]
    async fn many_open_connections_close_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("many.db");
        let db = Db::open(&path).await.unwrap();
        {
            let c = db.conn().await.unwrap();
            query("CREATE TABLE t (n INTEGER)")
                .execute(&c)
                .await
                .unwrap();
        }
        for _ in 0..20 {
            let mut keep = Vec::new();
            for _ in 0..64 {
                let c = sqlite::SqliteConn::open(&path).unwrap();
                let n: i64 = c
                    .fetch("SELECT count(*) FROM t", &[], Some(1))
                    .unwrap()
                    .into_iter()
                    .next()
                    .unwrap()
                    .try_get_at(0)
                    .unwrap();
                assert_eq!(n, 0);
                keep.push(c);
            }
            drop(keep);
        }
    }

    #[tokio::test]
    async fn defaults_mint_a_uuid_and_a_time() {
        let (_d, db) = temp().await;
        let c = db.conn().await.unwrap();
        let e = db.engine();
        Db::batch(
            &c,
            &format!(
                "CREATE TABLE t (id TEXT PRIMARY KEY DEFAULT {}, at INTEGER NOT NULL DEFAULT {})",
                e.uuid_sql(),
                e.now_sql()
            ),
        )
        .await
        .unwrap();
        query("INSERT INTO t DEFAULT VALUES")
            .execute(&c)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn a_closed_pool_refuses_checkouts_by_name() {
        let (_d, db) = temp().await;
        db.close();
        let err = db.conn().await.unwrap_err();
        assert!(err.is_pool_closed(), "{err}");
    }

    #[tokio::test]
    async fn attach_is_sqlite_only() {
        let Some(db) = pg("hive-db::attach_is_sqlite_only").await else {
            return;
        };
        let c = db.conn().await.unwrap();
        let err = c.attach(Path::new("x.db"), "o").unwrap_err();
        assert!(
            matches!(err, Error::Unsupported(Engine::Postgres, "ATTACH")),
            "{err}"
        );
        assert!(c.attached().is_empty());
        assert!(c.path().is_none());
        assert!(db.path().is_none());
    }

    /// The same typed round trip as the SQLite test, against typed columns:
    /// the host binds a `Uuid`, a `DateTime`, a `bool` and json, and reads
    /// them back as themselves, with the typed `Value` variants underneath.
    #[tokio::test]
    async fn postgres_binds_and_reads_every_type_by_name() {
        let Some(db) = pg("hive-db::postgres_binds_and_reads_every_type_by_name").await else {
            return;
        };
        assert_eq!(db.engine(), Engine::Postgres);
        let c = db.conn().await.unwrap();
        let t = unique_table();
        Db::batch(
            &c,
            &format!(
                "CREATE TABLE {t} (id uuid, n bigint, f double precision, b boolean, j jsonb, \
                 at timestamptz, raw bytea, maybe text, small integer)"
            ),
        )
        .await
        .unwrap();
        let id = Uuid::new_v4();
        let at = now();
        let j = serde_json::json!({"a": [1, 2]});
        query(&format!(
            "INSERT INTO {t} VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
        ))
        .bind(id)
        .bind(7i64)
        .bind(1.5f64)
        .bind(true)
        .bind(&j)
        .bind(at)
        .bind(vec![1u8, 2, 3])
        .bind(None::<String>)
        .bind(42i32)
        .execute(&c)
        .await
        .unwrap();
        let r = query(&format!("SELECT * FROM {t}"))
            .fetch_one(&c)
            .await
            .unwrap();
        assert_eq!(r.get::<Uuid>("id"), id);
        assert_eq!(
            r.get::<String>("id"),
            id.to_string(),
            "a uuid reads as text too"
        );
        assert_eq!(r.get::<i64>("n"), 7);
        assert_eq!(r.get::<i32>("n"), 7);
        assert_eq!(r.get::<i64>("small"), 42);
        assert_eq!(r.get::<f64>("f"), 1.5);
        assert!(r.get::<bool>("b"));
        assert_eq!(r.get::<serde_json::Value>("j"), j);
        assert_eq!(r.get::<DateTime<Utc>>("at"), at);
        assert_eq!(r.get::<Vec<u8>>("raw"), vec![1, 2, 3]);
        assert_eq!(r.get::<Option<String>>("maybe"), None);
        assert!(r.try_get::<String>("nope").is_err());
        assert!(r.try_get::<String>("n").is_err(), "an integer is not text");
        assert_eq!(r.get::<Value>("id"), Value::Uuid(id));
        assert_eq!(r.get::<Value>("b"), Value::Bool(true));
        assert_eq!(r.get::<Value>("at"), Value::Timestamp(at));
        Db::batch(&c, &format!("DROP TABLE {t}")).await.unwrap();
    }

    /// A statement written against the SQLite column shapes (a uuid as text,
    /// a time as an integer, a bool as 0/1, json as text) binds correctly
    /// against the typed columns, because the bind converts by the type the
    /// server inferred. This is what lets the store's statements run on both
    /// engines before every call site is retyped.
    #[tokio::test]
    async fn postgres_binds_the_sqlite_shapes_to_typed_columns() {
        let Some(db) = pg("hive-db::postgres_binds_the_sqlite_shapes_to_typed_columns").await
        else {
            return;
        };
        let c = db.conn().await.unwrap();
        let t = unique_table();
        Db::batch(
            &c,
            &format!("CREATE TABLE {t} (id uuid, b boolean, at timestamptz, j jsonb, n integer)"),
        )
        .await
        .unwrap();
        let id = Uuid::new_v4();
        let at = now();
        query(&format!("INSERT INTO {t} VALUES (?1, ?2, ?3, ?4, ?5)"))
            .bind(Value::Text(id.to_string()))
            .bind(Value::Integer(1))
            .bind(Value::Integer(micros(at)))
            .bind(Value::Text("{\"k\":1}".into()))
            .bind(Value::Integer(5))
            .execute(&c)
            .await
            .unwrap();
        let r = query(&format!("SELECT * FROM {t} WHERE id = ?1 AND at = ?2"))
            .bind(id.to_string())
            .bind(micros(at))
            .fetch_one(&c)
            .await
            .unwrap();
        assert_eq!(r.get::<Uuid>("id"), id);
        assert!(r.get::<bool>("b"));
        assert_eq!(r.get::<DateTime<Utc>>("at"), at);
        assert_eq!(r.get::<serde_json::Value>("j"), serde_json::json!({"k": 1}));
        assert_eq!(r.get::<i64>("n"), 5);
        // A mismatch names the target type rather than failing silently.
        let err = query(&format!("INSERT INTO {t} (id) VALUES (?1)"))
            .bind("not a uuid")
            .execute(&c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("uuid"), "{err}");
        Db::batch(&c, &format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn postgres_trigger_and_unique_refusals_classify() {
        let Some(db) = pg("hive-db::postgres_trigger_and_unique_refusals_classify").await else {
            return;
        };
        let c = db.conn().await.unwrap();
        let t = unique_table();
        Db::batch(
            &c,
            &format!(
                "CREATE TABLE {t} (n integer UNIQUE);
                 CREATE FUNCTION {t}_no_neg() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN
                     IF NEW.n < 0 THEN RAISE EXCEPTION 'n must not be negative'; END IF;
                     RETURN NEW;
                 END $$;
                 CREATE TRIGGER no_neg BEFORE INSERT ON {t} FOR EACH ROW EXECUTE FUNCTION {t}_no_neg();"
            ),
        )
        .await
        .unwrap();
        let err = query(&format!("INSERT INTO {t} VALUES (-1)"))
            .execute(&c)
            .await
            .unwrap_err();
        assert!(err.is_constraint(), "{err:?}");
        assert!(!err.is_unique_violation());
        assert_eq!(err.message(), "n must not be negative");
        assert_eq!(err.sqlstate().as_deref(), Some("P0001"));
        query(&format!("INSERT INTO {t} VALUES (1)"))
            .execute(&c)
            .await
            .unwrap();
        let err = query(&format!("INSERT INTO {t} VALUES (1)"))
            .execute(&c)
            .await
            .unwrap_err();
        assert!(err.is_constraint());
        assert!(err.is_unique_violation(), "{err:?}");
        assert_eq!(err.sqlstate().as_deref(), Some("23505"));
        Db::batch(&c, &format!("DROP TABLE {t}; DROP FUNCTION {t}_no_neg()"))
            .await
            .unwrap();
    }

    /// A transaction dropped without commit is rolled back, and the
    /// connection that held it is not the one the next checkout gets: a
    /// connection that was left mid-transaction is closed, so a later caller
    /// cannot land inside somebody else's aborted transaction.
    #[tokio::test]
    async fn postgres_dropped_transaction_rolls_back_and_the_connection_is_not_reused() {
        let Some(db) =
            pg("hive-db::postgres_dropped_transaction_rolls_back_and_the_connection_is_not_reused")
                .await
        else {
            return;
        };
        let t = unique_table();
        {
            let c = db.conn().await.unwrap();
            Db::batch(&c, &format!("CREATE TABLE {t} (n integer)"))
                .await
                .unwrap();
        }
        {
            let tx = db.begin().await.unwrap();
            assert!(!tx.is_autocommit());
            query(&format!("INSERT INTO {t} VALUES (1)"))
                .execute(&tx)
                .await
                .unwrap();
            // dropped, not committed
        }
        let c = db.conn().await.unwrap();
        assert!(
            c.is_autocommit(),
            "the next checkout inherited a transaction"
        );
        let n: i64 = query(&format!("SELECT count(*) FROM {t}"))
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(n, 0, "the dropped transaction's row landed");
        // A failed statement aborts the transaction; the rest of it is
        // refused until it ends. Named here so the port does not learn it
        // from a store test.
        let tx = db.begin().await.unwrap();
        let _ = query("SELECT no_such_function()")
            .execute(&tx)
            .await
            .unwrap_err();
        let err = query(&format!("INSERT INTO {t} VALUES (2)"))
            .execute(&tx)
            .await
            .unwrap_err();
        assert_eq!(err.sqlstate().as_deref(), Some("25P02"), "{err}");
        tx.rollback().await.unwrap();
        let tx = db.begin().await.unwrap();
        query(&format!("INSERT INTO {t} VALUES (3)"))
            .execute(&tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let n: i64 = query(&format!("SELECT count(*) FROM {t}"))
            .fetch_scalar(&c)
            .await
            .unwrap();
        assert_eq!(n, 1);
        Db::batch(&c, &format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn postgres_defaults_mint_a_uuid_and_a_time() {
        let Some(db) = pg("hive-db::postgres_defaults_mint_a_uuid_and_a_time").await else {
            return;
        };
        let c = db.conn().await.unwrap();
        let e = db.engine();
        let t = unique_table();
        Db::batch(
            &c,
            &format!(
                "CREATE TABLE {t} (id uuid PRIMARY KEY DEFAULT {}, at timestamptz NOT NULL DEFAULT {})",
                e.uuid_sql(),
                e.now_sql()
            ),
        )
        .await
        .unwrap();
        query(&format!("INSERT INTO {t} DEFAULT VALUES"))
            .execute(&c)
            .await
            .unwrap();
        let r = query(&format!("SELECT id, at FROM {t}"))
            .fetch_one(&c)
            .await
            .unwrap();
        let id: Uuid = r.get("id");
        assert_eq!(id.get_version_num(), 4);
        let at: DateTime<Utc> = r.get("at");
        let skew = (now() - at).num_seconds().abs();
        assert!(skew < 5, "default clock is {skew}s off the host clock");
        Db::batch(&c, &format!("DROP TABLE {t}")).await.unwrap();
    }

    #[tokio::test]
    async fn postgres_closed_pool_refuses_checkouts_by_name() {
        let Some(db) = pg("hive-db::postgres_closed_pool_refuses_checkouts_by_name").await else {
            return;
        };
        db.close();
        let err = db.conn().await.unwrap_err();
        assert!(err.is_pool_closed(), "{err}");
    }

    #[tokio::test]
    async fn postgres_refuses_a_bad_url_without_echoing_a_secret() {
        let err = Db::connect("postgres://nobody:s3cret@127.0.0.1:1/none?sslmode=disable")
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(!text.contains("s3cret"), "{text}");
    }
}

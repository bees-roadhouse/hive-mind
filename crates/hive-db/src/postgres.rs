//! The Postgres backend: `tokio-postgres` behind `deadpool-postgres`, TLS by
//! `rustls` (D43 §7).
//!
//! What this module owns and the rest of the crate does not have to know:
//!
//! - **Placeholders.** The workspace writes `?N`; Postgres wants `$N`. The
//!   rewrite is textual and skips string literals, quoted identifiers and
//!   comments, so a `?` inside `'...'` is left alone. The numbering is kept,
//!   which is why the convention is numbered placeholders in the first place.
//! - **Types.** A [`Value`] binds to whatever type the server inferred for the
//!   parameter: `Value::Text` to a `uuid` column parses, `Value::Integer` to a
//!   `timestamptz` is microseconds, `Value::Integer` to `boolean` is 0/1. That
//!   is the D38 §3 table read in both directions at the bind, so a statement
//!   written against SQLite's column shapes binds correctly against the typed
//!   Postgres schema without every call site changing first. A read comes
//!   back as the typed variant (`Uuid`, `Timestamp`, `Bool`, `Json`), which
//!   the typed [`FromSql`](crate::FromSql) impls accept beside the SQLite
//!   shapes.
//! - **Transactions.** `BEGIN` is plain `BEGIN`: Postgres has no `IMMEDIATE`
//!   and does not need one, writers do not serialise on a file lock. The
//!   connection tracks whether it is inside one, because the pool must not
//!   hand a connection mid-transaction to the next caller (invariant 14), and
//!   a connection in that state is taken out of the pool and closed rather
//!   than returned; the server rolls its transaction back on disconnect.
//! - **An error aborts the transaction.** Unlike SQLite, a statement that
//!   fails inside a Postgres transaction leaves it aborted, and every later
//!   statement fails with "current transaction is aborted" until rollback. A
//!   caller that tries a write and continues on refusal needs a `SAVEPOINT`
//!   here; that is a port concern for each such site, named so nobody finds
//!   it as a surprise.
//! - **The driver's connection task is spawned.** `tokio-postgres` splits a
//!   connection into a client and a connection future that must be polled;
//!   the pool spawns that future. It is the driver's socket loop, not a store
//!   future, and `docs/development.md`'s rule about not spawning store
//!   futures is about the latter.

use std::borrow::Cow;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::BytesMut;
use deadpool_postgres::{Manager, ManagerConfig, Object, RecyclingMethod};
use tokio_postgres::types::{IsNull, ToSql, Type};

use crate::{Error, Result, Row, Value};

/// How many connections one pool may have open at once. The same cap as the
/// SQLite pool, for the same reason: past it a caller waits for a return.
const MAX_OPEN: usize = 32;

/// A connection string parameter this crate handles itself: a PEM file of CA
/// certificates the server's certificate must chain to. `tokio-postgres`
/// refuses parameters it does not know, so it is stripped before parsing.
const ROOT_CERT_PARAM: &str = "sslrootcert";

/// The pool and what it was opened with, minus the secret.
pub(crate) struct Pool {
    pool: deadpool_postgres::Pool,
    describe: String,
}

impl Pool {
    /// Connects to `url` (`postgres://user:password@host:port/database?...`)
    /// and verifies one connection answers before returning. TLS is always
    /// `verify-full` when it is negotiated at all: the hostname is checked
    /// against the certificate, against the CA file `sslrootcert` names or
    /// the public roots. `sslmode=disable` is the only way to get plaintext,
    /// which is what a local container and CI use.
    pub(crate) async fn connect(url: &str) -> Result<Pool> {
        let (url, root_cert) = take_param(url, ROOT_CERT_PARAM);
        let config = tokio_postgres::Config::from_str(&url)
            .map_err(|e| Error::Other(format!("postgres url: {e}")))?;
        let describe = describe(&config);
        let tls = tls_connector(root_cert.as_deref())?;
        let manager = Manager::from_config(
            config,
            tls,
            ManagerConfig {
                // A connection is checked for liveness by whether the driver
                // closed it, not by a round trip per checkout; a connection
                // this crate let go mid-transaction never reaches the pool.
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = deadpool_postgres::Pool::builder(manager)
            .max_size(MAX_OPEN)
            .build()
            .map_err(|e| Error::Pool(e.to_string()))?;
        let pool = Pool { pool, describe };
        // The pool connects lazily; this is the "can I connect" probe, and it
        // is a real connect because a listening port is not readiness.
        let c = pool.checkout().await?;
        let one: i64 = c
            .fetch("SELECT 1::bigint", &[], Some(1))
            .await?
            .into_iter()
            .next()
            .ok_or(Error::NoRows)?
            .try_get_at(0)?;
        if one != 1 {
            return Err(Error::Other(format!(
                "{}: ping answered {one}",
                pool.describe
            )));
        }
        Ok(pool)
    }

    pub(crate) fn describe(&self) -> &str {
        &self.describe
    }

    pub(crate) async fn checkout(&self) -> Result<PgConn> {
        let client = self.pool.get().await.map_err(|e| match e {
            deadpool_postgres::PoolError::Closed => Error::Pool("connection pool closed".into()),
            deadpool_postgres::PoolError::Backend(e) => Error::Postgres(e),
            other => Error::Pool(other.to_string()),
        })?;
        Ok(PgConn {
            client: Some(client),
            in_tx: AtomicBool::new(false),
        })
    }

    /// Refuses every later checkout. Connections already checked out finish
    /// their work and are closed on return.
    pub(crate) fn close(&self) {
        self.pool.close();
    }
}

/// One pooled client. Goes back to the pool when dropped unless it is
/// mid-transaction, in which case [`PgConn::close_now`] has taken it out.
pub(crate) struct PgConn {
    client: Option<Object>,
    in_tx: AtomicBool,
}

impl PgConn {
    fn client(&self) -> &Object {
        self.client.as_ref().expect("client present until drop")
    }

    pub(crate) fn is_autocommit(&self) -> bool {
        !self.in_tx.load(Ordering::SeqCst)
    }

    /// Runs many statements through the simple query protocol, which is
    /// what a migration file wants: no parameters, any number of statements,
    /// DDL included. Tracks `BEGIN` and `COMMIT`/`ROLLBACK` by the first and
    /// last statement's first word, so a transaction opened this way is
    /// still known to the pool; `Db::begin` is the supported way to open one.
    pub(crate) async fn execute_batch(&self, sql: &str) -> Result<()> {
        self.client().batch_execute(sql).await?;
        self.note_transaction_words(sql);
        Ok(())
    }

    fn note_transaction_words(&self, sql: &str) {
        let mut statements = sql
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .peekable();
        let first = statements.peek().map(|s| first_word(s));
        let last = statements.last().map(first_word);
        match (first.as_deref(), last.as_deref()) {
            (_, Some("COMMIT" | "END" | "ROLLBACK" | "ABORT")) => {
                self.in_tx.store(false, Ordering::SeqCst);
            }
            (Some("BEGIN" | "START"), _) => {
                self.in_tx.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    pub(crate) async fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        let sql = rewrite_placeholders(sql);
        let stmt = self.client().prepare_cached(&sql).await?;
        let bound: Vec<&(dyn ToSql + Sync)> = params.iter().map(|v| v as _).collect();
        let n = self.client().execute(&stmt, &bound).await?;
        Ok(n)
    }

    pub(crate) async fn fetch(
        &self,
        sql: &str,
        params: &[Value],
        limit: Option<usize>,
    ) -> Result<Vec<Row>> {
        let sql = rewrite_placeholders(sql);
        let stmt = self.client().prepare_cached(&sql).await?;
        let columns: Arc<Vec<String>> = Arc::new(
            stmt.columns()
                .iter()
                .map(|c| c.name().to_string())
                .collect(),
        );
        let bound: Vec<&(dyn ToSql + Sync)> = params.iter().map(|v| v as _).collect();
        let rows = self.client().query(&stmt, &bound).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let mut values = Vec::with_capacity(columns.len());
            for (i, col) in stmt.columns().iter().enumerate() {
                values.push(read_column(&r, i, col.type_(), col.name())?);
            }
            out.push(Row::new(columns.clone(), values));
            if limit.is_some_and(|l| out.len() >= l) {
                break;
            }
        }
        Ok(out)
    }

    /// Takes the client out of the pool and closes it. What the pool does
    /// with a connection left mid-transaction: the server rolls the
    /// transaction back when the socket closes, and no later caller can
    /// inherit it.
    pub(crate) fn close_now(&mut self) {
        if let Some(obj) = self.client.take() {
            drop(Object::take(obj));
        }
    }
}

fn first_word(s: &str) -> String {
    s.split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase()
}

/// `user@host:port/database`, for errors. Never the password.
fn describe(config: &tokio_postgres::Config) -> String {
    let host = config
        .get_hosts()
        .first()
        .map(|h| match h {
            tokio_postgres::config::Host::Tcp(s) => s.clone(),
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(p) => p.display().to_string(),
        })
        .unwrap_or_else(|| "?".into());
    let port = config.get_ports().first().copied().unwrap_or(5432);
    format!(
        "postgres://{}@{host}:{port}/{}",
        config.get_user().unwrap_or("?"),
        config.get_dbname().unwrap_or("?")
    )
}

/// Removes `name=value` from the URL's query string and returns the value.
fn take_param(url: &str, name: &str) -> (String, Option<String>) {
    let Some((base, query)) = url.split_once('?') else {
        return (url.to_string(), None);
    };
    let mut found = None;
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            if let Some((k, v)) = pair.split_once('=')
                && k == name
            {
                found = Some(v.to_string());
                return false;
            }
            true
        })
        .collect();
    let url = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    (url, found)
}

/// A TLS connector trusting the CA file at `root_cert`, or the public roots
/// without one. rustls verifies the hostname whenever TLS is negotiated, so
/// there is no verify-ca-but-not-hostname mode to misconfigure.
fn tls_connector(root_cert: Option<&str>) -> Result<tokio_postgres_rustls::MakeRustlsConnect> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    match root_cert {
        Some(path) => {
            for cert in CertificateDer::pem_file_iter(path)
                .map_err(|e| Error::Other(format!("{ROOT_CERT_PARAM} {path}: {e}")))?
            {
                let cert =
                    cert.map_err(|e| Error::Other(format!("{ROOT_CERT_PARAM} {path}: {e}")))?;
                roots
                    .add(cert)
                    .map_err(|e| Error::Other(format!("{ROOT_CERT_PARAM} {path}: {e}")))?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Other(format!("tls: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

/// `?N` to `$N`, outside string literals, quoted identifiers and comments.
/// A `?` not followed by a digit is left alone; the workspace does not use
/// bare `?` placeholders.
pub(crate) fn rewrite_placeholders(sql: &str) -> Cow<'_, str> {
    if !sql.contains('?') {
        return Cow::Borrowed(sql);
    }
    // Byte offsets on char boundaries throughout: every branch advances by a
    // whole character or by a slice found from one, so a multi-byte literal
    // ('é') copies as itself. The first version of this walked bytes and
    // pushed each as a char, which turned 'é' into two mojibake characters.
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while let Some(ch) = sql[i..].chars().next() {
        match ch {
            '\'' | '"' => {
                // Copy the literal through, including a doubled quote inside
                // it, which closes and reopens and therefore copies as two.
                let end = sql[i + 1..]
                    .find(ch)
                    .map(|n| i + 1 + n + 1)
                    .unwrap_or(sql.len());
                out.push_str(&sql[i..end]);
                i = end;
            }
            '-' if sql[i..].starts_with("--") => {
                let end = sql[i..].find('\n').map(|n| i + n).unwrap_or(sql.len());
                out.push_str(&sql[i..end]);
                i = end;
            }
            '/' if sql[i..].starts_with("/*") => {
                let end = sql[i + 2..]
                    .find("*/")
                    .map(|n| i + 2 + n + 2)
                    .unwrap_or(sql.len());
                out.push_str(&sql[i..end]);
                i = end;
            }
            '?' if sql[i + 1..].starts_with(|c: char| c.is_ascii_digit()) => {
                out.push('$');
                i += 1;
            }
            _ => {
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Cow::Owned(out)
}

/// What the server will accept this value as, by the type it inferred for the
/// parameter. Every arm is one row of D38 §3 read back towards Postgres.
impl ToSql for Value {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        let mismatch = |what: &str| -> Box<dyn std::error::Error + Sync + Send> {
            format!("cannot bind {what} to a {ty} parameter").into()
        };
        match self {
            Value::Null => Ok(IsNull::Yes),
            Value::Integer(i) => match *ty {
                Type::INT2 => i16::try_from(*i)
                    .map_err(|_| mismatch("an integer out of range"))?
                    .to_sql(ty, out),
                Type::INT4 => i32::try_from(*i)
                    .map_err(|_| mismatch("an integer out of range"))?
                    .to_sql(ty, out),
                Type::INT8 => i.to_sql(ty, out),
                Type::OID => u32::try_from(*i)
                    .map_err(|_| mismatch("an integer out of range"))?
                    .to_sql(ty, out),
                Type::FLOAT4 => (*i as f32).to_sql(ty, out),
                Type::FLOAT8 => (*i as f64).to_sql(ty, out),
                Type::BOOL => (*i != 0).to_sql(ty, out),
                Type::TIMESTAMPTZ => crate::from_micros(*i)
                    .ok_or_else(|| mismatch("an integer that is not a timestamp"))?
                    .to_sql(ty, out),
                Type::TEXT | Type::VARCHAR => i.to_string().to_sql(ty, out),
                _ => Err(mismatch("an integer")),
            },
            Value::Real(f) => match *ty {
                Type::FLOAT4 => (*f as f32).to_sql(ty, out),
                Type::FLOAT8 => f.to_sql(ty, out),
                _ => Err(mismatch("a real")),
            },
            Value::Text(s) => match *ty {
                Type::TEXT | Type::VARCHAR | Type::NAME | Type::BPCHAR | Type::UNKNOWN => {
                    s.to_sql(ty, out)
                }
                Type::UUID => uuid::Uuid::parse_str(s)
                    .map_err(|e| mismatch(&format!("text that is not a uuid ({e})")))?
                    .to_sql(ty, out),
                Type::JSON | Type::JSONB => serde_json::from_str::<serde_json::Value>(s)
                    .map_err(|e| mismatch(&format!("text that is not json ({e})")))?
                    .to_sql(ty, out),
                Type::TIMESTAMPTZ => chrono::DateTime::parse_from_rfc3339(s)
                    .map_err(|e| mismatch(&format!("text that is not a timestamp ({e})")))?
                    .with_timezone(&chrono::Utc)
                    .to_sql(ty, out),
                Type::BYTEA => s.as_bytes().to_sql(ty, out),
                _ => Err(mismatch("text")),
            },
            Value::Blob(b) => match *ty {
                Type::BYTEA => b.as_slice().to_sql(ty, out),
                Type::JSONB | Type::JSON => serde_json::from_slice::<serde_json::Value>(b)
                    .map_err(|e| mismatch(&format!("bytes that are not json ({e})")))?
                    .to_sql(ty, out),
                Type::TEXT | Type::VARCHAR => std::str::from_utf8(b)
                    .map_err(|_| mismatch("bytes that are not utf-8"))?
                    .to_sql(ty, out),
                _ => Err(mismatch("a blob")),
            },
            Value::Bool(b) => match *ty {
                Type::BOOL => b.to_sql(ty, out),
                Type::INT2 => (*b as i16).to_sql(ty, out),
                Type::INT4 => (*b as i32).to_sql(ty, out),
                Type::INT8 => (*b as i64).to_sql(ty, out),
                _ => Err(mismatch("a bool")),
            },
            Value::Uuid(u) => match *ty {
                Type::UUID => u.to_sql(ty, out),
                Type::TEXT | Type::VARCHAR | Type::UNKNOWN => u.to_string().to_sql(ty, out),
                _ => Err(mismatch("a uuid")),
            },
            Value::Timestamp(t) => match *ty {
                Type::TIMESTAMPTZ => t.to_sql(ty, out),
                Type::TIMESTAMP => t.naive_utc().to_sql(ty, out),
                Type::INT8 => crate::micros(*t).to_sql(ty, out),
                Type::TEXT | Type::VARCHAR => t.to_rfc3339().to_sql(ty, out),
                _ => Err(mismatch("a timestamp")),
            },
            Value::Json(j) => match *ty {
                Type::JSON | Type::JSONB => j.to_sql(ty, out),
                Type::TEXT | Type::VARCHAR | Type::UNKNOWN => j.to_string().to_sql(ty, out),
                Type::BYTEA => j.to_string().into_bytes().to_sql(ty, out),
                _ => Err(mismatch("json")),
            },
        }
    }

    /// Every type, because the conversion above decides per type and
    /// reports a mismatch with the target named; a blanket `false` here
    /// would report "cannot convert" with no type.
    fn accepts(_ty: &Type) -> bool {
        true
    }

    tokio_postgres::types::to_sql_checked!();
}

/// One column of a row into a [`Value`], by the column's declared type.
fn read_column(row: &tokio_postgres::Row, i: usize, ty: &Type, name: &str) -> Result<Value> {
    let decode = |e: tokio_postgres::Error| Error::Decode(name.to_string(), e.to_string());
    macro_rules! get {
        ($t:ty, $wrap:expr) => {
            row.try_get::<usize, Option<$t>>(i)
                .map_err(decode)
                .map(|v| v.map($wrap).unwrap_or(Value::Null))
        };
    }
    match *ty {
        // `SELECT pg_advisory_xact_lock(...)` and friends return a row of
        // nothing; a caller that fetches it gets a null rather than a decode
        // error about a type it never asked for.
        Type::VOID => Ok(Value::Null),
        Type::BOOL => get!(bool, Value::Bool),
        Type::INT2 => get!(i16, |v| Value::Integer(v as i64)),
        Type::INT4 => get!(i32, |v| Value::Integer(v as i64)),
        Type::INT8 => get!(i64, Value::Integer),
        Type::OID => get!(u32, |v| Value::Integer(v as i64)),
        Type::FLOAT4 => get!(f32, |v| Value::Real(v as f64)),
        Type::FLOAT8 => get!(f64, Value::Real),
        Type::TEXT | Type::VARCHAR | Type::NAME | Type::BPCHAR | Type::UNKNOWN => {
            get!(String, Value::Text)
        }
        Type::BYTEA => get!(Vec<u8>, Value::Blob),
        Type::UUID => get!(uuid::Uuid, Value::Uuid),
        Type::TIMESTAMPTZ => get!(chrono::DateTime<chrono::Utc>, Value::Timestamp),
        Type::TIMESTAMP => get!(chrono::NaiveDateTime, |v| Value::Timestamp(v.and_utc())),
        Type::JSON | Type::JSONB => get!(serde_json::Value, Value::Json),
        Type::TEXT_ARRAY | Type::VARCHAR_ARRAY => {
            get!(Vec<String>, |v| Value::Json(serde_json::Value::from(v)))
        }
        Type::UUID_ARRAY => get!(Vec<uuid::Uuid>, |v| Value::Json(serde_json::Value::from(
            v.into_iter().map(|u| u.to_string()).collect::<Vec<_>>()
        ))),
        Type::INT8_ARRAY => get!(Vec<i64>, |v| Value::Json(serde_json::Value::from(v))),
        _ => Err(Error::Decode(
            name.to_string(),
            format!("postgres type {ty} is not decoded by hive-db; cast it in the statement"),
        )),
    }
}

/// The error and every cause under it, joined, because the driver's outer
/// message says where it failed and the source says why.
pub(crate) fn describe_error(e: &tokio_postgres::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

/// The SQLSTATE and message of a server-side error, when this is one.
pub(crate) fn db_error(e: &tokio_postgres::Error) -> Option<(String, String)> {
    e.as_db_error()
        .map(|d| (d.code().code().to_string(), d.message().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_renumbered_outside_literals() {
        assert_eq!(
            rewrite_placeholders("SELECT ?1, ?2 WHERE a = '?3' AND \"?4\" = ?10"),
            "SELECT $1, $2 WHERE a = '?3' AND \"?4\" = $10"
        );
        assert_eq!(
            rewrite_placeholders("-- ?1 in a comment\nSELECT ?1 /* ?2 */"),
            "-- ?1 in a comment\nSELECT $1 /* ?2 */"
        );
        assert_eq!(
            rewrite_placeholders("SELECT 'it''s ?1' , ?1"),
            "SELECT 'it''s ?1' , $1"
        );
        assert_eq!(rewrite_placeholders("SELECT 1"), "SELECT 1");
        assert_eq!(rewrite_placeholders("SELECT 'é' = ?1"), "SELECT 'é' = $1");
    }

    #[test]
    fn a_query_parameter_is_taken_out_of_the_url() {
        assert_eq!(
            take_param(
                "postgres://u@h/d?sslmode=disable&sslrootcert=/ca.pem&x=1",
                "sslrootcert"
            ),
            (
                "postgres://u@h/d?sslmode=disable&x=1".to_string(),
                Some("/ca.pem".to_string())
            )
        );
        assert_eq!(
            take_param("postgres://u@h/d?sslrootcert=/ca.pem", "sslrootcert"),
            ("postgres://u@h/d".to_string(), Some("/ca.pem".to_string()))
        );
        assert_eq!(
            take_param("postgres://u@h/d", "sslrootcert"),
            ("postgres://u@h/d".to_string(), None)
        );
    }

    #[test]
    fn describe_never_carries_the_password() {
        let c = tokio_postgres::Config::from_str("postgres://alice:hunter2@db.example:6543/hive")
            .unwrap();
        let d = describe(&c);
        assert_eq!(d, "postgres://alice@db.example:6543/hive");
        assert!(!d.contains("hunter2"));
    }
}

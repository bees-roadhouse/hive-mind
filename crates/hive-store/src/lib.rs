//! The data layer and the grant predicate, over the one database file (D38).
//!
//! The single enforcement point for "may this actor do this" (D1.4): no
//! handler, guest, tool or workflow step composes its own access check, and
//! nothing outside this crate may reference the grants table (invariant 1).
//!
//! # Where the fourteen invariants live in the Rust tree
//!
//! From `CLAUDE.md`, by number. "SQL" means the invariant is enforced by the
//! migration (a trigger, a constraint, an index) and its test runs against
//! it; "predicate" means the SQL text this crate owns in `predicate.rs`, which
//! `Guard` composes into every point check and every set read; a crate name
//! means the test lives with the behaviour in that crate.
//!
//! | # | invariant | enforced by | tested in |
//! |---|---|---|---|
//! | 1 | absence of scope is deny | predicate; `Guard` is the only caller | `tests/invariants.rs`, `tests/grants.rs` |
//! | 2 | the credential pins author and principal | SQL, `credentials_issue_check`; every writer here pins from the credential | `tests/invariants.rs`, `tests/agentruns.rs` |
//! | 3 | ownership is a property of a reference, not of bytes | hive-blob, `Catalog` | `hive-blob/tests/catalog.rs` |
//! | 4 | the events table is the transport | SQL triggers here; the tailer in hive-bus | `tests/events.rs`, `hive-bus/tests` |
//! | 5 | guests hold no sockets | hive-wasmhost, the WASI allowlist | `hive-wasmhost/tests` |
//! | 6 | the step log is a checkpoint journal | hive-workflow | **not yet** (bound to a design, as in Go) |
//! | 7 | every blocking host function completes on cancellation | hive-wasmhost, the call deadline | `hive-wasmhost/tests` |
//! | 8 | no blob without a ref | hive-blob, `Catalog::publish` takes a transaction; SQL trigger | `hive-blob/tests/catalog.rs` |
//! | 9 | untrusted content never reaches instruction position | hive-chat, hive-wasmhost | `hive-chat/tests`, `hive-wasmhost/tests` |
//! | 10 | money-spending steps are at-most-once | SQL (`agent_runs_turn_uq`); `AgentRunStore::finish_run`, the chat reclaimers | `tests/agentruns.rs`, `tests/chat.rs` |
//! | 11 | a check that accepts the fact it decides is not a check | predicate: it resolves its own facts; `Guard` takes no owner | `tests/invariants.rs` |
//! | 12 | trust is structural in the ABI | hive-wasmhost | `hive-wasmhost/tests` |
//! | 13 | the API is reachable over a unix socket | hive-sandbox, the daemon | `hive-sandbox/tests` |
//! | 14 | a key that omits a dimension is a bypass | every crate; the install table prefix | `tests/invariants.rs`, `tests/installs.rs`, `hive-wasmhost/tests` |
//!
//! A row that says **not yet** is a debt this table makes visible. It moves to
//! a test name when the crate lands, never to "done".

mod actors;
mod agentruns;
mod appdata;
mod appschema;
mod bootstrap;
mod builds;
mod chat;
mod credentials;
mod docblobs;
mod events;
mod grants;
mod guestblobs;
mod guestevents;
mod installs;
mod predicate;

pub use actors::{Actor, actor_by_id};
pub use agentruns::{AgentRunStore, RunWriter, reclaim_abandoned_runs};
pub use appdata::{AppData, InstallInfo, resolve_active_install};
pub use appschema::{apply_schema_plan, drop_schema_plan};
pub use bootstrap::{BootstrapConfig, BootstrapResult, bootstrap};
pub use builds::{BuildSpec, RegisteredBuild, register_build};
pub use chat::{
    Chat, ClaimedTurn, Conversation, Message, RunEvent, TURN_CLAIMED, TURN_DONE, TURN_FAILED,
    TURN_PENDING, Turn, TurnState,
};
pub use credentials::{
    CredentialDetail, credential_detail_by_token, ensure_bootstrap_credential, hash_token,
    issue_credential, new_token, resolve_credential,
};
pub use docblobs::descriptors_in;
pub use events::{
    Cursor, EVENT_COLUMNS, Event, EventWake, append_events, head, now, parse_cursor,
    resolve_cursor, tail, tail_window, valid_event_kind, wake as event_wake,
};
pub use grants::{
    Access, ActingInstall, GrantSource, GrantSpec, Guard, Reason, Subject, SubjectKind,
    UnshareResult, enter_break_glass, materialize_inherited, revoke_grant, unshare, write_grant,
};
pub use guestblobs::GuestBlobs;
pub use guestevents::{GuestEvents, platform_kind, visible_to};
pub use hive_db::{Conn, Connection, Db, Transaction};
pub use hive_identity::{Credential, Owner, PrincipalKind};
pub use hive_schema::{AUDIT_MIGRATIONS, MIGRATIONS, MigrateError, Migration, migrate, migrate_audit};
pub use installs::{
    CAPABILITY_ACTIVATE, InstallSpec, activate_install, grant_install_authority,
    revoke_install_authority, stage_install,
};

use std::path::Path;

/// Everything the store can refuse or fail with.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The predicate said no. Callers must not distinguish "no such row" from
    /// "not allowed to see it" any further than this ... the difference is an
    /// existence oracle.
    #[error("denied")]
    Denied,
    /// An act D19 reserves for a person was attempted by an AI actor.
    #[error("this act requires a human actor")]
    NotHuman(String),
    /// A token resolved to nothing live. Callers must not tell "no such token"
    /// apart from "revoked token": the difference is an oracle.
    #[error("no live credential")]
    NoCredential,
    /// A refusal the caller can fix: an empty message, an unknown role, a
    /// conversation with no runtime. An HTTP layer maps it to 400.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The database was already bootstrapped as something else.
    #[error("already bootstrapped {0}")]
    AlreadyBootstrapped(String),
    /// A kind that cannot be written.
    #[error("store: event kind is not a dotted identifier: {0:?}")]
    BadEventKind(String),
    /// Unshare would remove a directly-issued grant and the caller did not say
    /// it meant to.
    #[error("unshare would delete a direct grant: {0}")]
    WouldDeleteDirectGrant(String),
    /// A name reached DDL without surviving validation.
    #[error("store: identifier is not safe for DDL: {0}")]
    UnsafeIdentifier(String),
    /// A manifest feature the host accepts but cannot yet provision.
    #[error("store: not implemented: {0}")]
    NotImplemented(String),
    /// The row a caller named is not there. The `pgx.ErrNoRows` of the Go tree,
    /// for the writers that reported it.
    #[error("no rows")]
    NoRows,
    /// A capability verb's refusal, carrying the status the guest sees.
    #[error(transparent)]
    Host(#[from] hive_wasmhost::HostError),
    #[error(transparent)]
    Blob(#[from] hive_blob::BlobError),
    #[error(transparent)]
    Credential(#[from] hive_identity::IncompleteCredential),
    #[error(transparent)]
    Migrate(#[from] MigrateError),
    #[error("{0}: {1}")]
    Db(String, #[source] hive_db::Error),
    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for StoreError {
    /// A document or a result that would not encode. It is the host's own
    /// serialisation failing, never the guest's input (that is decoded with an
    /// explicit, non-echoing message), so the error's text is safe to carry.
    fn from(e: serde_json::Error) -> StoreError {
        StoreError::Host(hive_wasmhost::HostError::error(format!("encode: {e}")))
    }
}

impl StoreError {
    pub fn is_denied(&self) -> bool {
        matches!(self, StoreError::Denied)
    }

    pub(crate) fn db(what: impl Into<String>, e: hive_db::Error) -> StoreError {
        StoreError::Db(what.into(), e)
    }

    /// The engine's result code behind this, when there is one: a constraint
    /// or a trigger's `RAISE(ABORT)` is 19.
    pub fn db_code(&self) -> Option<i32> {
        match self {
            StoreError::Db(_, e) => e.sqlite_code(),
            _ => None,
        }
    }

    /// The engine's message behind this, for tests that assert on which
    /// trigger refused.
    pub fn db_message(&self) -> Option<String> {
        match self {
            StoreError::Db(_, e) => Some(e.message()),
            _ => None,
        }
    }

    /// Whether the database itself refused the write: a CHECK, a UNIQUE, a
    /// foreign key, or a trigger.
    pub fn is_constraint(&self) -> bool {
        matches!(self, StoreError::Db(_, e) if e.is_constraint())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// The platform's file inside a data directory.
pub const DB_FILE: &str = "hive.db";
/// The override audit's file beside it (D38 §3).
pub const AUDIT_FILE: &str = "hive-audit.db";

/// Holds the database files. Open it once per process.
#[derive(Clone)]
pub struct Store {
    db: Db,
    audit: Db,
}

impl Store {
    /// Opens (creating) the platform file and the audit file in `data_dir`
    /// and verifies a connection to each. It does not migrate; call
    /// [`migrate`] and [`migrate_audit`] explicitly so a read-only role can
    /// open the store.
    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Store> {
        let dir = data_dir.as_ref();
        let db = Db::open(dir.join(DB_FILE))
            .await
            .map_err(|e| StoreError::db("open", e))?;
        let audit = Db::open(dir.join(AUDIT_FILE))
            .await
            .map_err(|e| StoreError::db("open audit", e))?;
        for d in [&db, &audit] {
            let c = d.conn().await.map_err(|e| StoreError::db("connect", e))?;
            hive_db::query("SELECT 1")
                .execute(&c)
                .await
                .map_err(|e| StoreError::db("ping", e))?;
        }
        Ok(Store::from_dbs(db, audit))
    }

    /// Wraps files the caller already opened. The daemon uses `open`; this
    /// exists for tests and for tooling.
    pub fn from_dbs(db: Db, audit: Db) -> Store {
        Store { db, audit }
    }

    /// The platform file, for subsystems that need their own connections and
    /// transactions. It deliberately exposes no query helper that bypasses
    /// the guard.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The override audit's file. Append-only evidence; nothing authorizes
    /// against it and nothing in the daemon reads it on a request path.
    pub fn audit(&self) -> &Db {
        &self.audit
    }

    /// The in-process wakeup bell `append_events` rings. The bus listens to
    /// it in place of `LISTEN`; it is a hint and nothing more (invariant 4),
    /// and a process that is not this one polls.
    pub fn wake(&self) -> &'static EventWake {
        events::wake()
    }

    /// A write transaction: `BEGIN IMMEDIATE` on a fresh connection.
    pub async fn begin(&self) -> Result<Transaction> {
        self.db
            .begin()
            .await
            .map_err(|e| StoreError::db("begin", e))
    }

    /// A guard over this store. Reads go through the connection the caller
    /// hands each method; audit rows land in the audit FILE, on a connection
    /// of their own, because an override audit records that something
    /// happened and riding the caller's transaction would let a read stream
    /// rows to a client and then roll the evidence back with everything else.
    pub fn guard(&self) -> Guard {
        Guard::new(self.audit.clone())
    }

    /// One connection, for the multi-statement reads the guard needs outside
    /// a transaction.
    pub async fn conn(&self) -> Result<Conn> {
        self.db
            .conn()
            .await
            .map_err(|e| StoreError::db("connect", e))
    }

    /// Nothing to close: connections are per operation and the file needs no
    /// farewell. Kept so the daemon's shutdown reads the same as before.
    pub async fn close(&self) {}
}

/// The point check's SQL with every argument a placeholder, for the one test
/// fixture that binds the predicate's clock to cross a break-glass window
/// without sleeping. Hidden and named for what it is: nothing in the daemon
/// calls it, and `Guard` remains the only auditing entry point.
#[doc(hidden)]
pub fn __predicate_sql_for_tests() -> String {
    grants::point_sql()
}

/// Commits a transaction, naming the unit of work in the error.
pub(crate) async fn commit(tx: Transaction, what: &str) -> Result<()> {
    tx.commit()
        .await
        .map_err(|e| StoreError::db(format!("commit {what}"), e))
}

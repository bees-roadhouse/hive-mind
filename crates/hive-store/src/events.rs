//! The append-only log: the transport of record.

use std::fmt;
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, TimeZone, Utc};
use hive_db::{Connection, Row, query};
use hive_identity::{Credential, Owner, PrincipalKind};
use regex::Regex;
use uuid::Uuid;

use crate::grants::{Guard, Subject, SubjectKind};
use crate::predicate::{self, Args};
use crate::{Result, StoreError};

/// The format a kind must take, and it is the same alphabet the CHECK on
/// events.kind carries. Two copies is the deliberate trade named there: the
/// column is what holds for every writer including a `sqlite3` session, and
/// this is what gives a caller an error naming the field rather than a
/// constraint violation from three layers down.
static EVENT_KIND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]{0,127}$").expect("static regex"));

/// Rejects a kind before it can reach a subscriber's frame.
///
/// A kind is written into the `event:` field of an SSE frame, so a control
/// character in one splits a single event into two frames and lets the second
/// carry an `id:` the server had decided must not be written. Rejecting at the
/// writer is not a substitute for the column constraint and the column
/// constraint is not a substitute for this ... they fail for different callers.
pub fn valid_event_kind(kind: &str) -> Result<()> {
    if !EVENT_KIND.is_match(kind) {
        return Err(StoreError::BadEventKind(kind.to_string()));
    }
    Ok(())
}

/// A position in the events stream.
///
/// It is a PAIR, not an id. On Postgres that was forced by partitioning; here
/// it is kept because a replica reading its own copy of the file (D38 phase 3)
/// sees rows arrive behind the primary exactly as a late-committing
/// transaction did, and a position with a time is what lets the tailer sweep
/// a window behind itself. The id breaks ties and makes the position exact.
///
/// See docs/events-tailing.md.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// `None` is the zero time: a cursor that names no timestamp.
    pub at: Option<DateTime<Utc>>,
    pub id: i64,
}

impl Cursor {
    pub fn new(at: DateTime<Utc>, id: i64) -> Cursor {
        Cursor { at: Some(at), id }
    }

    /// A cursor that names a time and no row: a watermark.
    pub fn at_time(at: DateTime<Utc>) -> Cursor {
        Cursor {
            at: Some(at),
            id: 0,
        }
    }

    /// Whether the cursor names no position.
    pub fn is_zero(&self) -> bool {
        self.id == 0 && self.at.is_none()
    }

    /// The timestamp, with the Unix epoch standing in for "no time". The Go
    /// tree's zero `time.Time` (year 1) played the same role; both sort below
    /// every row that will ever exist.
    pub fn at_or_epoch(&self) -> DateTime<Utc> {
        self.at.unwrap_or(DateTime::UNIX_EPOCH)
    }

    /// Whether `self` sorts before `other` under (created_at, id).
    pub fn before(&self, other: &Cursor) -> bool {
        let (a, b) = (self.at_or_epoch(), other.at_or_epoch());
        if a != b {
            return a < b;
        }
        self.id < other.id
    }
}

impl fmt::Display for Cursor {
    /// Encodes the cursor for an SSE `id:` field: microseconds since epoch, a
    /// hyphen, then the row id. Clients treat it as opaque and hand it back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return Ok(());
        }
        let micros = self.at.map(|t| t.timestamp_micros()).unwrap_or(0);
        write!(f, "{micros}-{}", self.id)
    }
}

/// Decodes what a client sent back in Last-Event-ID.
///
/// A bare integer is accepted and yields an id with no timestamp, because an
/// older client (or a hand-written curl) may hold an id-only cursor. The
/// caller resolves the timestamp with one lookup; see [`resolve_cursor`].
pub fn parse_cursor(s: &str) -> Result<Cursor> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Cursor::default());
    }
    let bad = |what: &str| StoreError::InvalidInput(format!("cursor {s:?}: {what}"));
    match s.split_once('-') {
        None => {
            let n: i64 = s.parse().map_err(|_| bad("not an integer"))?;
            Ok(Cursor { at: None, id: n })
        }
        Some((micros, id)) => {
            let m: i64 = micros.parse().map_err(|_| bad("bad timestamp"))?;
            let n: i64 = id.parse().map_err(|_| bad("bad id"))?;
            let at = Utc
                .timestamp_micros(m)
                .single()
                .ok_or_else(|| bad("bad timestamp"))?;
            Ok(Cursor::new(at, n))
        }
    }
}

/// Fills in a missing timestamp for a bare-id cursor. Call it once per
/// connection, never per poll.
pub async fn resolve_cursor(db: &Connection, c: Cursor) -> Result<Cursor> {
    if c.id == 0 || c.at.is_some() {
        return Ok(c);
    }
    let at: Option<DateTime<Utc>> = query("SELECT created_at FROM events WHERE id = ?1")
        .bind(c.id)
        .fetch_scalar_optional(db)
        .await
        .map_err(|e| StoreError::db(format!("resolve cursor {}", c.id), e))?;
    match at {
        // The id is gone or was never ours. Treat it as "start from the
        // beginning of what we still keep" rather than guessing.
        None => Ok(Cursor::default()),
        Some(at) => Ok(Cursor::new(at, c.id)),
    }
}

/// One row of the append-only log. It is the transport of record: the wakeup
/// bell rung after a write is a hint, and every consumer stays correct if
/// every ring is missed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub id: i64,
    pub created_at: Option<DateTime<Utc>>,
    pub kind: String,
    /// What the event is about, in the shape the predicate takes, so a replay
    /// filters through the same rule as a live read. `None` for an event that
    /// names no grantable subject.
    pub subject: Option<Subject>,
    pub owner: Owner,
    pub author_actor: Uuid,
    pub principal_kind: PrincipalKind,
    pub principal_id: Uuid,
    /// Raw JSON.
    pub body: Vec<u8>,
    pub trust: String,
    pub cause_depth: i32,
    pub run_id: Option<Uuid>,
    pub origin: String,
    pub origin_id: Option<String>,
}

impl Event {
    /// A new event owned and authored by one credential's principal, with no
    /// subject. Everything else defaults the way `append_events` defaults it.
    pub fn new(kind: impl Into<String>, cred: &Credential, body: Vec<u8>) -> Event {
        Event {
            id: 0,
            created_at: None,
            kind: kind.into(),
            subject: None,
            owner: cred.owner_of(),
            author_actor: cred.actor_id,
            principal_kind: cred.principal_kind,
            principal_id: cred.principal_id,
            body,
            trust: String::new(),
            cause_depth: 0,
            run_id: None,
            origin: String::new(),
            origin_id: None,
        }
    }

    /// The event's own position.
    pub fn cursor(&self) -> Cursor {
        Cursor {
            at: self.created_at,
            id: self.id,
        }
    }
}

pub const EVENT_COLUMNS: &str = "id, created_at, kind, subject_kind, subject_id, subject_name,
    owner_kind, owner_id, author_actor, principal_kind, principal_id,
    body, trust, cause_depth, run_id, origin, origin_id";

pub(crate) fn scan_event(row: &Row) -> Result<Event> {
    let subject_kind: Option<String> = row.get("subject_kind");
    let subject_id: Option<Uuid> = row.get("subject_id");
    let subject_name: Option<String> = row.get("subject_name");
    let owner_kind: String = row.get("owner_kind");
    let principal_kind: String = row.get("principal_kind");
    let body: String = row.get("body");
    let subject = match (
        subject_kind.as_deref().and_then(SubjectKind::parse),
        subject_id,
    ) {
        (Some(kind), Some(id)) => Some(Subject {
            kind,
            id,
            name: subject_name,
        }),
        _ => None,
    };
    Ok(Event {
        id: row.get("id"),
        created_at: Some(row.get("created_at")),
        kind: row.get("kind"),
        subject,
        owner: Owner::new(
            PrincipalKind::parse(&owner_kind)
                .ok_or_else(|| StoreError::Other(format!("owner kind {owner_kind:?}")))?,
            row.get("owner_id"),
        ),
        author_actor: row.get("author_actor"),
        principal_kind: PrincipalKind::parse(&principal_kind)
            .ok_or_else(|| StoreError::Other(format!("principal kind {principal_kind:?}")))?,
        principal_id: row.get("principal_id"),
        body: body.into_bytes(),
        trust: row.get("trust"),
        cause_depth: row.get("cause_depth"),
        run_id: row.get("run_id"),
        origin: row.get("origin"),
        origin_id: row.get("origin_id"),
    })
}

fn scan_events(rows: Vec<Row>) -> Result<Vec<Event>> {
    rows.iter().map(scan_event).collect()
}

/// The in-process wakeup bell: what `NOTIFY` was, for the one process that
/// holds the file (D38).
///
/// One per process rather than one per store, because that is what a
/// Postgres channel was ... a name every connection in the process agreed on.
/// A second file open in the same process (tests) rings the same bell, which
/// costs a redundant poll and nothing else: a ring is a hint, never a fact
/// (invariant 4). A process that is not this one ... a replica ... hears
/// nothing and polls, which the tailer does unconditionally anyway.
///
/// `append_events` rings it inside the writer's transaction, before the
/// commit is visible. The tailer therefore reads twice on a ring, once at
/// once and once a moment later, so a commit that lands just after the ring
/// is seen without waiting for the backstop poll.
#[derive(Clone, Default)]
pub struct EventWake {
    inner: Arc<tokio::sync::Notify>,
}

impl EventWake {
    /// Rings the bell. Coalescing: a ring nobody was waiting for is stored
    /// once, so a burst of writes is one wakeup rather than a queue.
    pub fn ring(&self) {
        self.inner.notify_one();
    }

    /// Resolves on the next ring (or the stored one).
    pub async fn wait(&self) {
        self.inner.notified().await;
    }
}

static WAKE: LazyLock<EventWake> = LazyLock::new(EventWake::default);

/// The process-wide bell.
pub fn wake() -> &'static EventWake {
    &WAKE
}

/// Inserts events, filling in each one's `id` and `created_at`, and rings the
/// wakeup bell once for the whole call.
///
/// One ring per unit of work, not per row, for the same reason NOTIFY was once
/// per call (D4.11, D4.17): the bell is a hint and a burst is one hint.
///
/// Pass a transaction when the events have to land with other writes. A
/// journal entry, its mention row and its read grant are one transaction by
/// rule (D13.2), and the event belongs in it.
pub async fn append_events(conn: &Connection, events: &mut [Event]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    for e in events.iter_mut() {
        valid_event_kind(&e.kind)?;
        if e.origin.is_empty() {
            e.origin = "local".into();
        }
        if e.trust.is_empty() {
            e.trust = "trusted".into();
        }
        if e.body.is_empty() {
            e.body = b"{}".to_vec();
        }
        let body: serde_json::Value = serde_json::from_slice(&e.body)
            .map_err(|err| StoreError::InvalidInput(format!("event body is not json: {err}")))?;
        let (subject_kind, subject_id, subject_name) = match &e.subject {
            Some(s) => (Some(s.kind.as_str()), Some(s.id), s.name.as_deref()),
            None => (None, None, None),
        };
        // The host clock, bound here rather than left to the column default:
        // microseconds, and the same clock the bus watermarks against.
        let row = query(
            "INSERT INTO events (created_at, kind, subject_kind, subject_id, subject_name,
                                 owner_kind, owner_id, author_actor,
                                 principal_kind, principal_id,
                                 body, trust, cause_depth, run_id, origin, origin_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             RETURNING id, created_at",
        )
        .bind(hive_db::now())
        .bind(&e.kind)
        .bind(subject_kind)
        .bind(subject_id)
        .bind(subject_name)
        .bind(e.owner.kind.as_str())
        .bind(e.owner.id)
        .bind(e.author_actor)
        .bind(e.principal_kind.as_str())
        .bind(e.principal_id)
        .bind(&body)
        .bind(&e.trust)
        .bind(e.cause_depth)
        .bind(e.run_id)
        .bind(&e.origin)
        .bind(&e.origin_id)
        .fetch_one(conn)
        .await
        .map_err(|err| StoreError::db(format!("append event {:?}", e.kind), err))?;
        e.id = row.get("id");
        e.created_at = Some(row.get("created_at"));
    }
    // The ring is a hint and nothing more. A consumer that never hears it
    // still catches up on its next poll.
    wake().ring();
    Ok(())
}

/// Reads forward from a cursor: the host's view of the log, unfiltered.
///
/// Strictly after the cursor, so a full batch always makes progress. An earlier
/// version took a bare timestamp derived from the newest row DELIVERED, which
/// meant a burst larger than one batch produced a query that returned the same
/// oldest rows forever. Rows that become visible with a position BELOW the
/// cursor are not this function's job; see [`tail_window`].
pub async fn tail(db: &Connection, after: Cursor, limit: i64) -> Result<Vec<Event>> {
    let rows = query(&format!(
        "SELECT {EVENT_COLUMNS}
           FROM events
          WHERE created_at >= ?1
            AND (created_at, id) > (?1, ?2)
          ORDER BY created_at, id
          LIMIT ?3"
    ))
    .bind(after.at_or_epoch())
    .bind(after.id)
    .bind(limit)
    .fetch_all(db)
    .await
    .map_err(|e| StoreError::db("tail events", e))?;
    scan_events(rows)
}

/// Re-reads the recent past: everything from `from` up to and including the
/// cursor.
///
/// This is where the late-visibility rule lives. With one writer per file a
/// row's id and its commit are in order, so on the primary nothing lands
/// behind the cursor; on a replica (D38 phase 3) a batch of the primary's
/// frames arrives whole and late, and rows with positions BELOW what the
/// replica's tailer already read are exactly what `tail` will never return.
/// Sweeping the window behind the cursor and deduping by id is what catches
/// it, and the primary runs the same code so the replica's case is tested
/// every day.
pub async fn tail_window(
    db: &Connection,
    from: DateTime<Utc>,
    to: Cursor,
    limit: i64,
) -> Result<Vec<Event>> {
    let rows = query(&format!(
        "SELECT {EVENT_COLUMNS}
           FROM events
          WHERE created_at >= ?1
            AND (created_at, id) <= (?2, ?3)
          ORDER BY created_at, id
          LIMIT ?4"
    ))
    .bind(from)
    .bind(to.at_or_epoch())
    .bind(to.id)
    .bind(limit)
    .fetch_all(db)
    .await
    .map_err(|e| StoreError::db("sweep events", e))?;
    scan_events(rows)
}

/// The newest cursor in the log, or the zero cursor when it is empty. A
/// subscriber with no Last-Event-ID starts here rather than replaying everything
/// ever written.
pub async fn head(db: &Connection) -> Result<Cursor> {
    let row = query("SELECT created_at, id FROM events ORDER BY created_at DESC, id DESC LIMIT 1")
        .fetch_optional(db)
        .await
        .map_err(|e| StoreError::db("head", e))?;
    Ok(match row {
        None => Cursor::default(),
        Some(r) => Cursor::new(r.get("created_at"), r.get("id")),
    })
}

/// The database clock. Every watermark in the bus is derived from it rather
/// than from a caller's idea of the time; with the engine in-process it is the
/// host clock in the schema's unit, read through the file so a replica's
/// tailer and the primary's agree on what "now" means.
pub async fn now(db: &Connection) -> Result<DateTime<Utc>> {
    let us: i64 = query(&format!("SELECT {}", hive_db::NOW_SQL))
        .fetch_scalar(db)
        .await
        .map_err(|e| StoreError::db("db now", e))?;
    // The column default reads milliseconds; the host writes microseconds.
    // The newer of the two is the honest "now", so a row written a moment ago
    // with a finer clock is never ahead of the watermark derived here.
    let host = hive_db::now();
    let db_now = hive_db::from_micros(us).unwrap_or(host);
    Ok(if host > db_now { host } else { db_now })
}

/// The event feed's visibility rule, as a SQL expression over an events row
/// aliased `e`: the reason the credential may see it, or NULL.
///
/// An event that names a grantable subject follows that subject, so a revoked
/// grant stops replay (D4.13). An event that names none ... a run changing
/// state, a daemon lifecycle note ... falls back to its own owner, because
/// there is nothing to write a grant against. Both branches read the events
/// row THEMSELVES rather than taking its owner as a parameter: there is no
/// signature a caller can pass a mismatched owner through.
///
/// Parameters: `?1` principal kind, `?2` principal id, `?3` actor, `?4` now.
/// The subject branch passes NULL as the acting install, which the predicate
/// refuses to decide for a collection ... and the events table's CHECK is what
/// makes that question never arise: no writer produces a collection-subject
/// event, and the one guard is at the INSERT rather than at every read of the
/// feed.
fn visible_expr() -> String {
    let reason = predicate::reason(&Args {
        subject_kind: "e.subject_kind",
        subject_id: "e.subject_id",
        subject_name: "e.subject_name",
        principal_kind: "?1",
        principal_id: "?2",
        actor_id: "?3",
        access: "'read'",
        now: "?4",
        acting_install: "NULL",
    });
    let acting = predicate::acting_kind("?3", "?1", "?2");
    format!(
        "CASE WHEN e.subject_kind IS NULL OR e.subject_id IS NULL
              THEN CASE WHEN e.owner_kind = ?1 AND e.owner_id = ?2 AND {acting} IS NOT NULL
                        THEN 'owner' END
              ELSE {reason}
         END"
    )
}

impl Guard {
    /// Events strictly after `after` that the credential may see right now.
    ///
    /// "Right now" is the point: replay filters with CURRENT permissions, never
    /// permissions as of the event (D4.13). A revoked grant must not be replayed
    /// around, and because the filter is the same predicate text the live path
    /// uses, it cannot drift from it.
    ///
    /// Note what this does NOT pass: the owner. The rule reads each event row
    /// itself, exactly as the predicate resolves a subject's owner, so there is
    /// no parameter a caller can get wrong. The `since` bound must be at or
    /// before `after.at`.
    pub async fn replay(
        &self,
        db: &Connection,
        cred: &Credential,
        after: Cursor,
        since: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Event>> {
        let visible = visible_expr();
        let rows = query(&format!(
            "SELECT {EVENT_COLUMNS}
               FROM events e
              WHERE e.created_at >= ?5
                AND (e.created_at, e.id) > (?6, ?7)
                AND {visible} IS NOT NULL
              ORDER BY e.created_at, e.id
              LIMIT ?8"
        ))
        .bind(cred.principal_kind.as_str())
        .bind(cred.principal_id)
        .bind(cred.actor_id)
        .bind(hive_db::now())
        .bind(since)
        .bind(after.at_or_epoch())
        .bind(after.id)
        .bind(limit)
        .fetch_all(db)
        .await
        .map_err(|e| StoreError::db("replay events", e))?;
        scan_events(rows)
    }

    /// Filters a batch of already-received events down to the ones this
    /// credential may see, in one round trip.
    ///
    /// This is the live path. One tailer per host receives everything and the
    /// host filters per subscriber after receipt (D4.9), through the same rule
    /// the replay path uses.
    ///
    /// Only ids go to the database. The rows are re-read there rather than
    /// trusting the copies held in memory, which removes any way for a stale
    /// or hand-built event to talk its way past the filter.
    pub async fn visible(
        &self,
        db: &Connection,
        cred: &Credential,
        events: &[Event],
    ) -> Result<Vec<Event>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = events.iter().map(|e| e.id).collect();
        let visible = visible_expr();
        let allowed: Vec<i64> = query(&format!(
            "SELECT e.id
               FROM json_each(?5) AS c
               JOIN events e ON e.id = c.value
              WHERE {visible} IS NOT NULL"
        ))
        .bind(cred.principal_kind.as_str())
        .bind(cred.principal_id)
        .bind(cred.actor_id)
        .bind(hive_db::now())
        .bind(serde_json::Value::from(ids))
        .fetch_scalars(db)
        .await
        .map_err(|e| StoreError::db("filter events", e))?;
        Ok(events
            .iter()
            .filter(|e| allowed.contains(&e.id))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_string_round_trips() {
        let c = Cursor::new(Utc.timestamp_micros(1_700_000_000_123_456).unwrap(), 42);
        let s = c.to_string();
        assert_eq!(s, "1700000000123456-42");
        assert_eq!(parse_cursor(&s).unwrap(), c);
        assert_eq!(Cursor::default().to_string(), "");
        assert_eq!(parse_cursor("").unwrap(), Cursor::default());
        // A bare id is an accepted, unresolved cursor.
        assert_eq!(parse_cursor("17").unwrap(), Cursor { at: None, id: 17 });
        assert!(parse_cursor("x-1").is_err());
        assert!(parse_cursor("1-x").is_err());
    }

    #[test]
    fn kinds_are_dotted_identifiers() {
        assert!(valid_event_kind("journal.entry.created").is_ok());
        assert!(valid_event_kind("evil\nid: 9").is_err());
        assert!(valid_event_kind("").is_err());
        assert!(valid_event_kind(".leading").is_err());
    }
}

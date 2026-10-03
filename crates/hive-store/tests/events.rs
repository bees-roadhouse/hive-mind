//! The events table, the cursor, replay, credentials and the migration
//! invariants that live in the database rather than in a service. Ported from
//! events_test.go, schema_invariants_test.go and migrate_test.go.

mod common;

use chrono::{DateTime, TimeZone, Utc};
use common::{World, cred, user};
use hive_db::query;
use hive_identity::PrincipalKind;
use hive_store::{
    Access, Cursor, Event, GrantSpec, StoreError, Subject, append_events,
    ensure_bootstrap_credential, issue_credential, parse_cursor, resolve_credential, revoke_grant,
    write_grant,
};
use uuid::Uuid;

/// Ported from `TestCursorRoundTrip`.
#[test]
fn cursor_round_trip() {
    let at: DateTime<Utc> = Utc.timestamp_micros(1_736_899_200_123_456).unwrap();
    let c = Cursor::new(at, 4711);
    let back = parse_cursor(&c.to_string()).expect("parse");
    assert_eq!(back, c);

    // A bare id is accepted, because an older client may hold an id-only
    // cursor. It yields an id with no timestamp.
    let bare = parse_cursor("4711").expect("parse bare");
    assert_eq!((bare.id, bare.at), (4711, None));
    assert!(
        parse_cursor("not-a-cursor").is_err(),
        "a malformed cursor parsed"
    );
    assert_eq!(Cursor::default().to_string(), "", "zero cursor rendered");
}

/// Ported from `TestTailPrunesPartitions`, for an engine with no partitions:
/// the property the cursor pair buys here is that a tail from a position is
/// an index walk from that position, never a scan of the whole log. If this
/// fails because the composite query stopped using the cursor index, the doc
/// is wrong and so is the bus. (A query plan is the instrument whose failure
/// mode matches "did it scan".)
#[tokio::test]
async fn tail_walks_the_cursor_index() {
    let w = World::new("tail_walks_the_cursor_index").await;
    let alice = w.human("alice").await;
    let alice_cred = cred(alice, PrincipalKind::User, alice);
    let mut events: Vec<Event> = (0..50)
        .map(|_| Event::new("test.event", &alice_cred, b"{}".to_vec()))
        .collect();
    let conn = w.conn().await;
    append_events(&conn, &mut events).await.expect("append");
    query("ANALYZE").execute(&conn).await.unwrap();

    let plan = |sql: &str| {
        let conn = &conn;
        let sql = format!("EXPLAIN QUERY PLAN {sql}");
        async move {
            query(&sql)
                .fetch_all(conn)
                .await
                .expect("explain")
                .iter()
                .map(|r| r.get::<String>("detail"))
                .collect::<Vec<_>>()
                .join(" | ")
        }
    };
    let composite = plan(
        "SELECT id, created_at FROM events
          WHERE created_at >= 1 AND (created_at, id) > (1, 0)
          ORDER BY created_at, id LIMIT 500",
    )
    .await;
    assert!(
        composite.contains("events_cursor_idx"),
        "the composite tail does not walk the cursor index: {composite}"
    );
    assert!(
        !composite.contains("SCAN events") || composite.contains("USING INDEX"),
        "the composite tail scans the table: {composite}"
    );
    let window = plan(
        "SELECT id, created_at FROM events
          WHERE created_at >= 1 AND (created_at, id) <= (2, 0)
          ORDER BY created_at, id LIMIT 500",
    )
    .await;
    assert!(
        window.contains("events_cursor_idx"),
        "the late-commit sweep does not walk the cursor index: {window}"
    );
}

/// Ported from `TestReplayFiltersWithCurrentPermissions` (D4.13).
#[tokio::test]
async fn replay_filters_with_current_permissions() {
    let w = World::new("replay_filters_current_permissions").await;
    let alice = w.human("alice").await;
    let bob = w.human("bob").await;
    let alice_cred = cred(alice, PrincipalKind::User, alice);
    let bob_cred = cred(bob, PrincipalKind::User, bob);
    let inst = w.install("journal", user(alice), alice).await;
    let entry_id = w
        .entity(inst, "entries", "shared", user(alice), alice)
        .await;
    let subject = Subject::entity(entry_id);

    let mut ev = Event::new(
        "journal.entry.created",
        &alice_cred,
        br#"{"title":"family finances"}"#.to_vec(),
    );
    ev.subject = Some(subject.clone());
    let conn = w.conn().await;
    append_events(&conn, std::slice::from_mut(&mut ev))
        .await
        .expect("append");
    let created = ev.created_at.expect("created_at set");
    let from = Cursor::at_time(created - chrono::Duration::hours(1));
    let since = from.at_or_epoch();
    let g = w.guard();

    let seen = g
        .replay(&conn, &alice_cred, from, since, 100)
        .await
        .expect("replay for alice");
    assert_eq!(seen.len(), 1);
    let seen = g
        .replay(&conn, &bob_cred, from, since, 100)
        .await
        .expect("replay for bob");
    assert_eq!(seen.len(), 0, "bob saw events before the share");

    let grant_id = write_grant(
        &conn,
        &GrantSpec::direct(subject.clone(), user(bob), Access::Read, alice_cred),
    )
    .await
    .expect("share");
    let seen = g
        .replay(&conn, &bob_cred, from, since, 100)
        .await
        .expect("replay after share");
    assert_eq!(seen.len(), 1);

    // THE RULE: the event is unchanged, the permission is gone, and the replay
    // reflects the permission as it is NOW.
    revoke_grant(&conn, grant_id).await.expect("revoke");
    let seen = g
        .replay(&conn, &bob_cred, from, since, 100)
        .await
        .expect("replay after revoke");
    assert_eq!(seen.len(), 0, "bob replayed events around a revoked grant");
    // The live-path filter has to agree with the replay path.
    let live = g
        .visible(&conn, &bob_cred, std::slice::from_ref(&ev))
        .await
        .expect("visible");
    assert!(
        live.is_empty(),
        "the live filter disagreed with the replay filter after revocation"
    );
}

/// Ported from `TestAppendEventsNotifiesOncePerCall` (D4.11). The bell is a
/// hint and a burst is one hint; ringing it per row would make a bus wake
/// per row for no information.
///
/// The counter is process-wide and every other test in this binary rings
/// it, so an exact "before + 1" cannot be asserted without serialising the
/// suite (it was, and it went red the day bootstrap grew slower). What CAN
/// be asserted at any interleaving is the shape of the failure: a bell rung
/// per row adds at least as many rings as rows, and the fixture is sized so
/// that no plausible interference reaches it.
#[tokio::test]
async fn append_events_rings_once_per_call() {
    let w = World::new("append_events_rings_once").await;
    let alice = w.human("alice").await;
    let alice_cred = cred(alice, PrincipalKind::User, alice);
    let wake = hive_store::event_wake();

    const ROWS: usize = 200;
    let mut events: Vec<Event> = (0..ROWS)
        .map(|_| Event::new("test.event", &alice_cred, b"{}".to_vec()))
        .collect();
    let conn = w.conn().await;
    let before = wake.rings();
    append_events(&conn, &mut events).await.expect("append");
    let rang = wake.rings() - before;
    assert!(rang >= 1, "one append_events call rang nothing");
    assert!(
        (rang as usize) < ROWS / 2,
        "one append_events call of {ROWS} rows rang {rang} times: a bell per row"
    );

    // And an empty call rings nothing: there is nothing to tell. Twenty of
    // them ring fewer than twenty times whatever else is running.
    let before = wake.rings();
    for _ in 0..20 {
        append_events(&conn, &mut []).await.expect("append nothing");
    }
    assert!(wake.rings() - before < 20, "an empty append rang the bell");
}

/// Ported from `TestResolveCredentialDeniesOnAbsence`.
#[tokio::test]
async fn resolve_credential_denies_on_absence() {
    let w = World::new("resolve_credential_denies").await;
    let alice = w.human("alice").await;
    let conn = w.conn().await;
    let (token, id) = issue_credential(
        &conn,
        alice,
        user(alice),
        &cred(alice, PrincipalKind::User, alice),
        "cli",
        None,
    )
    .await
    .expect("issue");
    let got = resolve_credential(&conn, &token).await.expect("resolve");
    assert_eq!(
        (got.actor_id, got.principal_id, got.principal_kind),
        (alice, alice, PrincipalKind::User)
    );

    let stored: String = query("SELECT token_sha256 FROM credentials WHERE id = ?1")
        .bind(id)
        .fetch_scalar(&conn)
        .await
        .unwrap();
    assert!(
        !stored.contains(&token),
        "the token was stored in plaintext"
    );

    for bad in ["", "nope", &format!("{token}x")] {
        let err = resolve_credential(&conn, bad)
            .await
            .err()
            .unwrap_or_else(|| panic!("token {bad:?} resolved"));
        assert!(matches!(err, StoreError::NoCredential), "{err}");
    }
    query("UPDATE credentials SET revoked_at = ?2 WHERE id = ?1")
        .bind(id)
        .bind(common::now())
        .execute(&conn)
        .await
        .unwrap();
    assert!(
        resolve_credential(&conn, &token).await.is_err(),
        "a revoked credential resolved"
    );
}

/// Ported from `TestBootstrapCredentialIsIdempotentAndPinned`.
#[tokio::test]
async fn bootstrap_credential_is_idempotent_and_pinned() {
    let w = World::new("bootstrap_credential_idempotent").await;
    let conn = w.conn().await;
    for _ in 0..2 {
        ensure_bootstrap_credential(&conn, w.root, "dev-token")
            .await
            .expect("ensure");
    }
    assert_eq!(w.count("SELECT count(*) FROM credentials").await, 1);
    assert!(
        ensure_bootstrap_credential(&conn, Uuid::new_v4(), "dev-token")
            .await
            .is_err(),
        "the bootstrap token was repointed at another actor"
    );
}

/// Ported from `TestEventKindCannotCarryAFrameSeparator`. Two layers, tested
/// separately: the Rust check names the field; the CHECK holds for a writer
/// that never goes through Rust at all.
#[tokio::test]
async fn event_kind_cannot_carry_a_frame_separator() {
    let w = World::new("event_kind_no_frame_separator").await;
    let alice = w.human("alice").await;
    let alice_cred = cred(alice, PrincipalKind::User, alice);
    let hostile: [(&str, &str, &str); 6] = [
        (
            "newline",
            "note.created\nid: 99999999999999-999999\ndata: {\"stolen\":true}",
            "events_kind_",
        ),
        (
            "carriage return",
            "note.created\rid: 12345-6",
            "events_kind_",
        ),
        // SQLite's text functions stop at a NUL, so the alphabet rule alone
        // would read "note." and pass; the byte-length clause is what refuses
        // it, and it is the same named constraint.
        ("nul", "note.\0created", "events_kind_"),
        ("tab", "note.\tcreated", "events_kind_"),
        ("empty", "", "events_kind_"),
        ("space", "note created", "events_kind_"),
    ];
    let conn = w.conn().await;
    for (name, kind, column_err) in hostile {
        let mut ev = Event::new(kind, &alice_cred, b"{}".to_vec());
        let err = append_events(&conn, std::slice::from_mut(&mut ev))
            .await
            .err()
            .unwrap_or_else(|| panic!("rust/{name}: append_events accepted {kind:?}"));
        assert!(
            matches!(err, StoreError::BadEventKind(_)),
            "rust/{name}: {err}"
        );

        // Straight past the Rust layer, which is the point.
        let err = query(
            "INSERT INTO events (kind, owner_kind, owner_id, author_actor, principal_kind, principal_id, body)
             VALUES (?1,'user',?2,?2,'user',?2,'{}')",
        )
        .bind(kind)
        .bind(alice)
        .execute(&conn)
        .await
        .err()
        .unwrap_or_else(|| panic!("column/{name}: the column accepted {kind:?}"));
        assert!(
            err.to_string().contains(column_err),
            "column/{name}: rejected by something other than {column_err}: {err}"
        );
    }
    for ok in [
        "a",
        "note.created",
        "journal.entry.created",
        "seed-1.a_b.v2",
    ] {
        let mut ev = Event::new(ok, &alice_cred, b"{}".to_vec());
        append_events(&conn, std::slice::from_mut(&mut ev))
            .await
            .unwrap_or_else(|e| panic!("a legitimate kind {ok:?} was refused: {e}"));
    }
}

// --- invariants migration one pushes into the database ---------------------

/// Ported from `TestBlobCannotGoLiveWithoutARef` (D17.5).
#[tokio::test]
async fn blob_cannot_go_live_without_a_ref() {
    let w = World::new("blob_cannot_go_live_without_ref").await;
    let alice = w.human("alice").await;
    let hash = "a".repeat(64);
    let conn = w.conn().await;
    query("INSERT INTO blobs (sha256, size, driver, state, class) VALUES (?1, 10, 'disk', 'pending', 'original')")
        .bind(&hash)
        .execute(&conn)
        .await
        .expect("reserve blob");
    assert!(
        query("UPDATE blobs SET state = 'live', driver_ref = 'x', live_at = ?2 WHERE sha256 = ?1")
            .bind(&hash)
            .bind(common::now())
            .execute(&conn)
            .await
            .is_err(),
        "a blob went live with no reference"
    );
    let tx = w.store.begin().await.unwrap();
    query(
        "INSERT INTO blob_refs (sha256, owner_kind, owner_id, author_actor, source_kind, source_id, trust)
         VALUES (?1, 'user', ?2, ?2, 'upload', 'u1', 'trusted')",
    )
    .bind(&hash)
    .bind(alice)
    .execute(&tx)
    .await
    .unwrap();
    query("UPDATE blobs SET state = 'live', driver_ref = 'x', live_at = ?2 WHERE sha256 = ?1")
        .bind(&hash)
        .bind(common::now())
        .execute(&tx)
        .await
        .expect("go live with a ref");
    tx.commit().await.unwrap();

    query(
        "INSERT INTO blob_refs (sha256, owner_kind, owner_id, author_actor, source_kind, source_id, trust)
         VALUES (?1, 'user', ?2, ?2, 'screenshot', 's1', 'untrusted')",
    )
    .bind(&hash)
    .bind(alice)
    .execute(&conn)
    .await
    .expect("second ref with different trust");
    assert!(
        query("DELETE FROM blobs WHERE sha256 = ?1")
            .bind(&hash)
            .execute(&conn)
            .await
            .is_err(),
        "deleted a blob that still had references"
    );
}

/// Ported from `TestCaptureClassCannotBeEvicted`.
#[tokio::test]
async fn capture_class_cannot_be_evicted() {
    let w = World::new("capture_class_cannot_be_evicted").await;
    let alice = w.human("alice").await;
    let hash = "c".repeat(64);
    let tx = w.store.begin().await.unwrap();
    // The reference first, then live: the trigger is immediate.
    query("INSERT INTO blobs (sha256, size, driver, driver_ref, state, class) VALUES (?1, 10, 'disk', 'x', 'pending', 'capture')")
        .bind(&hash)
        .execute(&tx)
        .await
        .unwrap();
    query(
        "INSERT INTO blob_refs (sha256, owner_kind, owner_id, author_actor, source_kind, source_id, trust)
         VALUES (?1, 'user', ?2, ?2, 'screenshot', 's1', 'untrusted')",
    )
    .bind(&hash)
    .bind(alice)
    .execute(&tx)
    .await
    .unwrap();
    query("UPDATE blobs SET state = 'live', live_at = ?2 WHERE sha256 = ?1")
        .bind(&hash)
        .bind(common::now())
        .execute(&tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        query("UPDATE blobs SET state = 'evicted', evicted_at = ?2 WHERE sha256 = ?1")
            .bind(&hash)
            .bind(common::now())
            .execute(&*w.conn().await)
            .await
            .is_err(),
        "a capture-class blob was evicted"
    );
}

/// Ported from `TestEventsAreAppendOnlyAndOriginDedupes` (D4.12).
#[tokio::test]
async fn events_are_append_only_and_origin_dedupes() {
    let w = World::new("events_append_only_origin_dedupes").await;
    let alice = w.human("alice").await;
    let insert = |origin: &'static str, origin_id: Option<&'static str>| {
        let db = w.db().clone();
        async move {
            query(
                "INSERT INTO events (kind, owner_kind, owner_id, author_actor, principal_kind, principal_id, origin, origin_id)
                 VALUES ('journal.entry.created', 'user', ?1, ?1, 'user', ?1, ?2, ?3)",
            )
            .bind(alice)
            .bind(origin)
            .bind(origin_id)
            .execute(&db.conn().await.unwrap())
            .await
        }
    };
    insert("local", None).await.expect("local");
    insert("local", None)
        .await
        .expect("a second local event was rejected");
    insert("acme-hive", Some("e-1")).await.expect("bridged");
    assert!(
        insert("acme-hive", Some("e-1")).await.is_err(),
        "a duplicate (origin, origin_id) was accepted"
    );
    insert("cloud-hive", Some("e-1"))
        .await
        .expect("same origin_id from a different origin was rejected");
    assert!(
        query("UPDATE events SET kind = 'tampered'")
            .execute(&*w.conn().await)
            .await
            .is_err(),
        "an event row was updated"
    );
    assert!(
        query("DELETE FROM events")
            .execute(&*w.conn().await)
            .await
            .is_err(),
        "an event row was deleted"
    );
}

/// Ported from `TestWorkflowDefinitionsAreImmutable`.
#[tokio::test]
async fn workflow_definitions_are_immutable() {
    let w = World::new("workflow_defs_immutable").await;
    let alice = w.human("alice").await;
    let conn = w.conn().await;
    let id: Uuid = query(
        "INSERT INTO workflow_defs (name, spec, content_hash, owner_kind, owner_id, author_actor)
         VALUES ('mention-notify', '{\"steps\":[]}', ?1, 'user', ?2, ?2) RETURNING id",
    )
    .bind("d".repeat(64))
    .bind(alice)
    .fetch_scalar(&conn)
    .await
    .unwrap();
    assert!(
        query(
            "UPDATE workflow_defs SET spec = '{\"steps\":[{\"type\":\"agent_run\"}]}' WHERE id = ?1"
        )
        .bind(id)
        .execute(&conn)
        .await
        .is_err(),
        "a workflow definition was edited in place"
    );
    query("UPDATE workflow_defs SET enabled = 0 WHERE id = ?1")
        .bind(id)
        .execute(&conn)
        .await
        .expect("disable def");
}

/// Ported from `TestAgentRunIsAtMostOnceEverywhere` (invariant 10).
#[tokio::test]
async fn agent_run_is_at_most_once_everywhere() {
    let w = World::new("agent_run_at_most_once").await;
    let alice = w.human("alice").await;
    let conn = w.conn().await;
    let def_id: Uuid = query(
        "INSERT INTO workflow_defs (name, spec, content_hash, owner_kind, owner_id, author_actor)
         VALUES ('x', '{}', ?1, 'user', ?2, ?2) RETURNING id",
    )
    .bind("e".repeat(64))
    .bind(alice)
    .fetch_scalar(&conn)
    .await
    .unwrap();
    let run_id: Uuid = query(
        "INSERT INTO workflow_runs (def_id, definition_hash, actor_id, owner_kind, owner_id)
         VALUES (?1, ?2, ?3, 'user', ?3) RETURNING id",
    )
    .bind(def_id)
    .bind("e".repeat(64))
    .bind(alice)
    .fetch_scalar(&conn)
    .await
    .unwrap();
    assert!(
        query(
            "INSERT INTO workflow_steps (run_id, seq, name, type, retry_policy, max_attempts)
             VALUES (?1, 1, 'summarize', 'agent_run', 'at_least_once', 3)",
        )
        .bind(run_id)
        .execute(&conn)
        .await
        .is_err(),
        "an agent_run step was declared retryable"
    );
    query(
        "INSERT INTO workflow_steps (run_id, seq, name, type, retry_policy, max_attempts)
         VALUES (?1, 1, 'summarize', 'agent_run', 'at_most_once', 1)",
    )
    .bind(run_id)
    .execute(&conn)
    .await
    .expect("at-most-once agent_run");
    assert!(
        query("UPDATE workflow_steps SET state = 'indeterminate' WHERE run_id = ?1")
            .bind(run_id)
            .execute(&conn)
            .await
            .is_err(),
        "a step went indeterminate with no reason"
    );
    query("UPDATE workflow_steps SET state = 'indeterminate', indeterminate_reason = 'lease reclaimed' WHERE run_id = ?1")
        .bind(run_id)
        .execute(&conn)
        .await
        .expect("indeterminate with a reason");
}

/// Ported from `TestUntrustedRunHasNoEgress` (D17.3).
#[tokio::test]
async fn untrusted_workflow_run_has_no_egress() {
    let w = World::new("untrusted_run_no_egress").await;
    let alice = w.human("alice").await;
    let conn = w.conn().await;
    let def_id: Uuid = query(
        "INSERT INTO workflow_defs (name, spec, content_hash, owner_kind, owner_id, author_actor)
         VALUES ('x', '{}', ?1, 'user', ?2, ?2) RETURNING id",
    )
    .bind("f".repeat(64))
    .bind(alice)
    .fetch_scalar(&conn)
    .await
    .unwrap();
    assert!(
        query(
            "INSERT INTO workflow_runs (def_id, definition_hash, actor_id, owner_kind, owner_id, trust, egress_allowed)
             VALUES (?1, ?2, ?3, 'user', ?3, 'untrusted', 1)",
        )
        .bind(def_id)
        .bind("f".repeat(64))
        .bind(alice)
        .execute(&conn)
        .await
        .is_err(),
        "an untrusted run was given egress"
    );
}

// --- the migrator and the clock ---------------------------------------------

/// Ported from `TestMigrationsLoad`. No database: the embedded set parses and
/// is ordered, so a misnamed file fails the gate anywhere.
#[test]
fn migrations_load() {
    let migrations = hive_store::MIGRATIONS;
    assert!(!migrations.is_empty());
    assert_eq!(migrations[0].version, "0001");
    for m in migrations {
        assert_eq!(m.checksum().len(), 64, "{}", m.version);
        assert!(!m.sql.is_empty(), "migration {} is empty", m.version);
    }
}

/// Ported from `TestMigrateConcurrent`: two daemons booting at once must not
/// both run migration one. A bare file rather than the fixture, which has
/// already migrated; six racers on one file, and the write lock is the
/// mutex.
#[tokio::test]
async fn migrate_concurrent() {
    let dir = tempfile::tempdir().unwrap();
    let db = hive_db::Db::open(dir.path().join("hive.db")).await.unwrap();
    let results = futures::future::join_all((0..6).map(|_| hive_store::migrate(&db))).await;
    let mut applied = 0;
    for (i, r) in results.into_iter().enumerate() {
        let ran = r.unwrap_or_else(|e| panic!("racer {i}: {e}"));
        applied += ran.len();
    }
    assert_eq!(
        applied,
        hive_store::MIGRATIONS.len(),
        "migration one ran more than once"
    );
    let n: i64 = query("SELECT count(*) FROM schema_migrations")
        .fetch_scalar(&db.conn().await.unwrap())
        .await
        .unwrap();
    assert_eq!(n as usize, hive_store::MIGRATIONS.len());
}

/// Ported from `TestFutureEventCannotWedgeAPartition`. There is no partition
/// to wedge any more; what remains is the rule it protected, that created_at
/// is a local ingest time and a row dated far ahead is refused while a
/// minute of clock skew is not.
#[tokio::test]
async fn a_future_event_is_refused_and_a_minute_of_skew_is_not() {
    let w = World::new("future_event_refused").await;
    let alice = w.human("alice").await;
    let insert = |offset: chrono::Duration| {
        let db = w.db().clone();
        async move {
            query(
                "INSERT INTO events (created_at, kind, owner_kind, owner_id, author_actor, principal_kind, principal_id)
                 VALUES (?2, 'probe', 'user', ?1, ?1, 'user', ?1)",
            )
            .bind(alice)
            .bind(common::now() + offset)
            .execute(&db.conn().await.unwrap())
            .await
        }
    };
    assert!(
        insert(chrono::Duration::days(90)).await.is_err(),
        "an event dated three months out was accepted"
    );
    insert(chrono::Duration::minutes(1))
        .await
        .expect("a minute of clock skew was rejected");
}

/// The database clock and the host clock agree to within a second, and the
/// bus's watermark is derived from the former. A replica's tailer and the
/// primary's have to mean the same thing by "now".
#[tokio::test]
async fn the_database_clock_is_the_host_clock() {
    let w = World::new("database_clock").await;
    let conn = w.conn().await;
    let db_now = hive_store::now(&conn).await.expect("now");
    let skew = (common::now() - db_now).num_milliseconds().abs();
    assert!(skew < 1000, "database clock is {skew}ms off the host clock");
}

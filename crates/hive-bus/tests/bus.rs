//! The invariant this crate exists to hold, and the fan-out. Ported from
//! bus_test.go; each test names its origin.

use std::collections::HashSet;
use std::time::Duration;

use hive_bus::{Bus, Config};
use hive_db::{Db, query};
use hive_identity::{Credential, Owner, PrincipalKind};
use hive_store::{BootstrapConfig, Event, Store, append_events};
use hive_testdb::TestDb;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Harness {
    db: TestDb,
    store: Store,
    alice: Uuid,
    cred: Credential,
    cancel: CancellationToken,
    runs: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn new(test: &str) -> Harness {
        let db = TestDb::new(test).await;
        let store = Store::from_dbs(db.db().clone(), db.audit().clone());
        let res = store
            .bootstrap_in_tx(&BootstrapConfig {
                root_handle: "alice".into(),
                root_name: "Alice".into(),
                ..Default::default()
            })
            .await
            .expect("bootstrap");
        let alice = res.root_actor_id;
        Harness {
            db,
            store,
            alice,
            cred: Credential::new(alice, PrincipalKind::User, alice),
            cancel: CancellationToken::new(),
            runs: Vec::new(),
        }
    }

    fn db(&self) -> &Db {
        self.db.db()
    }

    async fn conn(&self) -> hive_db::Conn {
        self.store.conn().await.expect("checkout")
    }

    fn owner(&self) -> Owner {
        Owner::user(self.alice)
    }

    /// Starts a bus and waits until it has tailed once.
    async fn run(&mut self, cfg: Config) -> Bus {
        let b = Bus::new(self.db().clone(), cfg);
        let bus = b.clone();
        let cancel = self.cancel.clone();
        self.runs
            .push(tokio::spawn(async move { bus.run(cancel).await }));
        tokio::time::timeout(Duration::from_secs(10), b.ready())
            .await
            .expect("bus never became ready");
        b
    }

    async fn stop(mut self) {
        self.cancel.cancel();
        for r in self.runs.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(10), r).await;
        }
    }

    async fn human(&self, handle: &str) -> Uuid {
        let id = Uuid::new_v4();
        query(
            "INSERT INTO actors (id, kind, handle, display_name, principal_kind, principal_id, created_by_actor)
             VALUES (?1, 'human', ?2, ?2, 'user', ?1, ?3)",
        )
        .bind(id)
        .bind(handle)
        .bind(self.alice)
        .execute(&*self.conn().await)
        .await
        .unwrap();
        id
    }

    async fn append(&self, kind: &str, owner: Owner) -> Event {
        let mut ev = Event::new(
            kind,
            &self.cred,
            format!("{{\"kind\":{kind:?}}}").into_bytes(),
        );
        ev.owner = owner;
        append_events(&*self.conn().await, std::slice::from_mut(&mut ev))
            .await
            .expect("append");
        ev
    }
}

/// Drains a subscription until it has n events or the deadline passes.
async fn collect(sub: &mut hive_bus::Subscription, n: usize, within: Duration) -> Vec<Event> {
    let deadline = tokio::time::Instant::now() + within;
    let mut got = Vec::new();
    while got.len() < n {
        match tokio::time::timeout_at(deadline, sub.recv()).await {
            Ok(Some(batch)) => got.extend(batch),
            Ok(None) | Err(_) => break,
        }
    }
    got
}

fn quiet(poll: Duration, overlap: Duration) -> Config {
    Config {
        listen: false,
        poll_interval: poll,
        overlap,
        ..Config::default()
    }
}

/// Ported from `TestCorrectWithEveryNotificationDropped`: THE test. If this
/// fails, "a missed ring is a latency event, never a correctness
/// event" is false.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correct_with_every_notification_dropped() {
    let mut h = Harness::new("bus_correct_without_notifications").await;
    // A batch limit under the event count, so the inner catch-up loop runs.
    let b = h
        .run(Config {
            batch_limit: 8,
            ..quiet(Duration::from_millis(150), Duration::from_secs(2))
        })
        .await;
    let mut sub = b.subscribe(256);
    let mut want = HashSet::new();
    for i in 0..30 {
        want.insert(h.append(&format!("test.event.{i}"), h.owner()).await.id);
    }
    let got = collect(&mut sub, 30, Duration::from_secs(15)).await;
    assert_eq!(
        got.len(),
        30,
        "received {} of 30 events with notifications dropped",
        got.len()
    );
    for e in &got {
        want.remove(&e.id);
    }
    assert!(want.is_empty(), "{} events never arrived", want.len());
    let (notifications, polls) = b.stats();
    assert_eq!(
        notifications, 0,
        "the bus received notifications; the test did not prove what it claims"
    );
    assert!(polls > 0, "the backstop poll never ran");
    h.stop().await;
}

/// Ported from `TestBurstLargerThanOneBatchIsFullyDelivered`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burst_larger_than_one_batch_is_fully_delivered() {
    let mut h = Harness::new("bus_burst").await;
    let b = h
        .run(Config {
            batch_limit: 10,
            ..quiet(Duration::from_millis(100), Duration::from_secs(5))
        })
        .await;
    let mut sub = b.subscribe(1024);
    // One call, so every row lands inside one overlap window.
    let mut events: Vec<Event> = (0..120)
        .map(|i| Event::new(format!("burst.{i}"), &h.cred, b"{}".to_vec()))
        .collect();
    let conn = h.conn().await;
    append_events(&conn, &mut events)
        .await
        .expect("append burst");
    drop(conn);
    let mut want: HashSet<i64> = events.iter().map(|e| e.id).collect();
    for e in collect(&mut sub, 120, Duration::from_secs(30)).await {
        want.remove(&e.id);
    }
    assert!(
        want.is_empty(),
        "{} of 120 events never arrived; the tailer stopped making progress",
        want.len()
    );
    // And it is still alive afterwards: the wedge's signature was silence.
    let after = h.append("after.the.burst", h.owner()).await;
    let tail = collect(&mut sub, 1, Duration::from_secs(20)).await;
    assert!(
        tail.len() == 1 && tail[0].id == after.id,
        "nothing was delivered after the burst; the tailer is wedged"
    );
    let (_, polls) = b.stats();
    assert!(polls > 0, "the tailer never returned to its poll loop");
    h.stop().await;
}

/// A transaction holding an unfinished event insert. Its id is assigned at
/// the INSERT, before the commit, exactly as it was in Postgres.
struct LateEvent {
    tx: hive_db::Transaction,
    id: i64,
}

async fn begin_late_event(h: &Harness) -> LateEvent {
    let tx = h.db().begin().await.unwrap();
    let id: i64 = query(
        "INSERT INTO events (kind, owner_kind, owner_id, author_actor, principal_kind, principal_id)
         VALUES ('late.event', 'user', ?1, ?1, 'user', ?1) RETURNING id",
    )
    .bind(h.alice)
    .fetch_scalar(&tx)
    .await
    .unwrap();
    LateEvent { tx, id }
}

impl LateEvent {
    async fn commit(self) -> i64 {
        self.tx.commit().await.unwrap();
        self.id
    }
}

/// Ported from `TestLateCommitWithALowerIDIsNotSkipped`, and changed by the
/// engine (D38). The hazard the overlap window exists for is a row that
/// takes a low id early and commits after a higher one is already visible.
/// Postgres allows that; this engine has one writer at a time, so a second
/// append cannot even START until the late transaction ends, and the low id
/// commits first by construction. The test now proves that ordering rather
/// than arranging the hazard it rules out: the fast append blocks behind the
/// late one, lands with the HIGHER id, and both are delivered exactly once.
/// The overlap window and the dedupe set stay, because a second writer
/// process (phase 2 of D38) brings the hazard back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lower_id_cannot_commit_late_because_writers_are_serialised() {
    let mut h = Harness::new("bus_late_commit").await;
    let b = h
        .run(quiet(Duration::from_millis(100), Duration::from_secs(5)))
        .await;
    let mut sub = b.subscribe(64);
    let late = begin_late_event(&h).await;
    let fast = {
        let store = h.store.clone();
        let cred = h.cred;
        let owner = h.owner();
        tokio::spawn(async move {
            let mut ev = Event::new("fast.event", &cred, b"{}".to_vec());
            ev.owner = owner;
            let conn = store.conn().await.unwrap();
            append_events(&conn, std::slice::from_mut(&mut ev))
                .await
                .expect("append");
            ev
        })
    };
    // The fast append is queued behind the open write transaction: nothing
    // reaches the subscriber while the late one is open.
    let got = collect(&mut sub, 1, Duration::from_millis(500)).await;
    assert!(
        got.is_empty(),
        "an event committed while another writer held the lock: {got:?}"
    );
    let late_id = late.commit().await;
    let fast = fast.await.unwrap();
    assert!(
        late_id < fast.id,
        "the append that waited took the lower id ({} < {late_id})",
        fast.id
    );
    let got = collect(&mut sub, 2, Duration::from_secs(10)).await;
    let ids: Vec<i64> = got.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![late_id, fast.id], "delivered {ids:?}");
    h.stop().await;
}

/// Ported from `TestNotificationDeliversFasterThanThePoll`. The bell is the
/// in-process one every append rings (D38); the process-wide counter is what
/// proves the delivery came from a ring and not from the 30s poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_delivers_faster_than_the_poll() {
    let mut h = Harness::new("bus_notification_fast").await;
    let b = h
        .run(Config {
            poll_interval: Duration::from_secs(30),
            overlap: Duration::from_secs(2),
            ..Config::default()
        })
        .await;
    let mut sub = b.subscribe(256);
    // The listener task starts asynchronously. Warm up until a ring has
    // actually been received.
    let warmup = tokio::time::Instant::now() + Duration::from_secs(20);
    while b.stats().0 == 0 {
        assert!(
            tokio::time::Instant::now() < warmup,
            "the listener never subscribed"
        );
        h.append("warmup", h.owner()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (before, _) = b.stats();
    let start = tokio::time::Instant::now();
    let want = h.append("journal.entry.created", h.owner()).await;
    let deadline = start + Duration::from_secs(5);
    let elapsed = loop {
        match tokio::time::timeout_at(deadline, sub.recv()).await {
            Ok(Some(batch)) => {
                if batch.iter().any(|e| e.id == want.id) {
                    break start.elapsed();
                }
            }
            Ok(None) => panic!("subscription closed"),
            Err(_) => panic!("the event never arrived within 5s of a 30s poll interval"),
        }
    };
    assert!(
        elapsed <= Duration::from_secs(2),
        "delivery took {elapsed:?} with a 30s poll interval"
    );
    assert!(
        b.stats().0 > before,
        "delivered without a new notification, so this test proves nothing"
    );
    h.stop().await;
}

/// Ported from `TestVisibilityIsPerSubscriber` (D4.9).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn visibility_is_per_subscriber() {
    let mut h = Harness::new("bus_visibility_per_subscriber").await;
    let bob = h.human("bob").await;
    let bob_cred = Credential::new(bob, PrincipalKind::User, bob);
    let b = h
        .run(Config {
            poll_interval: Duration::from_millis(200),
            overlap: Duration::from_secs(2),
            ..Config::default()
        })
        .await;
    let mut sub = b.subscribe(64);
    let mine = h.append("alice.private", h.owner()).await;
    let theirs = h.append("bob.private", Owner::user(bob)).await;
    let got = collect(&mut sub, 2, Duration::from_secs(10)).await;
    assert_eq!(got.len(), 2, "the hub is unfiltered by design");
    let g = h.store.guard();
    let conn = h.conn().await;
    let for_alice = g.visible(&conn, &h.cred, &got).await.unwrap();
    assert!(
        for_alice.len() == 1 && for_alice[0].id == mine.id,
        "alice saw {} events",
        for_alice.len()
    );
    let for_bob = g.visible(&conn, &bob_cred, &got).await.unwrap();
    assert!(
        for_bob.len() == 1 && for_bob[0].id == theirs.id,
        "bob saw {} events",
        for_bob.len()
    );
    drop(conn);
    h.stop().await;
}

/// Ported from `TestSlowSubscriberIsDroppedNotBlocking`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_subscriber_is_dropped_not_blocking() {
    let mut h = Harness::new("bus_slow_subscriber").await;
    let b = h
        .run(Config {
            poll_interval: Duration::from_millis(100),
            overlap: Duration::from_secs(2),
            ..Config::default()
        })
        .await;
    let mut slow = b.subscribe(1);
    let mut healthy = b.subscribe(64);
    for i in 0..8 {
        h.append(&format!("flood.{i}"), h.owner()).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    // Never read from `slow` until now: the buffered batch, then None.
    let dropped = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if slow.recv().await.is_none() {
                break;
            }
        }
    })
    .await;
    assert!(
        dropped.is_ok(),
        "a subscriber that never read was not dropped"
    );
    let got = collect(&mut healthy, 8, Duration::from_secs(10)).await;
    assert_eq!(
        got.len(),
        8,
        "the healthy subscriber fell behind while another was stuck"
    );
    h.stop().await;
}

/// Ported from `TestSettledWatermarkLagsTheNewestEvent`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settled_watermark_lags_the_newest_event() {
    let mut h = Harness::new("bus_settled_lags").await;
    let b = h
        .run(Config {
            poll_interval: Duration::from_millis(100),
            overlap: Duration::from_secs(3),
            ..Config::default()
        })
        .await;
    let mut sub = b.subscribe(16);
    let e = h.append("journal.entry.created", h.owner()).await;
    assert_eq!(collect(&mut sub, 1, Duration::from_secs(10)).await.len(), 1);
    let settled = b.settled().expect("no watermark after a delivery");
    assert!(
        settled < e.created_at.unwrap(),
        "watermark {settled} is not behind the event it just delivered ({:?})",
        e.created_at
    );
    h.stop().await;
}

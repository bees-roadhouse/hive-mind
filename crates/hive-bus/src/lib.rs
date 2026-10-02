//! The events table as live delivery.
//!
//! The invariant everything here is built around, and the one a future
//! contributor will violate:
//!
//! > The events table is the transport. NOTIFY is a wakeup bell carrying an
//! > id. Every consumer stays correct if every notification is dropped.
//!
//! The bell is now in-process (D38): `hive_store::event_wake()` rings once per
//! appended batch, and the store is one file per daemon so every writer is in
//! this process. The tailer still never trusts it. It re-reads an overlap
//! window on every cycle, dedupes by id, and polls unconditionally on a timer
//! whether or not anything rang. A missed ring is a latency event, never a
//! correctness event ... and the tests prove that by running the whole suite
//! with the bell disconnected.

mod hub;
mod sse;
mod tailer;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use hive_db::Db;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub use hub::Subscription;
pub use sse::SseOptions;

/// Tunes the tailer. Every default is a latency or safety knob, not a
/// performance knob.
#[derive(Clone, Debug)]
pub struct Config {
    /// How far back every poll re-reads. THE load-bearing number: bigserial
    /// ids are assigned BEFORE commit, so a row assigned early and committed
    /// late becomes visible after rows with higher ids. Overlap must exceed the
    /// longest transaction that writes events.
    pub overlap: Duration,
    /// The unconditional backstop, run regardless of connection health.
    pub poll_interval: Duration,
    /// Bounds one tail query. A full batch means "there is more".
    pub batch_limit: i64,
    /// Whether to listen for the in-process bell at all. Turning it off is how
    /// the test suite proves the invariant: a bus nobody wakes must still be
    /// correct on the backstop poll alone.
    pub listen: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            overlap: Duration::ZERO,
            poll_interval: Duration::ZERO,
            batch_limit: 0,
            listen: true,
        }
    }
}

impl Config {
    pub fn defaults(mut self) -> Config {
        if self.overlap.is_zero() {
            self.overlap = Duration::from_secs(5);
        }
        if self.poll_interval.is_zero() {
            self.poll_interval = Duration::from_secs(5);
        }
        if self.batch_limit <= 0 {
            self.batch_limit = 500;
        }
        self
    }
}

pub(crate) struct Inner {
    pub(crate) db: Db,
    pub(crate) cfg: Config,
    pub(crate) hub: hub::Hub,
    /// "Something may have happened." A notification is a hint, and a
    /// redundant one costs nothing to drop.
    pub(crate) wake: Notify,
    /// The newest cursor position that can no longer gain rows behind it, in
    /// unix micros. Subscribers checkpoint here rather than at the newest event
    /// they saw.
    pub(crate) settled_micros: AtomicI64,
    pub(crate) notified: AtomicI64,
    pub(crate) polls: AtomicI64,
    ready: AtomicBool,
    ready_notify: Notify,
}

impl Inner {
    /// A checkout from the store's pool, with the store's error shape so the
    /// tailer and the SSE handler report one kind of failure.
    pub(crate) async fn conn(&self) -> Result<hive_db::Conn, hive_store::StoreError> {
        self.db
            .conn()
            .await
            .map_err(|e| hive_store::StoreError::db("connect", e))
    }
}

/// Owns one tailer per host and fans out in memory to every subscriber on
/// that host (D4.8). Cheap to clone; every clone is the same bus.
#[derive(Clone)]
pub struct Bus {
    pub(crate) inner: Arc<Inner>,
}

impl Bus {
    /// Builds a bus over an open store file. Nothing runs until `run` is called.
    pub fn new(db: Db, cfg: Config) -> Bus {
        Bus {
            inner: Arc::new(Inner {
                db,
                cfg: cfg.defaults(),
                hub: hub::Hub::new(),
                wake: Notify::new(),
                settled_micros: AtomicI64::new(0),
                notified: AtomicI64::new(0),
                polls: AtomicI64::new(0),
                ready: AtomicBool::new(false),
                ready_notify: Notify::new(),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.cfg
    }

    /// The watermark a subscriber may safely resume from: every event at or
    /// before it has been read, and no transaction can still commit one behind
    /// it. `None` until the first tail cycle completes.
    pub fn settled(&self) -> Option<DateTime<Utc>> {
        let micros = self.inner.settled_micros.load(Ordering::SeqCst);
        if micros == 0 {
            return None;
        }
        Utc.timestamp_micros(micros).single()
    }

    /// How the tailer has been woken: (rings, polls). A healthy system polls
    /// occasionally and is rung often; a system with the bell disconnected
    /// polls only, stays correct, and gets slower.
    pub fn stats(&self) -> (i64, i64) {
        (
            self.inner.notified.load(Ordering::SeqCst),
            self.inner.polls.load(Ordering::SeqCst),
        )
    }

    /// Whether the first tail cycle has run.
    pub fn is_ready(&self) -> bool {
        self.inner.ready.load(Ordering::SeqCst)
    }

    /// Resolves once the first tail cycle has run, so callers do not race the
    /// initial watermark.
    pub async fn ready(&self) {
        loop {
            let notified = self.inner.ready_notify.notified();
            if self.is_ready() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn mark_ready(&self) {
        if !self.inner.ready.swap(true, Ordering::SeqCst) {
            self.inner.ready_notify.notify_waiters();
        }
    }

    /// Rings the wakeup bell.
    pub fn kick(&self) {
        self.inner.wake.notify_one();
    }

    /// Joins the in-memory fan-out. Drop the subscription when done.
    pub fn subscribe(&self, buffer: usize) -> Subscription {
        self.inner.hub.subscribe(buffer)
    }

    /// Drives the listener and the tail loop until `cancel` fires.
    pub async fn run(&self, cancel: CancellationToken) {
        let listener = {
            let bus = self.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { bus.listen(cancel).await })
        };
        self.tail_loop(cancel).await;
        listener.abort();
        let _ = listener.await;
        self.inner.hub.close_all();
    }

    /// Forwards the in-process bell to the tail loop. There is no connection
    /// to lose any more; what remains of the old listener is the counter and
    /// the rule that the tail loop, never this task, does the reading.
    async fn listen(&self, cancel: CancellationToken) {
        if !self.inner.cfg.listen {
            cancel.cancelled().await;
            return;
        }
        let bell = hive_store::event_wake();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = bell.wait() => {
                    self.inner.notified.fetch_add(1, Ordering::SeqCst);
                    self.kick();
                }
            }
        }
    }
}

# Tailing the events table

Read this before writing anything that consumes `events`. The cursor has a
shape, the shape has reasons, and getting it wrong produces a bug that only
shows up under load or when a second writer arrives.

## The rules that do not change

From D4 (Hallie's findings), unchanged by the engine (D38):

1. **The events table is the transport. NOTIFY is a wakeup bell carrying an id.**
   Every consumer must stay correct if every notification is dropped.
2. **Never tail with a naive `WHERE id > last`.** Ids are assigned before
   commit, so an id assigned early and committed late is permanently skipped.
   Use an overlap window and dedupe by id.
3. **Backstop poll every 5 to 30 seconds** regardless of whether anything rang.
   That is what turns a missed ring into a latency event rather than a
   correctness event.

## What the engine changed

The store is one SQLite file per daemon (D38). Three consequences, and one
thing that deliberately did not change.

### The bell is in-process

`hive_store::event_wake()` is a process-wide `Notify`; `append_events` rings
it once per call after the commit, and the bus's listener task forwards each
ring to the tail loop. There is no connection to lose and nothing to
reconnect, so the old rule about never listening on a pooled connection has
nothing to apply to.

What it means for a writer **outside** the daemon's process: it cannot ring
the bell at all. Its rows arrive by the backstop poll, within one poll
interval. The e2e suite writes events that way on purpose, because that is
rule 1 stated the hard way.

### One writer at a time

Every write transaction is `BEGIN IMMEDIATE`, and the engine admits one. A
second writer waits for the first to finish before its `INSERT` runs, so its
id is assigned after the first one commits. **A lower id cannot commit late
today.** `crates/hive-bus/tests/bus.rs` proves the ordering rather than
arranging the hazard it rules out.

The overlap window and the dedupe set stay anyway. Phase 2 of D38 gives each
owner a file and the daemon `ATTACH`es them, and a second process on a file
is a second writer; the sweep costs one indexed read per cycle and the day it
earns its keep is not a day anyone will be reading this.

### The cursor is still `(created_at, id)`

Not because the table is partitioned (it is not, any more) but because the
two halves answer different questions: `created_at` bounds the sweep in time,
`id` breaks ties and is the only thing guaranteed unique. Both are indexed
together (`events_cursor_idx`). Carry both halves:

```sql
SELECT id, created_at, kind, body
  FROM events
 WHERE created_at >= ?1 - 5000000        -- overlap window, in microseconds
   AND (created_at > ?1 OR (created_at = ?1 AND id > ?2))
 ORDER BY created_at, id
 LIMIT 500;
```

Timestamps are integer microseconds since the epoch, assigned by the engine's
clock at the `INSERT` (`created_at` defaults to it), and the tailer reads the
same clock once per cycle through `hive_store::now` so that "five seconds ago"
is five seconds of the store's time and never the host's.

Dedupe by `id` after the overlap re-read. Handlers must be idempotent anyway.

### `Last-Event-ID` has to carry both

SSE reconnect (D4.13) sends back whatever was in the last `id:` field. Emit the
composite, not the bare id:

```
id: 1736899200123456-4711
```

that is `<created_at as microseconds since epoch>-<id>`. A client never parses
it; it hands it back verbatim and the host splits it. A host that receives a
bare integer (an old client, or a hand-written curl) resolves the timestamp
with one lookup on connect (`hive_store::resolve_cursor`), never per poll.

**Replay filters with CURRENT permissions**, never permissions as of the event
(D4.13). Use the predicate like any other read; a revoked grant must not be
replayed around.

## Retention

The table is append-only and there are no partitions to drop. Retention is
"keep" today; the day it is not, the pruning is a `DELETE ... WHERE created_at
< ?` under the same write lock as everything else, and the cursor's time half
is what makes that cheap.

## `(origin, origin_id)` uniqueness lives in a side table

D4.12 asks for a unique constraint on `(origin, origin_id)` from migration one,
so cross-hive bridging can dedupe later. `event_origins (origin, origin_id)
PRIMARY KEY` carries it, written by an `AFTER INSERT` trigger on `events` only
for rows where `origin_id IS NOT NULL`. Locally produced events leave it NULL
and cost nothing; a duplicate bridged event raises a unique violation on
insert, which is the behaviour D4.12 wanted. The side table outlived the
partitions that first required it because it also records which event a
bridged id landed as, which a bare index would not.

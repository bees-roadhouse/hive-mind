# D38: the store is SQLite, one file per daemon now and one per owner next; the client stays htmx

**Decided** 2026-10-02 by Nate: "forget postgres ... the sqlite stuff i want
to do with libsql", against a design note he brought that day: Turso/libSQL
as the engine ("millions of databases, one per tenant"), native replication
for read fan-out, per-database encryption, embedded replicas, cross-tenant
`ATTACH`; vectors stored beside the tenant's other rows; offline client sync
(LiteSync or SQLite Sync) switched on per tenant where sync is permitted; a
thin tenant router in front, so application code stays single-tenant and
isolation is an infrastructure concern. Shape and phasing by Propolis the
same day, after a probe build on the Windows desktop. Supersedes D24's
Postgres row and the Postgres half of its "no native library" rule; D32's
client decision stands.

## What was asked, and what it means for this code

The note describes four things that are separable, and separating them is
the decision:

1. **An engine change**: Postgres and sqlx out, SQLite through the `libsql`
   crate in. This is the whole of the first change and it touches every
   crate that holds a pool.
2. **A tenancy layout**: one database file per owner principal, reached
   through a router that turns a credential into a file. Nothing in the
   design above changes for it; it is a change to where rows live.
3. **Replication**: embedded replicas that sync from a primary, so reads
   fan out and the primary takes the writes.
4. **Offline clients**: a client holding its own file and syncing when
   allowed.

The order below is the order these become true. Each is a decision on its
own record so the one after it can be reversed without unpicking the one
before.

## The decision

### 1. The engine is SQLite; the libSQL fork was the first pick and was measured out

The decision as made named the `libsql` crate, Turso's C fork of SQLite,
and the port was built against it. It is recorded below as it was decided,
and then what the measurement found, because the reasons are what outlive
the pick.

**What the port found (2026-10-02, the same day).** With the whole store
ported and its suites green test by test, the test process died
intermittently with `STATUS_ACCESS_VIOLATION`. Reduced to a probe with no
hive code in it: open N connections on one file, run one `SELECT` each,
close them. With N up to about ten it holds; at N=16 it dies on the first
round, at N=64 every time, in `sqlite3_close_v2` reading a database handle
that was already freed (gdb on a GNU build of the same probe, with the
Windows debug heap off). Rollback-journal mode crashes the same way; the
threshold is per process, not per file (64 files, one connection each,
crashes). `libsql` 0.9.30 and 0.10.0-pre.4, MSVC and GNU builds, both.
The identical pattern against vanilla SQLite (`rusqlite`, bundled 3.50)
ran 600 connections at a time, twenty rounds, clean.

A daemon with a heap-corruption bug ten connections away is not a daemon,
so **the engine is vanilla SQLite through `rusqlite`**, bundled. Everything
below the wrapper changed; nothing above it did, which is what `hive-db`
is for. What that costs and where it is paid:

- **Vectors are not in the engine.** `F32_BLOB` and `vector_top_k` were
  libSQL additions. sqlite-vec comes back as the candidate (the note's
  original pick), registered per connection; its registration is one
  `unsafe` call to `sqlite3_auto_extension`, in `hive-db` and nowhere
  else, when vector indexes are built. `IndexMethod::Vector` refuses
  today for the manifest reason as well, so nothing is lost yet.
- **Replicas are not in the crate.** Phase 3 chooses between the fork
  (if fixed; the probe is the acceptance test), Litestream-style WAL
  shipping, or `sqld`. Nothing in phases 1 and 2 depends on the choice.
- **The pool is load-bearing either way.** `hive-db` keeps a bounded
  checkout pool because opening a connection per statement is wasteful on
  any engine; it was written first as a mitigation for the fork's bug and
  kept for the reason it should have been written anyway.
- **Every call blocks the calling thread.** It did under libSQL too: the
  fork's async API is synchronous underneath for a local file. Nothing
  changed; it is now visible in the type.

**The decision as made**, kept for the reasons:

Not `rusqlite` with the sqlite-vec extension, and not Turso's pure-Rust
engine (the `turso` crate, the rewrite the vendor now recommends for new
projects). The criteria, which outlive the pick:

- **Triggers.** The policy that makes "an AI cannot climb" hold for every
  writer that reaches the database is written as triggers (grants,
  credentials, actors, org membership, install authorities, events
  append-only). The Rust rewrite of SQLite did not have `CREATE TRIGGER`
  on the day this was decided, and a database that cannot refuse a write
  by itself moves every one of those rules into the writers, which is
  the exact shape invariant 11 warns about. libSQL is SQLite with
  additions, so every trigger expressible in SQLite is expressible here.
- **Vectors in the engine.** libSQL has `F32_BLOB(n)` columns,
  `vector32()`, `vector_distance_cos()`, a `libsql_vector_idx` index and
  `vector_top_k()`, in the file, replicated with it, with nothing to load
  and nothing to register. sqlite-vec would need `load_extension` on
  every connection (or an `unsafe` auto-extension hook, which the
  workspace forbids) and would not travel with a replica. sqlite-vec
  stays the right answer for a browser or WASM client if one ever holds a
  file of its own; the server does not need it.
- **Replicas and encryption are the same crate**, behind cargo features
  (`replication`, `sync`, `encryption`) the daemon can turn on when phase
  3 arrives, without a second driver.
- **It builds where the gate runs.** Probed on the Windows desktop with
  MSVC: vectors, `RETURNING`, JSON and JSONB, `ATTACH`, triggers with
  `RAISE`, WAL with two connections. (Two connections. The probe that
  found the crash opened sixteen.)

What it costs, accepted: `libsql-ffi` compiles the C amalgamation into the
binary. D24's rule was "a host with no `unsafe` cannot smuggle a native
library in"; the rule on *this repository's code* stands unchanged
(`unsafe_code = "forbid"` at the workspace), and the database engine is
the one native library the host is now built around rather than one
smuggled past the rule. The sqlx pick, and the TLS reasoning behind it,
go with Postgres.

### 2. Policy stays in the database; the predicate text moves into the store

Everything migration one enforced with plpgsql triggers is enforced with
SQLite triggers in the new migration one: the same rules, the same order,
static messages where the old ones interpolated an id. A trigger that
refuses a write refuses it for a hand-written `sqlite3` session as much as
for the daemon, which is the property that made them triggers (D21,
invariant 11).

What SQLite cannot hold is a stored function. `access_decision`,
`access_reason`, `tool_access_reason`, `route_access_reason`, `unshare`,
`visible_events` and `visible_event_ids` were functions the migration
installed and `Guard` called. They are now **one SQL text owned by
`hive-store`**, composed into the point check and into every set read from
the same constant, with `Guard` still the only caller and the grants table
still unreachable from any other crate. Invariant 1 is unchanged: one
enforcement point in the host data layer. What changed is where the text
is pinned ... by the binary rather than by the schema's checksum ... so the
store's test suite is now the thing that holds it, and
`tests/invariants.rs` runs against the real predicate on every build.

`subject_owner` becomes a view, `subject_owners`, so a new grantable kind is
still one `UNION` arm (D3's property, kept). `acting_kind` is an expression
that appears twice: in the predicate text and inlined in the two triggers
that need it. Two copies, written down here as the price.

### 3. The types and the dialect

Recorded so nobody rediscovers them:

| Postgres | here | why |
|---|---|---|
| `uuid` | `TEXT`, hyphenated lowercase; a `DEFAULT` expression mints a v4 for raw inserts | readable in `sqlite3`, indexable, and what the identity crate already prints |
| `timestamptz` | `INTEGER` microseconds since the Unix epoch, UTC | the bus checkpoints in microseconds; integers compare and index without a parse; `DEFAULT` uses `julianday('now')` at millisecond resolution and every host write binds a microsecond clock |
| `jsonb` | `TEXT CHECK (json_valid(...))` | one representation; the JSON functions accept it; a document round-trips as the bytes it arrived as |
| `boolean` | `INTEGER` 0/1 with a `CHECK` | |
| `bigint ... IDENTITY` | `INTEGER PRIMARY KEY AUTOINCREMENT` | |
| `~ '<regex>'` | `GLOB` with a negated class and a `length()` | no regex in the engine; the three alphabets (hex, slug, event kind) are expressible exactly |
| `IS [NOT] DISTINCT FROM` | `IS [NOT]` | same null-safe semantics |
| `NULLS NOT DISTINCT` unique index | expression index over `coalesce(col, '')` | |
| `$1` placeholders | `?1` | numbered, so the binding order is the number and not the first appearance |
| `FOR UPDATE SKIP LOCKED` | `UPDATE ... WHERE id = (SELECT ... LIMIT 1) RETURNING` under `BEGIN IMMEDIATE` | one writer at a time per file, so the claim is serialised by the engine |
| `LISTEN` / `NOTIFY` | an in-process wake after commit | the events table stays the transport (invariant 4); a replica will poll, which the tailer already assumes |
| `pg_advisory_lock` | `BEGIN IMMEDIATE` | the write lock is the mutex across processes |
| monthly partitions | gone; `(created_at, id)` index | the cursor stays a pair, because a replica's clock is not the primary's |
| `DEFERRABLE INITIALLY DEFERRED` | write order: the reference before the flip to live | SQLite triggers are immediate |
| the override audit on a second connection | the override audit in a second FILE, `hive-audit.db` | one writer per file: a second connection on the same file waits on the caller's write lock, which is a deadlock when the caller waits on the audit; D18.2 wants the evidence to survive the caller's rollback, and a file of its own is what makes it independent |
| `CREATE SCHEMA` per install | a table-name prefix, `<schema_name>__<collection>`, in the one file | phase 2 moves these to the owner's file |
| plpgsql `set_updated_at` per app | a `BEFORE UPDATE` trigger per collection table | |

Ids are monotonic with commit order now, because there is one writer. The
tailer keeps its overlap window and its dedupe anyway, because a replica
reading its own file behind the primary has exactly the late-visibility
problem the overlap exists for.

### 4. Migrations restart at 0001

No hive-mind database has ever been deployed: brh-infra's `hive` stack
runs the previous project's images against its own Postgres, and hive-mind
is Phase 0. There is nothing forward-only to carry, so the five Postgres
files are replaced by one SQLite file and the history starts again. The
machinery stays: embedded files, `schema_migrations` with a SHA-256 of the
bytes, a file applied differently refused rather than reapplied.

### 5. Phases, and what each one is a decision about

- **Phase 1, this change: one file per daemon.** `--data-dir` holds
  `hive.db` (and its WAL). Every table of migration one, every app's
  collection tables, the blob catalog and the event log live in it.
  `HIVE_SANDBOX_DATABASE_URL` and the Postgres scripts, service container,
  compose entry and `db-up` go. The whole data tier is now in-process.
- **Phase 2: one file per owner principal.** The control plane (actors,
  memberships, credentials, grants, install authorities, installs, builds,
  the blob catalog, runs, chat, workflow state) stays in `hive.db`; what
  an owner's apps store (collection tables, entities, links, mentions,
  the owner's slice of events) moves to `owners/<kind>-<id>.db`, opened
  by a resolver that takes the credential and nothing else (invariant 14:
  the file is keyed on the owner, never on the install or the app). A
  cross-owner read under a grant `ATTACH`es the subject's file for that
  statement; the predicate is unchanged and is still answered from the
  control plane before any attach happens. Open before it lands: the
  default attach limit (10; `SQLITE_MAX_ATTACHED` allows 125), whether
  events stay central, and what "delete a tenant" means for blobs. This is
  what makes "a user adds a database" a file rather than a migration.
- **Phase 3: replicas.** `--replica-of <url>` opens `hive.db` through
  `Builder::new_remote_replica` with a sync interval; the role flags D7
  already has decide who writes. Read-only roles serve from the replica;
  the primary keeps the writers and the bus. Encryption at rest is the
  same crate's `encryption` feature, keyed from the daemon's boot key.
- **Phase 4: offline clients.** Only when a native client exists to hold
  a file. See the next section.

### 6. The client stays server-rendered htmx (D32 stands)

The note's diagram puts a local SQLite with sqlite-vec and a sync layer
inside the client, which prompted the question of whether the browser
client should become "a regular app" again. No, and the reason is in the
diagram: a browser tab cannot be a LiteSync node or hold a database file
that outlives it. The client in that picture is a native one ... desktop
or mobile ... and D31 and D32 already say a desktop client later starts
from rendered pages rather than from a shell of its own. Nothing in this
decision is visible to the browser: the engine, the file layout and the
replicas all live behind the daemon's HTTP surface, and htmx keeps the
property D32 bought ... an installed app contributes a fragment and
nothing is rebuilt.

Offline is therefore still "a later decision, not a lost one" (D32 §1),
and it is phase 4 here: made when there is a client to make it for, with
sqlite-vec and a sync layer as the candidates the note names, and with
per-tenant gating as the shape ... LiteSync if "allowed" means an operator
decides which nodes connect, SQLite Sync's row policies if it means rows.
Until then the only replica is the daemon's own.

### 7. What the engine change makes true operationally

- **Every database test runs everywhere.** `hive_testdb::TestDb` hands each
  test a private file in a temp directory and deletes it on the way out.
  No Podman, no service container, no environment variable: the "database
  line is not optional" rule and the `SKIPPED:` lines it guarded are gone
  because the precondition they guarded is gone. The named-skip rule
  stays for the tiers that still have one (containers, Garage, chromium).
- CI loses the Postgres service; the stack compose loses its database
  container and its init dump; the daemon image needs a writable
  `--data-dir` and nothing else.
- `docs/development.md`, `CLAUDE.md`, `README.md`, `docs/events-tailing.md`
  and the schema's own comments are rewritten with this change rather
  than after it, because `hive-repodocs` pins the phrases that moved.

## What lost

- *Keep Postgres.* Nate's call, and the note's goals (a file per tenant,
  user-added databases without a migration, embedded replicas, vectors in
  the tenant's own store) are not things Postgres gives.
- *The `turso` crate.* No triggers on the day of the decision; `WITH
  RECURSIVE` partial. Revisit when triggers land; the `libsql` API it
  replaces is the one the daemon now wraps, so the seam is small.
- *sqlx's SQLite driver.* Vanilla SQLite: no engine vectors, no replicas,
  a second native library when the point was one.
- *A dual-backend store.* Two enforcement points answering one question
  (invariant 1's shape). The port is a replacement.
- *dqlite and rqlite for self-hosting.* Both replicate vanilla SQLite over
  Raft; neither runs libSQL's vectors or its replica protocol. The
  self-hosted replica story is libSQL's own server (`sqld`) and is a
  phase 3 question.
- *sqlite-vec on the server.* Above, and back on the table with the engine
  change in §1.

## Open

- Phase 2's file layout and the central-events question.
- (Closed the same day.) Whether `hive-db` keeps a connection pool: it
  does, bounded, checkout semantics, a connection mid-transaction closed
  rather than returned. See §1 for what measured it.
- The vector index method for app collections (`IndexMethod::Vector`
  still refuses, now for a different reason: the manifest has no way to
  declare a dimension, and `F32_BLOB` needs one).

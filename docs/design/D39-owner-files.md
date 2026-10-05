# D39: an owner's documents live in the owner's file; the control plane, the index rows and the events stay central

**Superseded by D43** (2026-10-04): the tenant is an organization's Postgres database; the owner files go from the SQLite backend in D43's phase 5.

**Status:** decided 2026-10-02, the same day D38 landed. This is D38's phase
2, made as its own record because the measurements below changed what phase
2 could be.

## What was asked

D38 §5: "one file per owner principal. The control plane stays in `hive.db`;
what an owner's apps store (collection tables, entities, links, mentions, the
owner's slice of events) moves to `owners/<kind>-<id>.db`, opened by a
resolver that takes the credential and nothing else." Three things were left
open there: the attach limit, whether events stay central, and what deleting
a tenant means.

## What the probe measured

Against the bundled engine (`rusqlite` 0.37, SQLite 3.50), before any of
this was built. Each line is a fact the design below rests on.

1. A file can be `ATTACH`ed **inside** `BEGIN IMMEDIATE`, DDL and writes to
   it run in that transaction, and `ROLLBACK` undoes them in both files.
   `DETACH` of a file the open transaction has touched is refused
   ("database is locked"); one it never touched detaches.
2. `BEGIN IMMEDIATE` on a connection with a file attached takes the write
   lock on the attached file too, before anything is written to it: a second
   connection writing that file alone waits. The control plane's lock and
   the owner file's lock are one lock for the duration.
3. A trigger cannot write to a table in another attached database
   ("qualified table names are not allowed on INSERT, UPDATE, and DELETE
   statements within triggers"), TEMP triggers included.
4. `PRAGMA journal_mode = WAL` cannot be set on an attached file inside a
   transaction, and attaching a fresh path leaves it in rollback-journal
   mode. The file has to be put into WAL by a connection of its own, once.
5. An unqualified table name resolves to `main` first; a cross-file join
   works when both sides are qualified. A prepared statement survives a
   `DETACH` and a re-`ATTACH` of the same alias to a different file (the
   engine re-prepares on schema change).
6. The attach limit is 10 per connection (`SQLITE_LIMIT_ATTACHED`), and the
   same alias cannot be attached twice ("database o1 is already in use").
7. From the engine's documentation, not measured here because a crash is
   not a thing a test arranges: in WAL mode a `COMMIT` spanning attached
   files is atomic per file, not across them. A crash in the middle of the
   commit can land one file's half and not the other's.

## The decision

### 1. Only the documents move

An owner's file holds the collection tables an install provisions
(`<schema>__<collection>`, the same names as phase 1) and nothing else. The
`entities`, `links` and `mentions` rows, the blob catalogue, the grants and
the events stay in `hive.db`.

Measurement 3 decides it. The grants trigger reads `subject_owners`, which
is a view over `entities`; `mentions` references `grants` and `actors`;
`links` references `entities`. A trigger in the control plane cannot see a
table in an owner's file, so moving `entities` out would move the policy
that reads it into Rust writers, which is the exact shape invariant 11
warns about. The `entities` row is the ownership index the predicate
resolves through, and the predicate stays answerable from the control plane
alone, before any attach happens, which is what D38 §5 asked for.

Events stay central, closing D38's open question: the bus tails one table,
the in-process bell rings for one table, and an owner's slice of the log is
a `WHERE`, not a file. A replica per owner (phase 3) can carry its owner's
events when there is a replica to carry them.

### 2. The file is keyed on the owner, and says so

The directory is `<control plane file stem>-owners/` beside `hive.db`
(`data/hive-owners/`; a test's `t_x_1234-owners/`), because an owner file
belongs to exactly one control plane and deriving the directory from that
file's path is what keeps the two from drifting apart. The file is
`<kind>-<uuid>.db`. The alias it is attached under is
`o_<kind>_<uuid without hyphens>`, deterministic, so attaching the same
owner twice on one connection is one attachment (measurement 6).

Every owner file carries one row, `owner_file (kind, id, created_at)`,
written when the file is created and checked every time it is attached. A
file copied or renamed under another owner's name is refused at the attach,
not discovered as somebody else's documents (invariant 14: the name is the
key, and the file carries the dimension the name could lose).

The `-owners` suffix rather than D38's `owners/` is the one departure from
§5, for the reason above.

### 3. Attach inside the transaction, initialise outside it

`attach_owner(conn, owner)` is the resolver. It takes a connection and an
owner, never an install or an app (invariant 14, as D38 §5 said), and:

- initialises the file once per process if it has not been: open it on a
  connection of its own, switch it to WAL, run the owner file's migrations
  (`migrations-owner/`), write the marker (measurement 4);
- `ATTACH`es it on the caller's connection, inside the caller's transaction
  when there is one (measurement 1), and checks the marker through the
  alias;
- returns the alias, which is the only thing a caller addresses tables
  through: every collection table name is `"<alias>"."<schema>__<coll>"`
  (measurement 5).

A pooled connection detaches everything on its way back to the pool and is
closed instead if it cannot (measurement 1 again: a connection mid-
transaction is already closed rather than returned, and one that would
return with an attachment is a connection keyed on something the next
caller did not ask for).

The owner a caller attaches is the **target install's** owner, the
principal whose documents they are. A grantee reading another person's
entry attaches that person's file, after the predicate said yes on the
control plane; a guest's own writes attach its own owner's. Nothing attaches
on the strength of the request's claim: the owner comes off the `installs`
row the target resolved to.

### 4. What this does and does not buy

Bought: a tenant is a file. Copy it, replicate it, encrypt it, delete it,
size it. "A user adds a database" is a file appearing in `-owners/` (what
an install provisions goes there), not a migration. Uninstall is still one
`DROP TABLE` per collection in the owner's file.

Not bought, and said plainly: **write concurrency.** Measurement 2 means a
write transaction that attaches an owner file holds that file's lock and
the control plane's together, and every write transaction takes the
control plane's lock, so one writer at a time is still one writer at a time
across the daemon. Phase 2 is a layout, not a throughput change.

Accepted, and recorded because it is new: measurement 7. A commit that
writes the owner file and the control plane together can, on a crash at
exactly the wrong moment, land in one file and not the other. The two
shapes that leaves are an `entities` row with no document (a read says not
found; the next write of that ref is a new row) and a document with no
`entities` row (unreachable, and dropped with its table on uninstall).
Neither is a leak and neither is silent; a sweep that reports them is phase
3 work, when replicas make torn files a routine rather than a crash.

### 5. The attach limit is not reached

One statement touches at most two owner files, the caller's and the
target's, on a connection whose attachments are released at return. The
limit of 10 (measurement 6) would matter to a query that joined many
owners' documents, and no such query exists: a cross-owner read is a read
of one subject under one grant.

## What lost

- *A `Db` pool per owner file, no `ATTACH`.* Simpler, and it loses the one
  transaction that writes the document and its `entities` row together.
  Measurement 7 is the price of atomicity across files under WAL; losing it
  on every write to save it on a crash is the wrong trade.
- *`entities` in the owner file.* Measurement 3.
- *Events in the owner file.* One table to tail; D38 §5's open question,
  closed above.
- *`owners/` as D38 wrote it.* `<stem>-owners/` instead, so the directory
  cannot belong to a different control plane than the one beside it.

## Open

- Deleting a tenant: the file goes, and the control plane's rows for that
  owner (installs, entities, blob refs, grants, events) are the real
  question. Not decided here; nothing deletes a tenant today.
- A sweep for the two torn shapes in §4, for phase 3.
- Replicas per owner file (phase 3) and encryption at rest per file, which
  this layout makes possible and does not do.

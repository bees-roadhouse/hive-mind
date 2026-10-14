# D60: merging a branch is replaying its operation log onto `main` through the same `Guard`: every write through the storage interface records an operation; a conflict is the same row and field written on both sides since the fork watermark, decided per field by D45's rule unless the manifest declares keep-both or ask-a-person; schema first, then data; idempotent, resumable, with a dry run; a write `main` refuses is a conflict, never a bypass

**Decided** 2026-10-14 by Nate, relayed by Pia from Forage: "i like the
first choice in #3. we need that." The choice was Forage's option for
getting branch data back to `main`: replay the branch's writes onto `main`
through the same enforcement point, with a conflict rule declared per
collection, UUID keys meaning rows created on both sides never collide.
Passed over at the same time: logical replication of selected tables.
Shape by Propolis; Forage's five questions and Pia's watermark point are
answered in the body. Builds on D45 (the operation log and the per-field
HLC conflict rule devices already use), D47 (bus items with `held` and
`acted`), D52 (`org_migrations`, `schema.apply`), D54 (the branch
capability), D59 (the backings and the fork watermark), invariants 1, 11
and 14. **Amends D54 §2 and D55 §2 and §*What lost***, both of which said
a branch is never promoted. They said it because the engine cannot merge
data; this record merges at the platform, where the predicate is, which
is where a merge belongs.

## What was checked before deciding

- **Today's storage write emits a pointer, not the write.** The write
  path in `hive-store` (`appdata.rs`, `emit`) appends one event per
  insert, update or delete, kind `<install>.<collection>.<verb>`, body
  `{"id", "collection"}`, with the credential, subject, owner and trust.
  Replaying those events could rebuild nothing. The events table is the
  transport and the bell (invariant 4) and stays so; it is not a write
  log.
- **D45 already defines the write log**, for devices: "what syncs up is
  an operation log, not rows", each operation with an idempotency key and
  a hybrid logical clock, replayed through the normal write API, decided
  by the same predicate and the same writer that pins `author_actor`
  from the credential (invariant 11), refusals visible and never silent.
  Nothing of it is built yet; D45's order puts it at step 3.
- **D45 §3 already defines the conflict rule**: last writer wins per
  field on the HLC; long text keeps both versions with the later one
  current; list order by fractional index; delete against edit, the later
  HLC wins. Not CRDTs, for the reasons D45 gives.
- **Row keys are UUIDs** (`uuid` on both engines, `hive-db`), so two
  sides creating rows never collide; a conflict can only be two writes to
  one existing row.

## The decision

### 1. Every write through the storage interface records an operation

- **An operation** is written in the same transaction as the write it
  describes, on every engine, server-side and on devices alike: `op_id`
  (UUID, the idempotency key), the HLC, `author_actor` and the owner
  principal from the credential (invariant 2), the install, the primitive
  (`table`, `document`, `blob`), the target (schema, table or
  collection, row id), the verb, and a **per-field delta** (the fields
  written and their new values; for a document, the paths; for a blob,
  the reference and the catalogue change). Trust travels with it
  (invariant 12).
- **The log is append-only and per org database**, beside the events
  table, retained for at least the longest live branch plus the restore
  window, and compacted beyond that. It is the one log: D45's device sync
  replays into it and reads from it; a branch is a replica with a log,
  exactly as a device is.
- **No write escapes it.** An app has no path but the interface (D53 §2);
  a host-internal writer (the registry, provisioning, the bus's own
  rows) writes platform rows that are not merged and are marked so; a
  writer that touches a merged primitive without an operation is a
  defect the suite asserts against (scenario 1).

### 2. The fork watermark and what counts as a conflict

- **The fork watermark** is recorded on the branch row at creation
  (D59 §4): the last operation id visible on the parent at the fork. For
  a `clone` branch, the last id at the pool drain. For a `snapshot`
  branch, the highest id in the snapshot's own log, which is correct by
  construction because the snapshot carries the log; nothing the daemon
  remembers can be more accurate than the copy itself.
- **The branch's merge set** is every operation in the branch's log after
  the watermark. **`main`'s divergence set** is every operation in
  `main`'s log after the same watermark. Both sides allocate ids from
  their own sequences after the fork, so an operation is identified by
  its `op_id`, never by position.
- **A conflict** is a row present in both sets with at least one field
  written on both sides. Decided per field: the **default** is D45 §3
  (HLC last-writer-wins; long text keeps both with the branch's version
  as a conflict revision when it loses; delete against edit by HLC). A
  manifest may declare, per collection or table, `merge: lww` (the
  default, named), `merge: keep-both` (every field, not only long text:
  the losing version is kept as a revision on the row), or `merge:
  ask-a-person` (§3). Fields written on one side only are never a
  conflict. Rows created on the branch are inserted; rows created on
  `main` are untouched.
- **A refusal is a conflict.** Every replayed operation is decided on
  `main` by the predicate from the operation's own actor and principal
  identifiers (invariant 11), as if that actor made the write now. An
  operation allowed on the branch and refused on `main` (a grant revoked
  since the fork, a row moved to another owner) becomes a conflict of
  kind `refused`, surfaced as §3 does, never applied under the merging
  person's authority. The merging person's actor is recorded on every
  row the merge touched as the merge's author, beside the original
  author the row keeps.

### 3. Ask-a-person is a held bus item and a tool

- Each `ask-a-person` conflict, and each `refused` one, is a **bus item**
  (D47) addressed to the merging person, carrying the row, the two
  versions, the two authors and HLCs, and the three resolutions (`branch`,
  `main`, `edit`), held until acted. The UI's review queue is the board
  view of those items; the tool is `merge.resolve {merge_id, op_id,
  resolution}`; an AI actor may be granted `merge.resolve` for an org
  that wants it, and the act is recorded as its (invariant 2).
- The merge pauses at the first held operation and resumes when every
  held item is acted, in op order, so a resolution never applies out of
  order with the writes around it.

### 4. Schema first, then data; idempotent, resumable, planned

- **`merge.plan {branch}`** is the dry run: it computes the merge set,
  the divergence set, the schema delta (the branch's `org_migrations`
  not on `main`), the conflicts by kind and rule, and writes nothing.
  Its report is the thing a person reads before `merge.apply`.
- **`merge.apply {branch}`** runs the branch's unapplied migrations on
  `main` through `schema.apply` (D52 §1) first; a migration that fails
  aborts the merge with no data applied. Then the data operations in
  op order, each through the write API under the rules above, each
  recorded as applied by its `op_id` on a **merge cursor** on the branch
  row. A crash resumes from the cursor; an operation already applied is
  skipped by its id, so apply is idempotent.
- **A merge is itself an operation set on `main`** with its own ids, so
  a device syncing after the merge receives the merged writes the way it
  receives any others, and a second branch forked from `main` after the
  merge sees them in its watermark.
- **After apply** the branch is kept until its TTL (so the report and
  the resolutions stay inspectable), then swept as D59 says; `reset`
  after a merge forks again from the new `main`.

### 5. What a merge never does

- Join or copy at the engine level; every row lands through the write
  API and the predicate (invariant 1).
- Apply an operation under an authority other than its own actor's
  (invariant 11).
- Guess a merge of text (D45's reason); keep-both or ask.
- Merge platform rows (grants, credentials, installs, the bus, the
  registry); a branch is a rehearsal of data and schema, and a change to
  who may do what is made on `main` by the people who may make it.
- Replay across orgs. A merge is within one org's database; cross-org
  is D43 §4 and D49.

## Acceptance scenarios, checked in before the code

1. **Every interface write has an operation.** A mutation test removes
   the operation write from one path at a time (table insert, document
   update, blob link) and the suite fails each time with the message
   naming the path; a host-internal platform write has no operation and
   is listed as such.
2. **The watermark is the fork.** A `clone` branch's watermark equals the
   last op id at the drain; a `snapshot` branch's equals the max id in its
   own log; a write on `main` just before the fork is not in the merge
   set and a write just after is in the divergence set.
3. **Disjoint fields merge; same field conflicts.** Two sides edit
   different fields of one row: both land. Two sides edit the same field:
   the later HLC wins under `lww`, both versions exist under `keep-both`,
   and a held item exists under `ask-a-person`; one test per rule, each
   asserting the row afterwards and the revision or item.
4. **Delete against edit** follows D45 §3 across the merge, both
   directions.
5. **A refusal is a conflict, not a bypass.** A grant revoked on `main`
   after the fork turns the branch's write by that actor into a `refused`
   item; resolving it `main` leaves the row untouched; resolving it
   `branch` applies it under the merging person's authority and records
   both actors; the mutation that skips the predicate on replay is caught
   by the existing grant suite run over a merge.
6. **Schema first, abort whole.** A branch with a migration and data: the
   migration applies before any row; a migration made to fail on `main`
   leaves `main`'s catalogue and rows unchanged and the merge in state
   `failed` with the message.
7. **Idempotent and resumable.** The merge is killed mid-apply (a fault
   injected after N operations); `merge.apply` again applies exactly the
   rest, asserted by op ids on `main`, with no duplicate rows and no
   duplicate revisions.
8. **Plan writes nothing.** `merge.plan` on a branch with conflicts
   reports them and `main`'s log, catalogue and rows are byte-identical
   before and after.
9. **Devices see the merge.** A device's sync cursor (D45) after a merge
   receives the merged writes as ordinary operations.

## Order

1. **The operation log on every interface write** (§1), landing with
   D52 order 2 (the interface over the existing engines), because it is
   the first thing D45's device sync and this record both need. Scenario
   1.
2. **The fork watermark** in D59's `clone` (D59 order 1). Scenario 2.
3. **`merge.plan`**, the conflict computation, the rules. Scenarios 3,
   4, 8.
4. **`merge.apply`** with schema first, the cursor, the refusal path,
   the held items and `merge.resolve`. Scenarios 5, 6, 7.
5. Scenario 9 when D45's device sync reads the log.

Before D59's `snapshot` backing: a snapshot branch cut before the log
exists has no watermark and cannot merge, so the log lands first.

## What lost

- *Logical replication of selected tables.* One-way, breaks on a
  conflicting row, and lands rows without the predicate; passed over by
  Nate.
- *"A branch is never promoted"* (D54 §2, D55 §2). True of the engine,
  not of the platform; replaced.
- *Replaying the events table.* It carries pointers (checked); the
  operation log is the record of writes.
- *CRDTs for merge.* D45's reason stands across the merge.
- *Merging under the merging person's authority.* Invariant 11; a
  refused write is a conflict for a person.
- *A second log for branches.* One log serves devices (D45), branches
  (this) and audit; a second would be a second truth about what was
  written.

## Open

- Retention of the operation log beyond the longest branch plus the
  restore window; compaction shape.
- Whether `keep-both` on a structured field needs a UI beyond the
  revision list; nectar's slots (D55 §3) may be where it first shows.
- Blob merges: a reference written on both sides to different bytes is a
  conflict on the reference field under the row's rule; whether a blob
  released on `main` and still referenced on the branch is restored or
  refused is not decided.
- Rate: a long-lived `dev` branch with many writes merges in op order;
  batching under one transaction per N operations is an implementation
  choice the suite's crash test bounds.

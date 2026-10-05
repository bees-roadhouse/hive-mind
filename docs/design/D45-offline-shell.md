# D45: the shell is a Solid.js client with an offline device store and a cache of recent blobs; apps stay framed; the server stays authoritative

**Decided** 2026-10-05 by Nate, on Pia's recommendation: "i like that. but
yes we want offline capabilities of hive mind, including recent blobs."
Shape by Propolis. **Amends D32 §1 for the shell**, as D44 amended it for
app UI. D32 said offline was "a later decision, not a lost one"; this is
that decision. D38 §6's reasoning that a browser tab cannot hold a database
is superseded by OPFS (below), which is what makes it possible now.

## Why it changes now

D32 picked htmx because apps contributed HTML fragments that the shell
composed, and server-rendered fragments are what htmx is good at. D44 moved
app UI into sandboxed frames on their own origins, so the shell no longer
composes app markup. What remained in htmx's favour was less JavaScript,
which does not outweigh working on a train. The house stack already says it
(DevOps book, page 1921): htmx does not fit offline apps or anything with a
real client loop; Solid.js does.

## The decision

### 1. What moves and what stays

- **The shell** (navigation, the core entities' views, chat, the frame host
  for apps) is a Solid.js client, built to static assets that `hive-webui`
  embeds and serves under the same CSP. There is no Node at runtime; Node is
  a build tool only.
- **Admin and settings pages** may stay server-rendered htmx. They are
  online by nature, and D44 §1's capability tokens protect them.
- **Apps** stay in D44's frames, in any stack, online-only. An app may later
  opt into an offline contract of its own. That is out of scope here, except
  for one rule: it would reach data through the same sync API, never
  through the shell's store.

### 2. The device store is a partial replica, and the server is the authority

Each device holds **SQLite in the browser** (the official WASM build, on the
OPFS SyncAccessHandle-pool VFS, in a worker). A native client later uses the
same schema on a real file. It holds a **partial, per-user, per-org
replica** of the core entities (journal entries, tasks, lists, contacts,
decisions) and their links.

- **What syncs down is what the predicate releases**, read through the same
  API every other client uses, under the device's credential. Under D43,
  that is the org database's row-level security plus `Guard`. There is no
  sync-specific read path, because a second read path is a second
  enforcement point (invariant 1).
- **What syncs up is an operation log**, not rows. Each offline write is
  recorded as an operation with an idempotency key, and on reconnect it is
  replayed through the normal write API. The policy triggers, the
  predicate, and the writer that pins `author_actor` from the credential
  (invariant 11) decide each one exactly as they would online. An operation
  the server refuses comes back as a visible rejection on the device, never
  a silent drop. The device never claims to be the authority on anything.
- **Cross-org data syncs as references only**: the link and the remote id,
  never the other org's rows. Opening one needs the network, because
  D43 §4 resolves it through the other org's daemon.
- **Trust travels with the row.** A row that was `untrusted` on the server
  is `untrusted` in the device store (invariants 9 and 12), and a device
  write after reading untrusted data is marked as such when it replays.

### 3. Conflicts: last writer wins per field, with a hybrid logical clock; text bodies keep both

The rule is per field, not per row, so two devices editing different fields
of one task both win.

- **Structured fields** (a task's state, due date or title; a contact's
  phone number; list membership): **last writer wins per field**, ordered
  by a **hybrid logical clock** stamped on the device and carried in the
  operation, not by wall-clock time on arrival. A device whose clock is
  wrong cannot jump ahead of the server's ordering, because the server
  clamps an HLC that is too far ahead of its own.
- **Long text** (a journal entry's body, a decision's text, a note):
  **both versions are kept** when two devices edited the same body
  concurrently. The later one becomes current, and the other is kept as a
  conflict revision the person resolves. No merge is guessed.
- **List order** uses fractional indexes, so concurrent inserts do not
  conflict.
- **Delete against edit:** the later HLC wins. An edit after a delete
  restores the row, so a delete that lost the race was intended later.

**Why not CRDTs.** A CRDT document (Yjs, Automerge) would merge text
character by character. It would also make the stored form of an entry a
CRDT encoding that the server must understand to run triggers, full-text
search (D41) and row-level security over it, so every policy that reads the
body would read through a decoder. Per-field last-writer-wins keeps the
server's rows plain, and keeping both text versions is honest about the one
place a merge would matter. A CRDT body for one entity type can come later
behind this rule if real use shows conflicting text edits are common. That
would be measured, not assumed.

### 4. Recent blobs live on the device too

A device keeps a **bounded cache of blob bytes** (attachments, images,
voice notes) in OPFS, beside the store.

- **"Recent"** means used or created on this device in the last **30
  days**, within a budget of **1 GiB per device**, both adjustable per
  device in settings. Eviction is least recently used once the budget is
  reached, and the time window is only a ceiling. The person can pin a blob
  to keep it regardless.
- **A blob created offline is never evicted until it has uploaded.** If
  the budget fills with unuploaded blobs, the device says so and refuses
  new ones rather than lose data.
- **Content-addressed, so the bytes dedupe, and invariant 3 still holds.**
  A device uploading a blob the server already has does not gain a
  reference by naming its hash. A reference comes from uploading the bytes
  (the server can skip storing them twice) or through a `link_ref` from a
  reference the credential already holds. A hash alone is never a
  capability, offline or not. That is exactly the shortcut "it dedupes"
  invites, so it gets a test before the code.
- A cached blob is the bytes of a reference the device held. When the
  server revokes that reference, the next sync evicts the bytes.

### 5. Auth offline and data at rest

- **A device session survives offline for a bounded time**: 14 days by
  default, set by the org. Past that, the device's data stays encrypted and
  unreadable until it reconnects and re-authenticates.
- **Revocation takes effect on next contact**: a revoked device is told to
  wipe its store and blob cache, and it does so before showing anything.
  That is the strongest guarantee an offline device permits, and the record
  says so instead of implying more.
- **Local data is encrypted at rest** with a key held as a non-extractable
  WebCrypto key in the origin's storage. Honestly stated: this protects a
  copied disk or a profile lifted from the browser's storage, not a live
  unlocked session on the device. Where the person enables it, the key
  wraps under a passkey (WebAuthn PRF) so the store also needs the person,
  not just the machine.

## Order

1. **The sync API**, server side: changes since a cursor, per entity type,
   under the credential, and the operation replay endpoint with idempotency
   keys. Built against the store D43 phase 1 produces, and tested on both
   engines.
2. **The Solid shell, online-only**, replacing the htmx shell route by
   route, and the e2e suite carried across.
3. **The device store and the operation log**, then offline writes.
4. **The blob cache**, with the invariant 3 test first.
5. **Offline auth, encryption at rest and remote wipe.**

No more htmx shell UI is built from today. Admin pages are the exception
in §1.

## What lost

- *Keep htmx with a service worker caching pages.* Cached HTML can be read
  offline but not written to, and the ask is to work offline.
- *A CRDT per entity.* §3.
- *The device store as a full replica of the org.* It is per user, decided
  by the predicate. A full replica would put other people's rows on a laptop.
- *Writes that sync as rows.* Rows would bypass the triggers and the
  author-pinning writer; operations go through them.
- *IndexedDB as the store.* No SQL, no shared schema with a future native
  client, and the full-text search D41 wants would have to be rebuilt in
  JavaScript.

## Open

- Whether chat history syncs (assumed: the recent conversations, read-only
  offline, since a turn needs the server).
- Search offline: FTS5 in the device store is the candidate, over what has
  synced.
- The exact HLC skew the server accepts before clamping.

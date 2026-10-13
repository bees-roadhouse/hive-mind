# D51: every org and every person gets a key of their own at sign-up, made on their device and never held by the operator; content is sealed under it by default; an org may grant the server a working copy it can take back; rotation is one step and never a flag day

**Decided** 2026-10-13 by Nate: "i want each orgs database to be encrypted
for them. I don't want any access." And: "when an org self-signs up (or
individual user) they have their own encryption key for everything, and
make it simple to rotate and change." Shape by Propolis, after one concern
was put to him and he reaffirmed. **Amends D46 §2b**, which made org data
readable by the server and sealed only its backups and blobs, and
generalises D46 §2a from one person to any context. Builds on D43 (a
database per org), D45 §2 and §5 (the device replica, the bounded offline
session), D46 §2a (the per-field seal with associated data, device
wrapping, the proved recovery key), D48 (the vault and its leases) and D50
(sign-in with a second factor, step-up for key operations).

## The concern, stated once and answered by the shape

A database the operator cannot read is a database the server cannot read
either, and the server is what runs full-text search over org data,
pgvector, the D42 model workers, hosted agent runs over org content (D35)
and vault leases to those runs (D48). Every one of those needs plaintext
on the server at some moment. D46 §2b accepted that and said so: "tier (b)
protects against stolen backups and blob stores, not against the server."

Nate's answer is that the org decides, not the operator. So the shape
below makes **sealed** the default for everyone who signs up, lets an org
grant the server a working copy of its key when it wants server features
over its content, and makes that grant the org's to revoke. **The operator
never holds any org's key in either mode.** What the operator can do in
served mode is read process memory on a host they control; the record says
that plainly rather than implying a boundary that is not there.

## The decision

### 1. One key hierarchy for every context

D43 made a person's own space their personal org, and D46 §1 made a context
one person's space or one org. D51 gives **every context one key**, made
the same way, whether the context is a person or an org of forty:

- **The context key** is a random 256-bit key generated **on the device of
  the person who creates the context**, at sign-up (D50: after the second
  factor is enrolled, before any data is written). The server never sees it
  in plaintext; it receives only wrapped copies.
- **Wrapped per member device.** Each device of each member holds a
  non-extractable key pair (D46 §2a); the context key is wrapped to each
  device's public key and the server stores the wrapped blobs, which it
  cannot open.
- **Wrapped to a recovery key**, generated on the creating device, shown
  once as words, and **proved before the context holds any data** (the
  person re-enters it, the client unwraps with it, only then is the context
  live). For an org, the recovery key may be split: `k` of `n` word sets
  held by different admins, so no one admin can recover alone and no one
  admin's loss locks the org out. The split is chosen at creation and shown
  as a choice, with 1-of-1 the default for a personal org and 2-of-3 the
  suggested default for an org with two or more admins.
- **Per-item subkeys** (`HKDF(context key, item id)`), **per-field seals**
  with associated data binding `(context, entity type, item id, field name,
  key version)`, and **ciphertexts that carry their key version**, exactly
  as D46 §2a has them. Invariant 14 in cryptographic form, unchanged.
- **Adding a member** (an org admin invites; the member accepts on a device)
  is an existing member's device wrapping the context key to the new
  member's device key after the new device shows a short code. Adding a
  device is the same between a person's own devices. Nothing here needs
  the server to hold the key.
- **Removing a member or a device rotates forward** (§4). The removed
  party keeps whatever it already had locally; D45 §5's wipe-on-contact is
  the most that can be done, and the record says so.

D46 §2a's "user data key" is this key for a personal org. Nothing in D46
§2a changes except its name and its reach.

### 2. Two modes, chosen by the org, shown everywhere

A context is in one of two modes. The mode is set at creation, changeable
by an admin with step-up (D50 §4), and **shown on every pane that displays
the context** (D46 §5), beside the tier label it already carries.

**Sealed (the default for every self-signed-up org and person).** Every
content field of every entity, every link edge between the context's
items, every blob's bytes, every document's extracted text and every
embedding is sealed on members' devices under the context key. The server
holds ciphertext and the plaintext metadata D46 §2a already lists: ids,
owner and context, entity type, HLC and timestamps, sizes, counts, which
blobs belong to which item, and the grant and placement rows. Sync, RLS,
quotas, the op log's ordering and author pinning run on those (D45). What
the server cannot do: content triggers, server-side search, server-side
models, readable backups, vault leases of this context's entries to
server-side runs. Each of those is available through an **unlock** (§3) or
on the device (D45, D46 §3).

**Served (the org's explicit choice).** The org grants the server a
**working copy** of the context key: an admin's device wraps the key to
the daemon's own key pair (a key the daemon generates at boot and holds in
memory, with its public half published; the private half is never written
to disk), and the daemon keeps the unwrapped key in memory for as long as
the grant stands. With it, the server does what D46 §2b described: content
triggers, full-text and vector indexes over the context (D41), the model
workers (D42), hosted runs reading the context (D35), vault leases (D48),
and projections to peers (D49) carrying plaintext fields the predicate
releases. At rest, the database's content columns are still ciphertext
under the context key (the daemon seals on write and opens on read),
backups are ciphertext, blobs are ciphertext: a stolen disk, backup or
blob store opens nothing without a key the operator does not hold.

**Said plainly, so nobody is surprised:** in served mode the running daemon
holds the key in memory, and whoever controls that host can read that
memory. Served mode protects against everything except the live server.
The client says that in those words when an admin switches to it, and the
switch is an event the whole org can see. **Revoking the grant** is one
step (§4): the daemon's copy is destroyed, the key is rotated forward so a
copy taken from memory earlier opens nothing new, and the context is
sealed again with its server-side indexes dropped.

**Why both, and why sealed is the default.** Nate's words set the default:
"I don't want any access." The second mode exists because the same person,
for the household, wants hosted agents that read the household's boards
and a search box that works, and that is a server reading content. The
record refuses to pretend one mode does both. It gives the org the choice,
labels it, and makes the safer one the one you get by doing nothing.

### 3. Unlocks: server features over sealed content, one purpose at a time

D46 §3 already has the mechanism for a person's heavy models; D51 makes it
the general way a sealed context lets the server do work:

- A member with the right grant releases, from their device, a **purpose
  key**: the context key wrapped to the daemon's boot key pair together with
  a purpose (`index`, `embed`, `run <agent>`, `lease <entry>`), a scope
  (the whole context, a board, an item) and a lifetime (minutes to hours;
  the org sets the ceiling).
- The daemon unwraps it in memory, does exactly the named work, writes
  back only ciphertext the devices can open (or, for an index, keeps the
  derived index only for the unlock's lifetime), and **destroys the key at
  the deadline**. Every unlock is an event with who, what, how long.
- A **standing unlock** is an unlock with no deadline, and it is exactly
  served mode for one purpose. The client shows standing unlocks on the
  context's security page beside the mode, because three standing unlocks
  for `index`, `run` and `lease` are served mode under another name, and
  the org should see that.

An AI actor (D26 §4) never holds a context key; its run receives an unlock
or a lease and nothing it can keep (D48 §1's tmpfs and environment rule).

### 4. Rotation and change are one step

Nate: "make it simple to rotate and change."

- **Rotate** is one button on the context's security page, behind step-up
  (D50 §4). The device generates a new context key, wraps it to every
  current member device and to the recovery key (or the recovery shares),
  bumps the key version, and the devices re-seal existing data **in the
  background**, newest first, with progress shown. Ciphertexts carry their
  version, so old and new coexist and nothing stops working mid-rotation.
  The old key is kept, wrapped, until the last ciphertext under it is
  re-sealed, then destroyed; the client shows "rotation complete" when it
  is, and a rotation that stalls says so rather than quietly leaving two
  keys forever.
- **Rotation happens automatically** on member removal, device revocation
  and the revocation of a served-mode grant or a standing unlock, because
  each of those means a copy may exist that should open nothing new. The
  person sees it happen; they do not have to remember it.
- **Change** means moving where the key lives, not what it is: wrap the
  context key additionally (or instead) to an **external key** the org
  controls, which is a D48 vault entry of kind `wrapping key` that may live
  in the org's own 1Password vault through D48 §4, or in a KMS or HSM later
  behind the same seam. Bring-your-own-key is therefore a wrapping choice,
  made and unmade in one step, and never a migration.
- **Recovery** is the recovery key or shares (§1). If a person loses every
  device and the recovery key, the data is gone, and the client said so in
  those words at creation. There is no operator escrow, because an operator
  who could recover could read; Nate's sentence rules it out.

### 5. What this changes in the records it touches

- **D46 §2b** is superseded: an org's live content columns are sealed under
  the org's key, not only its backups and blobs; the server reads them only
  in served mode or under an unlock. D46's tier table becomes: (a) sealed,
  the default for every context; (b) served, by the org's grant; (c)
  infrastructure, Pia's side, unchanged.
- **D43 §2** is unchanged in structure: RLS and the predicate run on
  metadata and grants, which stay plaintext; the predicate never needed
  content. **D43 §3** (full text and vectors in the org's database) becomes
  served-mode or unlock-time behaviour; in sealed mode those indexes live
  on the devices (D45, D46 §3).
- **D48 §3** stands and sharpens: "server-leasable" for an entry is the
  vault's word for "this entry's context is served, or this entry has a
  standing `lease` unlock"; a sealed context's entries lease only through
  an unlock or on the device.
- **D49**: a projection to a peer carries plaintext fields only when the
  source context is served or an unlock covers the projection; a sealed
  context projects metadata and ciphertext the peer's members cannot open
  unless the key is also wrapped to them, which is the end-to-end shared
  board D46 left room for, and is still left for later.
- **D50 §4**: creating a context, switching mode, granting or revoking a
  served-mode copy, rotating, changing the wrapping, and releasing a
  standing unlock all require step-up.

### 6. What the daemon holds, in one table

| | sealed | served |
|---|---|---|
| context key in daemon memory | only inside an unlock's lifetime | yes, until revoked |
| context key on daemon disk, in the database, in a backup | never | never |
| content columns in the database | ciphertext | ciphertext (sealed on write by the daemon) |
| server-side indexes over content | none, or for an unlock's lifetime | yes |
| who can read a stolen disk, backup or blob store | nobody | nobody |
| who can read the live server's memory | the host's controller, and it holds nothing of this context's | the host's controller |
| server features over content | through unlocks, or on the device | yes |

## Acceptance scenarios, checked in before the code

1. **The server never sees the key.** Creating a context on a test device
   produces wrapped blobs on the server and no plaintext key in any
   request body, database row, log line or file; the test plants a marker
   in the key material and scans everything the daemon wrote.
2. **Sealed means sealed.** With a sealed context's database and blob store
   dumped, no content field, link edge, extracted text or embedding is
   recoverable; the metadata D46 §2a lists is, and the test asserts both
   halves.
3. **A moved ciphertext fails to open** (D46's test, now for every
   context): swap a sealed field between rows, items or contexts and the
   open fails, never decrypting as the wrong thing.
4. **Served opens, revoked closes.** Granting a served copy makes a
   server-side search return results; revoking it drops the index, rotates
   the key, and the same search returns nothing; a key copied from memory
   before revocation opens no ciphertext written after it.
5. **Rotation never stops the world.** During a rotation of a context with
   more rows than one background pass handles, every read and write
   succeeds, old and new versions coexist, and "rotation complete" is
   reported only when the last old-version ciphertext is gone (*Check the
   clock*: the test asserts the completion event comes after the last
   re-seal, not after the button).
6. **Removal rotates.** Removing a member bumps the key version and the
   removed device's wrapped copy no longer exists on the server; its old
   copy opens nothing written afterwards.
7. **Recovery works and only the recovery works.** A context with every
   device gone is recovered with the recovery words (or `k` of `n` shares);
   with fewer than `k`, the refusal names the threshold.
8. **Unlocks expire.** An `index` unlock of one minute leaves a usable index
   for one minute and none after; the daemon's memory holds no key after
   the deadline (asserted by the daemon's own key inventory endpoint, which
   lists held keys by context, mode and expiry and never their bytes).
9. **No operator path.** There is no route, flag, environment variable or
   CLI subcommand by which the daemon's operator obtains a context key or
   reads sealed content; the test enumerates the surface and the
   `hive-repodocs` guard asserts the record's sentence stays in `CLAUDE.md`
   once it is added there.

## Order

1. **The key hierarchy and the client-side seal**, which is D46 order step
   3 widened to every context: the context key at sign-up, device wrapping,
   the proved recovery key, the per-field seal, rotation with versions.
   Scenarios 1, 2, 3, 5, 6, 7. Lands with the D45 client's first store work.
2. **The daemon's boot key pair and the key inventory endpoint**, so a key
   can be wrapped to the server at all; the daemon seals content columns on
   write in served mode. Scenario 9's enumeration starts here.
3. **Unlocks** (§3) for `index`, `embed`, `run` and `lease`; scenario 8.
4. **Served mode** as a standing grant with revoke-and-rotate; scenario 4.
   D41's indexes and D42's workers become served-mode or unlock-time work.
5. **Recovery shares** for orgs and **external wrapping keys** through the
   D48 vault.
6. The `CLAUDE.md` invariant: "the operator never holds a context key", once
   the code exists for it to bind.

## What lost

- *Transparent database encryption with a key the operator keeps.* It is
  what D46 §2b was, and it protects against stolen media only; Nate's
  sentence is about the operator.
- *Operator escrow of the recovery key.* An operator who can recover can
  read.
- *One mode for everyone.* Sealed-only loses server AI and search for the
  household that wants them; served-only is D46 §2b with a new name. The
  org chooses, and the choice is shown.
- *Rotation as a migration job.* Versions on ciphertext and background
  re-sealing make it a button; a stop-the-world re-encrypt would make
  people avoid rotating, which is worse than any single key's age.
- *Per-row keys from a server-side KMS.* The server would hold the keys;
  §6's table would have "yes" in the wrong cells.
- *Postgres-level encryption per database.* Community Postgres has no
  per-database transparent encryption, extensions that add it are not in
  the CNPG image and would still hold the key server-side, and SQLite has
  nothing comparable; application-level sealing under the context key works
  on both engines and on the device.

## Open

- The end-to-end shared board across orgs (a key wrapped to another org's
  members), left open by D46 and still open.
- Exact unlock lifetime ceilings and the recovery share defaults; set with
  the first orgs.
- Whether a served-mode grant should expire and need renewal by an admin
  (assumed: it may, as an org setting; default no expiry, shown).
- Hardware-backed device keys (passkey PRF, platform keystores) as the
  device key pair, which D45 §5 already points at.

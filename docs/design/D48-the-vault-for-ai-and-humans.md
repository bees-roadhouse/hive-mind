# D48: the vault has two faces over one store: humans see items they own, an AI only ever holds a lease; 1Password is a synced driver behind the seam, with the vaults it cannot reach said out loud

**Decided** 2026-10-13 by Nate: "let's ensure the secrets vault works very
well. Made for AI and Humans. sync with 1password vault should be available."
Shape by Propolis. Builds on `docs/vault.md` (the lease design, a seam not a
store, a backend as a guest), D35 (what the vault never holds), D43 §1 and §8
(secret references in the registry, resolved through this seam), D46 (the
three encryption tiers; an item lives in one context) and D47 (an ask is an
item). This record turns the design note into decisions, and the decisions
into slices for #87.

## What "works very well" has to mean, for each face

**For a human**, a vault is the thing they already use: items with a title,
fields, notes, tags and URLs, grouped where they expect them, searchable,
editable, with a history. If it is worse than 1Password they will keep the
secret in 1Password and paste it into the platform, and the paste is the
copy nobody can revoke. So the human face has to be at least as convenient
as the tool they have, and the way to get there is not to compete with it:
**the item they see here can be the item they see there**, kept in step.

**For an AI**, a vault is the thing it must not be able to keep. An agent
run needs a key for a few minutes and a tool call needs a token once. The
AI face is therefore not a read at all. It is a **lease**: time-boxed,
issued to an actor acting for a principal (invariant 2), injected where a
snapshot cannot capture it, recorded against the run, revocable by expiring
rather than by hunting for copies. An AI can **ask** for a credential it
does not hold, and the ask is an item the owner answers (D47); it cannot
read one, and it cannot learn a value by any path other than a lease it was
granted.

Both faces are over **one store**, so that what a human sees as "Nate's
Anthropic key, version 3, leased to propolis for run 8f2…, expires 14:10"
is the same row the lease was cut from. A vault with a human store and an
AI store would be two stores for one secret, and they would drift the way
every pair does.

## The decision

### 1. Entries are owned, versioned, leased; the lease is the only exit

Unchanged from `docs/vault.md`, now decided:

- An **entry** is owned by a principal (a person, or an org; the context it
  lives in under D43/D46), has a kind (`api_key`, `token`, `password`,
  `login`, `ssh_key`, `certificate`, `note`, `document`), a title, typed
  fields of which some are **secret** and the rest **plain** (a username and
  a URL are plain; the password is secret), tags, and an optional
  **external reference** (§4).
- **Versioned.** A change to a secret field adds a version. A lease names
  the version it was cut from; "which key did that run use" stays
  answerable after a rotation, and a run in flight finishes on the version
  it started with.
- **Leased, never read, by an AI.** `vault.lease {entry, purpose, ttl}` is
  the only call that produces a secret value for a run, and it produces it
  into the run's environment or tmpfs (`docs/vault.md`: never a layer,
  because `podman commit` cannot capture either). The lease row carries
  `actor`, `principal`, `run`, `entry`, `version`, `purpose`, `issued_at`,
  `expires_at`, `revoked_at`. Automated runs always lease (D35 §5).
- **A human reads through the client**, which is a different verb
  (`vault.reveal`) with a different audit event and a per-reveal
  confirmation, and never from an AI session's credential: a credential
  whose actor is an AI gets the lease verb and nothing else, decided by the
  predicate, not by the client hiding a button.
- **Every lease, reveal, ask, version and revocation is an event**, so the
  owner's view of an entry shows its history: who held it, when, for what,
  and whether they gave it back early.

### 2. Grants decide, in the one enforcement point

A vault entry is a grant subject (`subject_kind = 'secret'`) with verbs
`lease`, `reveal`, `edit`, `share`. Absence is deny (invariant 1). The
predicate decides a lease exactly as it decides a collection read, in
`Guard`, and under D43 as a function in the org's database with row-level
security as the second wall. A lease policy is a grant with conditions a
human set: which actor, which purpose, what TTL ceiling, whether an ask is
needed each time or a standing grant is enough. No lease is minted outside
`hive-store`, and the harness and the chat worker receive a lease, never an
entry.

An **ask** (D47) is how an AI gets a grant it lacks: `vault.ask {entry or
description, purpose}` creates an item addressed to the entry's owner, and
the owner's reply is a grant (standing or one-shot) or a refusal with a
reason, attached to the ask. The vault's open question, whether an AI
should be able to name a credential it was not given, is answered: it may
name one and be refused; it may not read one. Naming is how a run that
needs a credential the person forgot to grant gets unstuck without a human
pasting a value into a chat.

### 3. Encryption, by context, and the one honest exception

D46's tiers apply to the vault as to every entity, with one consequence
that has to be said plainly.

- **Org entries** (an org's API keys, service credentials, shared logins)
  are org data: readable by the server, because the server is what leases
  them to runs. At rest they are sealed under a **per-org vault key**, which
  is itself wrapped by the daemon's **master vault key**; versions carry
  their key version so rotation is never a flag day. The master key comes
  from a key source that is never the database and never an image: a
  sealed secret or file at boot today (`docs/vault.md`'s boot-derived key,
  now per org), a KMS or HSM later behind the same seam. Rotating the
  master re-wraps the per-org keys; rotating an org key re-seals its
  entries in the background.
- **Personal entries** are tier (a) by default: sealed on the person's
  devices, the server never holds the key. **A tier (a) entry cannot be
  leased to a server-side run**, because the server cannot read it. That is
  the exception, and the design does not paper over it: the client offers
  two shapes for a personal entry, shown with their meaning:
  - **Device-only** (default): reveal on your devices, usable by a native
    client's local runs later (D45 open), never leased by the server.
  - **Server-leasable**: stored at the org tier of the person's personal
    org, so the server can lease it to that person's hosted runs. The
    client says, in those words, that the server can read it.
  A person's Anthropic API key for their hosted agents is the typical
  server-leasable entry; a personal banking login is the typical
  device-only one. A D46 §3 per-session unlock can lease a device-only
  entry for a bounded time with an audit row, when that is what the person
  wants, and nothing does it silently.
- **What D35 excludes stays excluded.** A Claude.ai or ChatGPT session
  token is never an entry, server-leasable or otherwise; the per-principal
  config volume is where the binary keeps its own sign-in, and a vault
  compromise does not become an account compromise.

### 4. 1Password is a driver behind the seam, and it syncs

`docs/vault.md` already named 1Password and Bitwarden as backends worth
supporting out of the box and said the backend is a WASM guest on the
ordinary ABI, so it holds no sockets, caches nothing the host cannot see,
and reaches exactly one remote host through the egress proxy. That stands.
What is decided here is what "sync" means and what it cannot mean.

**A link** joins one 1Password vault to one hive-mind context (an org, or
a person's personal org). It is set up by a human who holds both: they
create or choose a 1Password vault, grant the integration access to it on
the 1Password side, and register the link here with the integration's
credential, which is itself an entry at the org tier (so the thing that
reads 1Password is leased to the driver per sync, like any other secret).

**Mode per link**, chosen at setup and shown on every synced entry:

- **Mirror**: 1Password is the source of truth; entries here are read-only
  copies, leasable, and an edit is offered as "open in 1Password". The
  simplest mode and the right default for a household that already lives in
  1Password.
- **Sync**: both sides edit. Each side keeps its versions; a change on one
  side becomes a new version on the other. A concurrent edit of the same
  secret field keeps **both** versions and marks the entry as needing a
  human's eye (D45 §3's rule for long text, applied to the one field class
  where a guessed merge would be a wrong secret). A deletion on either
  side **archives** on the other and never deletes; the owner empties the
  archive on purpose.

**The mapping** is per entry: 1Password item categories map onto entry
kinds, fields onto fields with the secret/plain distinction kept from
1Password's concealed flag, tags onto tags, and the 1Password item id is
the entry's external reference, so the same item is never imported twice
and an `op://vault/item/field` reference the household already uses in a
launcher resolves to the same thing on both sides.

**What it cannot do, said at setup and in the record.** 1Password does
not let an integration (a service account, or a Connect server) reach the
built-in **Private, Personal, Employee** or default Shared vaults
(1Password's own documentation of service accounts). So a person's Private
vault **cannot be linked**, and the client says so with the fix: create a
vault (the household's convention would be `Hive Mind — <name>`), move or
share the items into it, and link that. The link setup refuses a vault it
cannot read with that specific message rather than a generic failure, and
a test asserts the message (CLAUDE.md, *Check which refusal*).

**Rate limits shape the sync, not the other way round.** 1Password's
integration limits are hourly per token and daily per account, with
writes far scarcer than reads (on the household's plan, on the order of a
hundred writes an hour). The driver therefore keeps a **cursor** per link,
syncs on a change here immediately but **batches writes** to the other
side, polls the other side no tighter than its read budget allows, backs
off on the rate-limit error with the retry time shown to the owner, and
never fans one entry edit into one request per field. Leases do not touch
1Password at all: a lease is cut from the entry here, so a hundred runs an
hour cost zero 1Password requests.

**Transport criteria, which outlive the pick.** The driver is a guest, so
it needs: an HTTPS JSON API reachable through the egress proxy; no CLI
process and no native library; a credential that can be leased per sync
and rotated. Measured against 1Password's offerings on 2026-10-13:

- **1Password Connect** (a self-hosted pair of containers with a REST API
  and a credentials file) meets all three. It runs on the cluster, which is
  Pia's side; its credentials file and bearer token are entries at the
  daemon's tier.
- **The service-account SDKs** (Go, JavaScript, Python, "version 0" per
  1Password's own docs; no Rust SDK and no documented wire protocol) do not
  meet "no native library, callable from a guest" today. When they do, the
  driver changes and this record does not.
- **The `op` CLI** is how the operator reads the daemon's own boot secrets
  at launch (the house rule) and is not a driver: it is a process, it wants
  a desktop app or a token in its environment, and a guest cannot run it.

So the first driver speaks Connect. Bitwarden's self-hosted server exposes
an HTTPS JSON API and meets the same criteria; it is the second driver and
is not designed here beyond that sentence.

### 5. Humans and AIs in one view

- An entry's page shows its versions, its links (which 1Password item,
  which mode), its grants (who may lease it and under what policy), its
  **live leases** (actor, run, purpose, expiry) with a revoke button, and
  its history. The person sees exactly what their AIs hold, right now.
- An AI's view of the vault is the list of entries it **may lease** and the
  asks it has open. It never sees a value in that list, and `tools/list`
  shows it `vault.lease` and `vault.ask`, never `vault.reveal`.
- A lease shows up in the run's record (`agent_runs`, as `docs/vault.md`
  requires), so "what did this run have access to" is answered from the
  run as well as from the entry.
- Nothing about leases is written into 1Password; the audit is this
  platform's. A synced item carries no trace of which AI used it, because
  1Password is not the system of record for that and writing it there would
  spend the write budget on noise.

### 6. The daemon's own secrets go through the same seam

D43 §1 and §8 already say the registry holds secret **references** and
resolves them through this seam. The daemon's database credentials, the
blob store's, the Connect token, the per-org vault keys' wrapping: each is
an entry at the daemon's tier, leased to the component that needs it at
boot or on rotation, with the master key the only thing that comes from
outside. "Until the vault lands it names a file or environment key" (D43)
is the transition and this record is what ends it.

## Acceptance scenarios, checked in before the code

1. **A leased value is never at rest in plaintext.** After a lease, a scan
   of the store's files (both engines) and of a `podman commit` of the run's
   container finds no byte sequence of the value; the test plants a marker
   value and greps.
2. **A lease is pinned.** A lease cut for actor A acting for principal N
   cannot be redeemed by actor B, nor by A acting for M; the refusal names
   the rule.
3. **Deny by default.** An AI credential's `vault.reveal` is refused by the
   predicate with the grant message, not a missing-tool error; a
   grant-less `vault.lease` likewise. A mutation test removes the Rust
   check and the SQL policy one at a time and the suite fails each time
   (*Check what the mutation removed*).
4. **Versions.** A rotation while a lease is live leaves the lease on the
   old version until expiry and cuts new leases from the new one.
5. **Sync both ways** against a Connect server in the container test tier
   (as the egress suite has a real proxy): an item changed in 1Password
   becomes a version here within one read cycle; an edit here reaches
   1Password within the write budget; a concurrent edit of one secret field
   keeps both versions and flags the entry; a delete on either side
   archives on the other.
6. **The Private vault refusal**: linking a vault the integration cannot
   read fails with the specific message, and the test asserts that
   message.
7. **The ask flow**: an AI's `vault.ask` creates an item addressed to the
   owner; the owner's grant reply makes the next `vault.lease` succeed and
   the refusal reply makes it fail with the owner's reason.

## Order (the #87 slices, re-cut)

1. **Entries, versions, leases in `hive-store`** with the grant kind, the
   per-org key wrapping and the master key source, on both engines under
   D43 phase 1 (after slice 3, so the predicate function carries the new
   kind once). Scenarios 1 to 4.
2. **The chat worker and the harness resolve a run's credential**: config
   volume, else lease, else refuse (D35 §5); the lease lands in the
   environment or tmpfs and on `agent_runs`.
3. **The daemon's own secrets** through the seam (§6), ending D43's
   file-or-env transition.
4. **The 1Password driver** as a guest speaking Connect, with the egress
   allowlist widened per sync to that one host; scenarios 5 and 6. Connect
   on the cluster is an infra ask to Pia.
5. **The client's vault screens** (D45 order step 2): the entry page of §5,
   link setup with the Private-vault message, the device-only versus
   server-leasable choice.
6. **The ask flow** on D47's items; scenario 7.
7. Bitwarden, when someone needs it.

## What lost

- *Running `op` in the daemon or a sidecar as the integration.* A process
  with a long-lived token in its environment, outside the egress proxy,
  that a guest cannot drive. The CLI stays what it is in this house: the way
  the operator seeds boot secrets.
- *An in-process 1Password SDK.* None exists for Rust, and an in-process
  client would hold the integration token where every run can see the
  process, with no egress confinement.
- *1Password as the only store, no vault here.* Then there are no leases,
  no per-run audit, no versions a run can be pinned to, and every run's
  access is a read of the real item against the write budget.
- *Linking a Private vault.* Impossible by 1Password's rules; said at setup
  instead of discovered at the first sync.
- *Leasing tier (a) entries from the server by default.* It would make
  "the server never holds the key" false for the one class of data where a
  person most expects it to hold. The server-leasable choice is explicit.
- *Guessing a merge on a concurrent secret edit.* A merged secret is a
  wrong secret; both versions are kept and a human picks.

## Open

- The master key source on the cluster (sealed secret now; KMS or HSM
  later), and who may rotate it. Pia's side, with the criteria above.
- Whether a native client (D45 open) can lease a device-only entry to a
  **local** run without the server; the shape is a lease cut on the device,
  and nothing is designed.
- Sharing an entry to another org through a D43 §4 link: a lease over a
  federation call. Not until links exist.
- Whether the household wants one `Hive Mind` 1Password vault per person
  or one for the household org; both are a link each and the record does
  not need to choose.

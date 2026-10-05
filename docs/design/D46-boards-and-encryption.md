# D46: boards are where things are shared; a person's own data is encrypted on their devices; documents get meaning on whichever side can read them; one workspace across every context

**Decided** 2026-10-05 by Nate, thinking aloud across several messages, and
relayed by Pia: "i also want client side encryption, for the per user
stuff"; "org level journal? or just org level white-boards for sharing only
meaningful data such as lists / notes / scratch? ... data can relate and
emerge from the different white boards"; "org level documents and user
level documents? stored as blobs, ocr'd with ai client side or server side
for org, and embedded as well for meaning? ... we should [encrypt]"; "i'd
like the user to be able to make their own 'user board(s)' as well. switch
between orgs and users.. or be able to hold a few up side by side". Shape by
Propolis. Builds on D43 (a database per org, a person's own space as their
personal org) and D45 (APIs only, a full offline replica on the client).

## 1. Contexts, boards, placement, links

**A context** is one person's own space or one org. Under D43 each is its
own database: the personal space is the person's personal org, which is
exactly why D43 gave everyone one.

- **The journal is personal, and there is no org journal.** Sharing happens
  on **boards**.
- **A board** is a shared space of cards: lists, notes, scratch, tasks,
  decisions, documents. An org has many boards, and a person has any number
  of their own **user boards**, which use the same model in their own
  context.
- **Placing is not copying.** One item can sit on several boards in its
  context (a `placements (item, board, position)` row each, fractional
  positions per D45 §3).
- **Emergence is links.** Any item links to any other, across boards, and
  from a journal entry to a board item. Every item shows its backlinks and
  where it is placed. Nothing rearranges data behind the person's back. A
  freeform canvas view can come later over the same rows.

**An item lives in exactly one context**, because a context is a database.
That makes crossing contexts an explicit act, never a side effect:

- **Private to org (the share step).** Placing an item from the person's own
  space onto an org board **moves** it into the org's database. It is
  decrypted on the device and becomes org data, readable by the server
  (tier b), after a warning that names the org and says so. The personal
  space keeps a link to it. One copy, so the two cannot drift. The person
  can choose "copy instead", which is labelled as such.
- **Org to org.** A cross-org link per D43 §4, a reference and never a copy,
  unless the person copies on purpose. There is never a silent copy across
  orgs.
- **Org to private.** A copy, labelled. The org's item stays the org's.

## 2. Three encryption tiers

| tier | covers | who can read it | protects against |
|---|---|---|---|
| **(a) personal** | the content of everything in a person's own space | that person's devices only; the server never holds a key | the server, its operators, its backups, a stolen disk |
| **(b) org** | an org's database backups and its blobs | the server (it needs to: triggers, RLS on content, search, links, models) | a stolen backup or blob store, and one org's backup opening another's |
| **(c) infrastructure** | the volumes everything sits on | the hosts | stolen disks; Pia's side |

An end-to-end encrypted board (a members-only key, wrapped to each member)
fits this model as tier (a) generalised from one person to a few. It is
left room for here and not designed.

### 2a. Personal: client-side, keys never on the server

**What is encrypted:** in the person's own context, every content field of
every entity (journal text, titles, bodies, list items, notes, contact
fields, decision text, card text), link edges between private items (who
links to whom is meaning too), blob bytes, the text extracted from
documents, and **embeddings**, because an embedding leaks the meaning of
what it came from.

**What stays plaintext, accepted explicitly:** ids, the owner and context,
entity type, HLC and timestamps, sizes, counts, and which blobs belong to
which item. Sync, row-level security, quotas, the op log's ordering and the
author pinning (invariants 2 and 11) all run on these. What this leaks:
how much a person writes, when, of what type, and how big it is. Not what
it says.

**The construction:**

- A random **user data key**. Each item gets a subkey,
  `HKDF(user data key, item id)`. Each field is sealed with AES-256-GCM
  (WebCrypto's AEAD) under a fresh random nonce, with **associated data
  binding (owner, context, entity type, item id, field name, key
  version)**. A ciphertext swapped into another row or field by the server
  fails to open rather than decrypting as the wrong thing. Invariant 14 in
  cryptographic form: the seal is keyed on every dimension its meaning
  depends on.
- **Per device:** a non-extractable WebCrypto key pair. The user data key
  is wrapped to each device's public key. The server stores only the
  wrapped keys, which it cannot open.
- **Recovery key:** generated on the device, shown once as words, and used
  to wrap the user data key one more time. **Mandatory and proved before
  any data is encrypted**: the person re-enters it, the client unwraps with
  it, and only then is encryption switched on. Losing every device and the
  recovery key loses the data, and the client says that in those words at
  setup. There is no server escrow unless Nate asks for it, and that would
  be a decision of its own.
- **Adding a device** needs approval from an existing device (the new
  device shows a short code, the existing one confirms it and wraps the key
  to the new public key) or the recovery key.
- **Revoking a device rotates forward**: a new user data key, new writes
  under it, and existing data re-encrypted in the background by a device
  that holds both. The revoked device keeps whatever it already had
  locally; no scheme can take that back, and D45 §5's wipe-on-contact is
  the best that can be done. Ciphertexts carry their key version, so
  rotation is never a flag day.

**The op log still holds** (D45 §2–3). Last-writer-wins per field compares
HLCs, which are plaintext, so the server orders encrypted fields without
reading them. "Both versions kept" for long text is detected the same way,
from the base HLC an operation was written against. Showing and resolving
that conflict happens on the device, which is the only place that can read
it. What the server loses for tier (a) data: content triggers, search,
server-side models, and readable backups. Capping and CNPG back up
ciphertext.

### 2b. Org: readable by the server, keyed per org at rest

Org data has to stay readable by the server. Tier (b) is therefore about
the copies that leave it:

- **Backups** of an org's database are encrypted with that org's key
  before they leave the daemon's hands (`pg_dump` through an AEAD stream
  keyed per org). The registry holds a reference to the key, never the
  key.
- **Blobs** of an org are encrypted by the blob layer with a per-org key
  (D40's driver gains the seal), so a copied blob store does not open
  every org at once.
- **The live database** sits on tier (c). Per-org encryption of a running
  Postgres would mean a cluster per org; that cost is not paid here.
  **Said plainly: tier (b) protects against stolen backups and blob
  stores, not against the server.**
- A self-hosted org (D43 §8) holds its own at-rest story.

### 2c. Infrastructure (Pia's side, recorded for context)

As of 2026-10-05: nas138's pool is ZFS-encrypted (aes-256-gcm), so the
democratic-csi volumes and the backups there are encrypted at rest.
Longhorn volumes are **not** encrypted, and the CNPG `data/data` disks
look unencrypted. Both are raised as brh-infra work.

## 3. Documents at both levels

A document is a blob. Its **extracted text** (OCR, or the text layer) and
its **embeddings** are rows tied to the blob reference (invariant 3: tied
to the reference, never to the hash).

- **Org documents:** OCR and embeddings on the server, by D42's model
  worker, with vectors in pgvector in the org's database (D43 §3). Search
  and "meaning" are server-side.
- **Personal documents:** tier (a), so the server cannot read them. OCR
  and embedding run **on the device**: WASM OCR, with WebGPU where
  present, and a small embedding model, with vectors in the device store
  (sqlite-vec or the equivalent) and vector search done locally over the
  full replica (D45). Extracted text and vectors sync up encrypted, so a
  second device does not have to redo the work. Criteria for the picks,
  which outlive them: runs offline in a browser worker, fits a phone's
  memory, a licence we can ship, and the same model on every device, so
  vectors from two devices are comparable.
- **Heavy models on private data** need an **explicit, per-session unlock**.
  The person releases a session key to the server for a bounded time and
  a named purpose. The server processes in memory, writes back only
  ciphertext the device can open, and records the unlock in the audit.
  Never a silent server copy.

**Consequence, so nobody is surprised:** the hosted agents (chat, the
harness, D35/D37) run on the server. They **cannot read a person's journal
or private items** unless that person unlocks for the session. The person's
own AI features over private data run on the device, or with that unlock.

## 4. Dedupe for encrypted blobs

A personal blob is sealed under `HKDF(user data key, plaintext hash)`, so
the same file sealed twice **by the same person** yields the same
ciphertext and dedupes. Across people, nothing dedupes, and that is the
point: a server comparing plaintext hashes across users would learn that
two people hold the same file. The server addresses personal blobs by the
**ciphertext's** hash, and never sees the plaintext's.

## 5. One workspace across contexts

- **The client is signed in to every context at once**: the person's own
  space and each org they belong to, each with its own credential and its
  own replica (D45, D43).
- **A context switcher** (me, each org), **plus side-by-side panes**: open
  several boards at once from any mix of contexts. Each pane is labelled
  with its context and its tier, and the tier is shown, not implied.
- **Dragging across panes is a placement into another context**, so §1's
  rules apply: private to org shows the share warning, org to org makes a
  reference, and nothing is copied silently.
- **Pane layouts are per device**, saved locally, and work offline. They
  are not synced; a layout is a preference about a screen, not data.

## Order

1. Boards, placements and links in the schema, under D43 phase 1. Plaintext
   first, on both engines.
2. The workspace, contexts and panes in the D45 client.
3. Tier (a): keys, recovery proved before first use, the field seal with
   associated data, device approval and rotation. **A test first that a
   ciphertext moved to another row or field fails to open.**
4. The share step (private to org) with its warning.
5. Personal document OCR and embeddings on the device. Org documents on the
   D42 worker.
6. Tier (b): per-org backup and blob keys.

## What lost

- *An org journal.* Nate's call. The journal is the person's own; boards
  are what people share.
- *Encrypting org data client-side too.* It would take away the triggers,
  RLS on content, search and the models that make shared data useful, and
  the people sharing it trust the org's server by choosing it.
- *Shared plaintext-hash dedupe.* §4.
- *Server-escrowed recovery.* It would make "the server never holds a key"
  false. Only if Nate asks.
- *Two copies on share.* They drift, and the private copy would keep
  pretending to be private.

## Open

- The end-to-end encrypted board (tier a for a group): membership changes
  and key rotation.
- Which OCR and embedding models meet §3's criteria, measured on a phone.
- Whether the person's own AI actor gets a standing unlock, or whether
  unlocks are always per session (assumed: per session).

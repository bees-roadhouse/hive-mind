# D52: an org's database is provisioned on sign-up and the org owns and evolves its schema; one storage interface over blobs, tables and documents, the same on the guest ABI, the client API and the tools; backends are drivers an org adds, tested by the host's own suite

**Decided** 2026-10-13 by Nate: "we need a way to dynamically spin up org
databases they can own the schema for and change.. and make an abstracted
storage interface for blobs, tables, and, 'documents'. like openai did."
And, a minute later: "and the ability to add custom backends that connect
to that interface. through a storage driver add." Shape by Propolis. The
OpenAI reference could not be verified from here (no product by that
description was found on 2026-10-13), so this record stands on hive-mind's
own terms and does not claim a comparison. Builds on D43 (a database per
org, provisioning automated, bring-your-own Postgres), D41 (full text and
vectors on collections), D40 and D11 (the blob driver seam), D44 and D45
(APIs only), D48 (a driver as a WASM guest; the vault for a driver's
credential), D50 (sign-up) and D51 (a context key; sealed and served).

## What exists, so the record changes the right things

- **Provisioning is already automated** (D43 §6): `org create` writes the
  registry row, creates the database and roles, runs the migrations, flips
  to live, idempotently. D43 §8 lets an org attach its own Postgres. What is
  missing is the trigger: nothing calls it when a person or org signs up.
- **Schema is owned by apps, not orgs.** A manifest declares collections
  (`storage.collections`, D41's `fts(path)` and `vector(path, dim)`), the
  registry provisions a schema per install, and the org can change nothing
  about it except by changing the app.
- **Documents exist; tables do not.** The guest's five storage verbs
  (`storage.insert`, `get`, `update`, `delete`, `query`) are JSON documents
  in a collection, with containment, `search` and `near`. There is no typed
  row surface.
- **Blobs have a seam with one driver** (D40): bytes on the mounted path,
  references and trust in the catalogue (invariants 3 and 8). The S3 driver
  was removed with its tier because two backends was two tiers to keep
  green for a household on one mount.
- **The vault's backend is a guest** (D48 §4): a WASM driver reaching one
  host through egress, with its credential a vault entry. That is the
  driver shape this record generalises.

## The decision

### 1. Sign-up provisions; the org owns its schema from the first minute

- **Sign-up calls `org create`.** When a person enrols (D50) their personal
  org is provisioned; when a person creates an org, it is provisioned,
  with them as its first admin. D43 §6's path is the only path; self-service
  adds the caller, not a second mechanism. An org may attach its own
  database instead (D43 §8), at creation or later, through the same check.
- **An org owns its schema.** Beside the platform's tables (actors, grants,
  credentials, events, the blob catalogue, the item and board tables of D46
  and D47, which the org cannot touch) and beside the apps' schemas (one per
  install, declared by manifests), an org gets a schema of its own,
  `org`, in its database, where it defines **tables** (§2) and
  **collections** (§2) and changes them over time.
- **Changes are versioned migrations the org authors.** `schema.plan`
  takes a desired shape (a table added, a column added or renamed, an index
  added, a column's type widened; drops are explicit and two-step) and
  returns the migration the daemon would run, in the dialect of the org's
  engine, plus what it would do to existing rows. `schema.apply` runs it as
  the org's migration role (D43 §2's `hive_owner_<org>`), inside one
  transaction where the engine allows, records it in `org_migrations`
  (version, text, checksum, who, when) and emits an event. There is no
  down migration: undo is the next forward migration, as everywhere else
  in this repo (`hive-schema`'s rule, applied to orgs).
- **What the platform adds to every org table, and the org cannot remove:**
  `id`, `owner_kind`, `owner_id`, `author_actor`, `created_at`,
  `updated_at`, the HLC (D45 §3), `trust` (invariant 12), `key_version`
  (D51), and the row-level security policy that calls `access_decision` on
  the row (D43 §2). The writer pins owner and author from the credential
  (invariant 11); the org's own columns come after. An org table is
  therefore a content entity like any other to the predicate, the sync API
  (D45) and the seal (D51), with no special path.
- **What the org cannot define:** policy functions, triggers that read the
  credential, anything in the platform schema, a table without the mandatory
  columns, an unqualified reference into another install's schema. The
  planner refuses each with its reason, and a test asserts the message
  (CLAUDE.md, *Check which refusal*).
- **Who may change the schema:** a grant, `subject_kind = 'schema'`,
  verbs `plan` and `apply`, held by org admins by default and grantable to
  an AI actor acting for an admin, so an agent can propose and, if granted,
  apply a change; absence is deny. Every apply needs step-up (D50 §4) when a
  human does it and a standing grant when an AI does, and both are events.
- **Under D51:** an org table's content columns are sealed in a sealed
  context, which means a typed column the server must index (a `WHERE` on
  a sealed integer) is only indexable in served mode or under an unlock.
  The planner says so when it sees one: "this index needs served mode or an
  unlock to be used by the server; on devices it works regardless." The org
  decides, with the cost in front of it.

### 2. One storage interface: blobs, tables, documents

Three primitives, one interface, the same on every surface:

| primitive | what it is | verbs | query |
|---|---|---|---|
| **blob** | bytes under a reference; ownership, permission and trust on the reference, never the hash (invariant 3) | `put`, `get`, `head`, `link`, `release` | by reference only |
| **table** | typed rows in an org-owned table (§1) or an app-declared one | `insert`, `get`, `update`, `delete`, `select` | a constrained form: `where` as a tree of comparisons on named columns, `order`, `limit`, `after` (cursor); never raw SQL from a caller |
| **document** | a JSON document in a collection, schemaless except for declared indexes (today's `storage.*`) | `insert`, `get`, `update`, `delete`, `query` | containment, `search` (words, never syntax), `near` (D41), plus the table form's `order`, `limit`, `after` |

**One interface, three surfaces.** The same verbs with the same shapes and
the same `{trust, data}` envelope (invariant 12) are offered as:

- **guest capabilities** on the ABI (`blobs.*`, `tables.*`, `documents.*`;
  the existing `storage.*` becomes `documents.*` with the old names kept as
  aliases until the checked-in guests move);
- **routes on the client API** (D45), under the device credential;
- **MCP tools** (#90) for agents, with the install or the org as the
  qualifier the way D33 and the eighth key (CLAUDE.md invariant 14) require,
  so `tables.select` on `org/inventory` and on `journal/entries` are
  different tools and never "whichever came first".

**One enforcement point.** `Guard` decides every call before any driver is
reached (invariant 1); the primitive's verbs are grant kinds (`blob`,
`table`, `collection`, with the install or org as subject). A driver (§3)
never sees the facts the decision was made from, only the decided call.

**Why three and not one.** A document store can hold a table and a table
can hold a document, and both can hold bytes. The record keeps three
because each has a query form the others cannot offer without pretending:
blobs are fetched by reference and never searched; tables have typed
comparisons and the engine's indexes; documents have containment, words
and vectors. Collapsing them would give every caller the weakest of the
three or a query language nobody can secure.

**What the org sees.** A storage page per context: its tables and
collections with their schema versions, its blob usage, which driver backs
each primitive (§3), and the queries the AI actors ran, because "what did
the agent read" is a question the owner gets to ask.

### 3. Drivers: a backend an org adds, behind the interface

A **driver** is the backend behind one primitive for one context. The
platform ships defaults and an org may add its own.

- **Default drivers:** blobs on the mounted path (D40); tables and
  documents in the org's database on whichever engine it is (Postgres or
  SQLite, through `hive-db`). A single-box install has these and nothing
  else to configure.
- **A driver is one of two kinds.** A **native driver** is Rust in this
  workspace behind the primitive's trait (`BlobDriver`, `TableDriver`,
  `DocumentDriver`), compiled in, selectable per context. A **guest
  driver** is a WASM guest that implements the driver ABI (the same verbs,
  as host-to-guest calls) and reaches its remote store through the egress
  proxy, allowlisted to exactly that store's host (D48 §4's shape for the
  vault). A guest driver holds no sockets and no ambient state (invariant
  5), so it cannot cache what the host cannot see, and its credential is a
  D48 vault entry leased to it per call batch, never baked in.
- **Adding a driver** (`storage driver add`, or the storage page): name
  the driver package (a native one by name, a guest one by its module
  reference), bind it to a context and a primitive, point it at a vault
  entry for its credential, and run the **conformance suite** against it
  before it takes any traffic. The binding is a registry row keyed on
  `(org, primitive, generation)`: moving a context between drivers bumps
  the generation and drains the old pool (D43 §1's rule, invariant 14),
  with a copy under a suspended generation the way D43 §8 moves a
  database.
- **The conformance suite is the host's, checked in, and the same for
  every driver** (D24: the guests test the host, here the host tests the
  driver). It runs every verb, the query forms, concurrency, size bounds,
  partial failure (a put that dies mid-way leaves no reference), and, for
  blob drivers, the rule D40 kept from the S3 era: a scriptable type is
  never redirected. A driver that does not pass does not bind. A driver
  that passes gets a row saying which suite version it passed.
- **What a driver never does.** Decide access (§2), see plaintext in a
  sealed context (D51: it stores and returns ciphertext and never knows),
  hold a credential of its own, reach a second host, or change the shape
  of the data (a blob driver stores bytes, a table driver stores typed
  rows, a document driver stores JSON; translation is the interface's,
  not the driver's).
- **The S3 driver comes back as a driver, not as the default.** D40 removed
  it because two defaults was two tiers; as an org-added driver it is one
  more conformance run and no household pays for it. The redirect
  delivery D40 gave up stays given up: every read is proxied by the daemon,
  so the catalogue's rule about scriptable types holds for every driver.
- **Failure is the org's.** A driver that is down is that context's
  outage, surfaced on its storage page and in the registry's health (D43
  §8), and no other context waits on it.

### 4. Where tables sit against documents and apps

- An **app** still declares its collections in its manifest; it may now
  also declare **tables** (typed columns, indexes) in the same `storage`
  block, provisioned per install exactly as collections are. The `uses`
  grant (D33, #97) covers both kinds.
- An **org** defines its own in the `org` schema (§1). An app that wants an
  org table reaches it through `uses` with the org as the qualifier, under
  a grant the org makes; absence is deny.
- A **document** collection may gain declared indexes over time the way an
  org table gains columns: `schema.plan` covers both.

## Acceptance scenarios, checked in before the code

1. **Sign-up provisions.** Enrolling a person leaves a live personal org in
   the registry with its database, roles and migrations applied; creating
   an org does the same with the creator as admin; both idempotent on
   retry (D43 §6's state column).
2. **The org owns only its schema.** `schema.apply` creating a table in the
   `org` schema succeeds; the same against the platform schema or an app's
   install schema is refused with the message naming the boundary.
3. **Mandatory columns and RLS cannot be omitted.** A plan without
   `owner_kind` or with a policy function is refused with the reason; a
   table created through `schema.apply` has `FORCE ROW LEVEL SECURITY` and
   a policy calling `access_decision`, asserted by reading the catalogue,
   not by trusting the planner.
4. **Deny by default across all three primitives.** A grant-less `blobs.get`,
   `tables.select` and `documents.query` are each refused by the predicate
   with the grant message, through the ABI, the API and the tools alike;
   one test runs the same call through all three surfaces.
5. **The driver sees ciphertext.** In a sealed context (D51), a test driver
   that records what it received holds no plaintext marker after a
   `tables.insert` and a `documents.insert`; in served mode the same
   driver still receives ciphertext, because the daemon seals before the
   driver.
6. **Conformance gates binding.** A deliberately broken test driver
   (returns a reference for a put that wrote nothing) fails the suite and
   is not bound; the refusal names the failing case.
7. **Generation moves drain.** Rebinding a context's blobs to a second
   driver bumps the generation; reads during the move succeed from the old
   driver until the copy completes, then from the new; the old pool is
   gone afterwards (asserted by the driver inventory, not by timing).
8. **Qualified tools.** With two installs of one app, `tables.select` on
   each is a distinct tool and reaches its own install's rows (invariant
   14's seventh case, re-run for the new primitive).

## Order

1. **Sign-up provisions** (D43 §6 called from D50's enrolment), under D43
   phase 2 when the registry exists. Scenario 1.
2. **The interface** over the existing engines: `tables.*` added,
   `storage.*` renamed to `documents.*` with aliases, `blobs.*` exposed on
   the ABI and the API as the catalogue already is, the three traits with
   the default drivers behind them. No new storage behaviour. Scenario 4.
3. **Org-owned schema**: the `org` schema, `schema.plan` and `schema.apply`,
   `org_migrations`, the grant kind, the mandatory columns and policy.
   Scenarios 2, 3, 8.
4. **The driver registry and the conformance suite**, with the default
   drivers passing it first. Scenarios 6, 7.
5. **The first added drivers**: S3-compatible blobs (native), and a guest
   driver as the proof of the ABI (a remote document store through
   egress). Scenario 5 across both.
6. App-declared tables in manifests.

## What lost

- *Raw SQL from guests, agents or the client.* One query language nobody
  can secure, against a database whose second wall is RLS; the constrained
  forms are what the predicate can reason about.
- *One primitive for everything.* §2's reason; each has a query form the
  others cannot honestly offer.
- *Orgs editing the platform or app schemas.* The platform tables are the
  walls; an app's schema is the app's contract with its data.
- *Down migrations.* Undo is forward, as `hive-schema` already does.
- *Drivers that decide anything.* A driver is a place bytes go; the
  interface decides, seals and audits before it is called.
- *The S3 driver as a default again.* D40's reason stands; as an added
  driver it costs its users and nobody else.

## Open

- The exact grammar of the `select` and `query` forms (joins across org
  tables within one context are wanted and bounded; across contexts never,
  D49).
- Quotas per primitive per context, and who sets them.
- Whether an org may bring a driver as a native Rust crate (a build) rather
  than a guest. Assumed no: natives are the workspace's, guests are the
  org's; a native driver is a PR here.
- A column type for sealed-and-searchable (order-preserving or
  deterministic encryption) so a sealed context can index a typed column
  server-side at a stated leakage cost. Not designed; named so nobody
  assumes it is free.

# D43: the server store is Postgres, one database per organization behind a registry; row-level security inside an org; cross-org by link, over the network; SQLite stays behind the seam

**Decided** 2026-10-04 by Nate: "let's switch it back to postgres but keep
our same data model, at least with row level security. whatever is the most
efficient. each organization requires it's own database though. cross org
relationships would need setup.. and can work no matter where the app
servers are running." Relayed by Pia the same evening; shape and phasing by
Propolis. **Supersedes D38 and D39 for the server.** D38 §6 (the client
stays htmx) and D40 to D42 stand, with the engine-specific parts of D41
re-homed below. D24's Postgres row comes back; its sqlx pick does not.

## Why the reversal is not a flip-flop

D38 left Postgres for four things the note promised: a file per tenant,
embedded replicas, vectors in the tenant's own store, encryption per
database. Two days of building measured most of them away:

- **Replicas and encryption were libSQL features**, and libSQL was measured
  out on a heap-corruption bug at sixteen connections (D38 §1). Vanilla
  SQLite has neither; phase 3 was left choosing between three things none of
  which existed in the tree.
- **The owner file did not buy concurrency** (D39 §4): an attached file
  shares the control plane's write lock, so the daemon is one writer at a
  time however many owners there are, and a commit across two files is not
  atomic under WAL (D39 measurement 7). D39 wrote the torn shapes down as
  accepted.
- **The predicate left the database** (D38 §2): SQLite has no stored
  functions, so the access decision became a Rust-owned SQL text pinned by
  the binary rather than by the schema. That was a cost, recorded as one.

What survives from D38's goals is the one Nate restated: **a tenant is a
unit you can find, move, back up and delete on its own.** The unit becomes
the organization's database rather than an owner's file, and the shared
CNPG cluster (`data/data`: HA across both Pis, nightly dumps, WAL archiving
and PITR returning with brh-infra #297) does the replication, backup and
restore that the file layout was going to have to grow.

## The decision

### 1. One Postgres database per organization; a registry finds it

**An organization is the tenant.** The schema already has organizations: an
`actors` row of kind `org`, with `org_members` (D1.2, D19.2). Every
principal has exactly one **home organization**; a person who is in no
organization gets a personal one at enrolment (the household is one org;
a person's personal space is another). An org's database holds everything
that today lives in `hive.db`, the owner files and the audit file **for the
principals whose home it is**: installs, builds, entities, links, mentions,
grants, events, chat, runs, the blob catalogue, every collection table.

**The registry is a small Postgres database of its own**, the control plane
above the orgs. It holds what has to be answered before you know which org
database to open, and nothing else:

| table | holds | why it is here |
|---|---|---|
| `orgs` | id, slug, state (`provisioning` / `live` / `suspended` / `deleting`), schema version | an org exists before its database does |
| `org_stores` | org id, store generation, host, port, database name, role name, a secret reference (never a password) | the lookup; the generation is part of every pool key |
| `identities` | a human's global id and their home org | sign-in has to find the org before any org database is open |
| `credential_index` | credential digest → (org id, credential id) | a bearer token is resolved to an org in one indexed read; the credential itself, its scope and its revocation stay in the org's database, where the policy triggers can reach them |
| `org_links` | the two orgs, link id, state, each side's federation endpoint | §4 |

Everything an access decision depends on stays in the org's database, so
the predicate is answerable from one database with no network hop. The
registry is a phone book, never a policy store: if it is wrong, a request
lands at the wrong database, finds no credential matching the token there,
and is refused (invariant 1). That property is tested, not assumed.

**App servers find an org's database by lookup**, so an org's database can
be on the shared CNPG cluster, on a cluster of its own, or on a single box,
and the daemon does not care which. The secret reference is resolved
through the same seam the vault will use (#87); until that lands it names a
file or environment key, never a literal.

**Pools are keyed on (org id, store generation).** Invariant 14: a pool
keyed on the org alone survives a move of that org's database and keeps
writing to the old one. Moving an org bumps the generation, and every
daemon's next lookup drains the old pool. What the key omits: the
principal. That is safe because the connection carries no principal of its
own between transactions (§2).

### 2. Inside an org: the same data model, policy back in the database, row-level security

**The tables are D38's tables in Postgres types**: the D38 §3 table read
right to left. `uuid`, `timestamptz`, `jsonb`, `boolean`, identity columns,
`$n` placeholders, `FOR UPDATE SKIP LOCKED` for claims, `pg_advisory_xact_lock`
where SQLite used the file lock. The per-install table-name prefix goes
back to a **schema per install**, which is what D38 replaced with a prefix
for want of schemas. Events go back to append-only enforced by trigger and
by `REVOKE UPDATE, DELETE`.

**The predicate goes back into the database as functions**:
`access_decision`, `access_reason`, `tool_access_reason`,
`route_access_reason`, `visible_events`, installed by the migration and
pinned by its checksum again. `Guard` in `hive-store` stays the only Rust
caller (invariant 1 is about the host's enforcement point, and that does
not move). Functions are the efficient form Nate asked for: a set read
that composes the predicate is planned once by the server, rather than
shipped as a text the client pastes into every query.

**Row-level security is the second wall, not the first.** Every table that
holds a principal's data is `ENABLE` and `FORCE ROW LEVEL SECURITY`, with a
policy that calls `access_decision` on the row's subject for the
transaction's principal. The facts the policy reads come from
`SET LOCAL hive.actor`, `hive.principal` and `hive.install`, set by
`hive-db` from the `Credential` as the first statement of every
transaction, and from nowhere else: there is no API that sets them from a
value a caller supplies. Invariant 11 is the reason that matters ... the
policy must resolve its facts from identifiers, not accept them ... and the
reason it holds is that `Credential` is the only thing `begin` accepts.

The roles:

- `hive_owner_<org>` owns the schema; migrations run as it; nothing else
  does.
- `hive_app_<org>` is what the daemon connects as: no `BYPASSRLS`, not the
  owner, `INSERT` only on `events`, nothing on the policy functions'
  internals.
- A transaction with no principal set sees no rows (the policies compare
  against `current_setting('hive.principal', true)`, which is null, and
  null matches nothing). Absence of scope is deny, at the second wall too.

So a defect in a Rust read that forgets the predicate, or a hand-written
`psql` session as the app role, returns the caller's rows and nobody
else's. The Rust `Guard` check stays, because D38 measured defence in depth
as real (CLAUDE.md, *Check what the mutation removed*), and the store suite
tests each wall with the other removed.

**The bell is `NOTIFY` again**, carrying an id, from the commit, per org
database: one `LISTEN` connection per org pool. Invariant 4 is unchanged:
the events table is the transport and the backstop poll keeps every
consumer correct when every notification is dropped. With many writers
back, the late-commit hazard D38 §7 retired returns, and the overlap window
and the dedupe are what answer it, as they did before D38.

### 3. Vectors and full text (re-homing D41)

D41's manifest surface (`fts(path)`, `vector(path, dim)`, `storage.query`
`search` and `near`) is unchanged. Under Postgres, `fts` provisions a
generated `tsvector` column with a GIN index and `search` becomes
`websearch_to_tsquery` (still words, never syntax); `vector` provisions a
`pgvector` column with an HNSW index. This needs pgvector in the CNPG
image, which is an infra ask (below). The sqlite-vec `unsafe` stays, in
`hive-db`'s SQLite backend only.

### 4. Cross-org: a link on both sides, resolved over the network

Two organizations relate only through a **link**, set up on purpose from
both sides: an `org_links` row in the registry, and in each org's database
a `peer_orgs` row plus the grants that org chose to make to the peer.
A grant to another org's principal names a **remote principal**, a new
principal kind `(peer org, actor id)`, and the predicate treats it like
any other grantee. Nothing is granted by the link itself; the link only
makes grants to the other side expressible.

A read across orgs is a **request, never a join**. The daemon serving org A
asks the registry where B is served, and calls B's daemon's federation
route with a link credential (minted per link, pinned to `(link, A's
acting principal, A's acting actor)` per invariant 2). B's daemon resolves
it in **B's** database, runs **B's** predicate against the remote
principal, and answers. A never holds a connection to B's database and
never sees a row B's predicate did not release. This works with A and B on
one daemon, two daemons on one cluster, or two clusters: the code path is
the same, and the single-daemon case is just a loopback call, so it gets
tested on every build rather than only in a deployment that happens to be
split.

What comes back across a link is `untrusted` until B's side says
otherwise, per invariant 12: B's trust claim about its own rows is B's
claim, carried and labelled, not adopted.

### 5. SQLite stays behind `hive-db`, and what that costs

SQLite stays for a single-box install, the test loop, and offline clients.
D38's *What lost* refused a dual-backend store because it is "two
enforcement points answering one question". That is still the danger, and
this is how it is answered rather than waved away:

- **One enforcement point in Rust**: `Guard`, in `hive-store`, for both
  backends. The backends differ below it, not beside it.
- **The policy is two texts**: plpgsql functions and triggers for Postgres,
  the SQLite triggers and predicate text that exist today. That is the
  price, and it is held by **one test suite run against both**:
  `hive_testdb::TestDb` gains a backend parameter, every store and
  invariant suite runs twice in CI, and a rule that holds on one engine and
  not the other is a red gate, not a deployment surprise.
- **A single-box SQLite install is one org.** No registry, no federation
  peers until it is given a registry entry. The owner-file layer of D39
  goes: a single box has one tenant, and the reason D39 split files (a
  tenant as a unit) is now the org database's job.
- **Row-level security has no SQLite form.** The second wall exists only
  under Postgres; the SQLite backend has the first wall and the triggers,
  which is what it has today. A single-box install is one org, which is
  where the second wall matters least.

### 6. Provisioning an org is automated

`hive-sandbox org create <slug>` (and the same as an admin route): write
the `orgs` row as `provisioning`, create the database and the two roles,
run the migrations as the owner role, grant the app role, write the
`org_stores` row, flip to `live`. Every step is idempotent and the state
column says how far it got, so a crash mid-provision resumes rather than
leaking a half-made database. The daemon needs a **provisioning role** on
the cluster with `CREATEDB` and `CREATEROLE` and nothing else, used by this
path only and never by request handling; that role is an infra ask.

Deleting an org (D39's open question, now answerable): suspend it in the
registry, drain its pools, dump it, drop the database and roles, mark the
row deleted. A tenant is a database; deleting one is `DROP DATABASE`, and
the dump is the undo.

### 7. Driver

`tokio-postgres` with `deadpool-postgres`, rustls for TLS. Criteria, which
outlive the pick: async without a blocking thread per call (the engine is
across a network now); no compile-time query checking against a live
database, because the gate runs with nothing up and the SQL lives in
migrations and `hive-store` constants either way; prepared statements;
`LISTEN` on a connection of its own; no native library. sqlx is not
picked again: its macros wanted a database at build time, and the
`tokio::spawn` rule in `docs/development.md` came from its futures. The
rule stays regardless (CLAUDE.md, *Await a store or migration future on
the calling task*).

### 8. An org may bring its own Postgres

Nate, the same evening: "i want to allow self-hosted databases too." The
registry already makes location irrelevant; what a self-hosted database
changes is **who else holds the keys**, so this section is mostly about
the trust boundary.

- **Registering is a check, not a form.** `hive-sandbox org attach <slug>
  --dsn-secret <ref>` connects and refuses with the specific reason if any
  of these fails: the Postgres major-version floor (16, for `SET LOCAL`
  semantics and the RLS features §2 uses; the number lives in code and
  this record names the criterion), the extensions D41 needs when the org
  declares vector or full-text collections (`vector`), a TLS connection
  verified in full against the CA the org supplies, and the privileges:
  the migration role can `CREATE` in the database and create functions
  and triggers; the app role is not the owner, has no `BYPASSRLS`, and is
  not a superuser. An app role that could bypass the second wall is
  refused, because the second wall is what that role exists to sit
  behind. Superuser is never required of either role.
- **Two roles, two secrets, both references.** The registry stores a
  reference to each secret, never a value; until the vault (#87) lands,
  the reference resolves to a file the daemon reads and the value is
  encrypted at rest under the daemon's boot key where it has to be stored
  at all. Rotation is a new secret reference plus a generation bump (§1),
  so the old pool drains instead of failing in place.
- **Migrations run from here**, as the migration role, on upgrade. The
  registry's `orgs.schema_version` is the per-org record, and an org can
  be **held** (`orgs.migrate_hold`, with a reason): a held org keeps being
  served at its version as long as the binary still supports that version,
  and the daemon refuses to start a binary whose minimum supported version
  is above a held org's, rather than serving it wrongly.
- **Connectivity is per org and visible.** TLS `verify-full` always,
  connect and statement timeouts, the pool keyed as in §1, and a health
  state per org database (`ok` / `degraded` / `unreachable` /
  `refused`, with the last reason) in the registry, surfaced to that
  org's admins and the operator. An unreachable org is that org's outage
  and nobody else's: no request for another org waits on it.
- **The trust boundary.** A self-hosted org's administrator has superuser
  on their own database, so the triggers and both walls are advisory
  **for their own org's data**. That is accepted: it is their data. What
  is not accepted is anyone else relying on that database's word. So:
  - A credential is resolved to its org by the registry's
    `credential_index` before any org database is read. A forged
    credential row in a self-hosted database can authenticate only into
    that database.
  - A link exists when the **registry** says so, and its state lives
    there. A `peer_orgs` row in an org's database is that org's
    bookkeeping, never evidence.
  - The link credential that carries A's acting principal to B is minted
    and signed by the registry's key for (link, principal, actor), and B
    verifies the signature, not A's database's opinion. B's answer comes
    back `untrusted` to A (§4), so a self-hosted B that lies about its own
    rows can mislead only a reader who chose to link to it, and is marked
    as untrusted when it does.
  - No hosted org's data is ever written to, or read from, a self-hosted
    database. Everything that crosses goes through the federation route,
    as §4 already requires.
- **Backups are the owner's.** A hosted org is in CNPG's backups and its
  PITR (brh-infra #297). A self-hosted org owns its own; the registry
  records which is which so nobody assumes otherwise. Registering a
  reachable self-hosted database as an extra backup source is possible
  later and is not in this record.
- **Moving in or out** is a dump and restore under a suspended org: suspend
  in the registry, drain pools, `pg_dump` with the schema version
  recorded, restore at the target, run the requirements check, write the
  new `org_stores` row with the next generation, resume. Logical
  replication for a move without downtime is a later option; the
  generation key is what makes either one safe.

## Phases

Each is a PR, in this order, and each leaves the gate green on both
backends.

1. **`hive-db` grows a Postgres backend; `hive-schema` grows a Postgres
   migration set; `TestDb` runs both.** One database, no orgs yet: the
   existing suites pass against Postgres. The test database is the podman
   `hive-mind-pg-rust` container locally and a service container in CI;
   the database test tier needs a backend again, so the named-skip rule
   and `HIVE_SANDBOX_REQUIRE_*` come back for it.
2. **The registry and the per-org pool.** `org_stores`, the generation-keyed
   pools, `credential_index`, provisioning; a daemon serves several orgs.
3. **Row-level security**, with a mutation test per wall.
4. **Links and federation**, with the loopback test above and the
   registry-signed link credential (§8).
5. **Self-hosted orgs**: `org attach`, the requirements check, migration
   holds, per-org health.
6. **D39's owner files go** from the SQLite backend, once nothing reads
   through them.

## What lost

- *One database with RLS, org id on every row.* The most efficient on one
  cluster, and it fails "each organization requires its own database": an
  org cannot be moved, restored or deleted on its own, and one missing
  `org_id` predicate is a cross-org leak.
- *A schema per org in one database.* Closer, but a PITR restore is still
  the whole cluster's database, and an org cannot live on a different host.
- *Cross-org via `postgres_fdw` or `dblink`.* A join across databases is
  exactly what "work no matter where the app servers are running" rules
  out, and it would put one org's credentials inside another org's
  database.
- *The registry inside an org's database.* Sign-in would need the database
  it is trying to find.
- *Dropping SQLite.* The test loop, the single box and the offline client
  all want it, and the price (§5) is a suite run twice.

## Infra (Pia's side; state as of 2026-10-04)

- **Done.** `data/data` runs Postgres 17 with `vector` available.
  pgvector is pre-installed in `template1`, because it is not a trusted
  extension and an org role cannot create it. Every database the
  provisioner creates inherits it.
- **Done.** The registry database is `hive_registry`, owned by the role
  `hive_registry` (LOGIN only, PUBLIC connect revoked). The provisioning
  role is `hive_provisioner` (LOGIN, `CREATEDB`, `CREATEROLE`, nothing
  else). Since Postgres 16, `CREATEROLE` grants admin only over roles that
  role itself created, so the provisioner cannot reach any other
  application's roles: the least privilege §6 asks for is the engine's
  default, not a convention. Both credentials are in 1Password and are
  read at run time, never written down.
- **Later.** A network policy and sealed secrets for the daemon's
  namespace, when the daemon is deployed on the cluster. Today the host is
  in-cluster only, so a daemon running off the cluster needs an exposed
  route first.

## Open

- Whether an AI actor's home org is always its principal's (assumed yes).
- Rate and size limits on the federation route.
- When the registry itself needs to be replicated beyond CNPG's HA.
- Moving an org between clusters live (the generation key makes it safe;
  nothing does it yet).

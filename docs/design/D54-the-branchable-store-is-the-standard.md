# D54: the branchable store is the platform's standard storage: a Neon project per organization, provisioned at sign-up; branch, reset and restore are storage capabilities a pool is keyed on; plain Postgres stays as the fallback that reports them absent and degrades loudly; a self-hosted engine on the cluster is a later record

**Decided** 2026-10-14 by Nate: "what if we use neon postgres for hive
mind? branching seems like it would be very effective here when building
apps and such etc.. probably other features of branching we can use here."
Then, after the costs were put to him: "but the branching data makes sense
here and plain postgres doesn't have that. what do you think in the end
here?" And: "let's do it prop. sounds like a sound design." Shape by
Propolis, who first argued for Neon as one driver among several and moved
to this record's position once the question was framed as a platform
capability rather than a database choice; the reasoning is §*Why standard
and not driver*. Builds on D43 (a database per org behind a registry,
provisioning automated, bring-your-own Postgres, the pool key), D52 (the
org-owned schema with `schema.plan` and `schema.apply`; one storage
interface; drivers bound per `(org, primitive, generation)`), D53 (`hive
dev` and apps as processes), D51 (sealed and served contexts), D48 (the
vault holds the engine's credential), D45 and D49 (sync and projections
serve one timeline). **Amends D43 §1, §6 and §7** (where a hosted org's
database lives and how it is provisioned; the pool key gains a dimension)
and **D52 §1 and §3** (`schema.plan` rehearses on a branch; the engine
behind the default table and document drivers). D43's infra section
(CloudNativePG on the cluster) stands as the fallback engine, not the
default.

## What was verified before deciding, so the record rests on facts

Checked on 2026-10-14 against Neon's own documentation and, where it said
nothing, against the repository; third-party sources are named as such.

- **Compute and storage are separate; writes do not scale out.** One
  Postgres primary per branch, scaled vertically and to zero; read
  replicas; storage (pageservers over an object store) scales and shards.
  No distributed transactions, no sharded tables. This is not YugabyteDB,
  and §*What lost* says why that is the right trade for a platform whose
  growth is more organizations rather than a bigger table.
- **A branch is a copy-on-write fork of schema and data, instant, and
  one-way.** A child can be *reset from parent*; nothing merges a child's
  data back. (Neon's branch documentation, read through a third-party
  mirror; the direction was confirmed, the exact API not.)
- **Restore is a branch operation**: a point-in-time restore of a branch
  replaces its data from its own history and keeps the prior state as a
  named branch for the retention window.
- **Scale-to-zero drops session state.** Temp tables, prepared statements,
  advisory locks and `LISTEN` subscriptions do not survive a compute
  stopping; unlogged tables lose their rows. Neon's compatibility page says
  so, and Databricks's Lakebase page (the same engine) says the same.
- **No superuser.** Functionality needing superuser or the local filesystem
  is refused. D43 §8 already requires neither role to be superuser.
- **Extensions are a curated list**, and it includes `vector`, `pg_trgm`,
  `pgcrypto` and `pg_stat_statements` (D41 needs `vector`).
- **Failure and recovery, from Neon's HA page:** a lost safekeeper is
  invisible (quorum of three); a pageserver fails over to a secondary in
  seconds; a Postgres crash restarts in seconds; a lost node reschedules in
  one to two minutes; an unresponsive endpoint is detected and reattached
  after five minutes; an availability-zone outage is one to ten minutes.
  There is no hot standby primary. Clients must reconnect.
- **Self-hosting:** the storage engine (pageserver, safekeepers, storage
  broker, compute) is Apache-2.0 in `neondatabase/neon`. The control plane
  (create project, create branch, start a compute, hand back a connection
  string) is not published; the repository's compose files say they are
  for testing images and not for deploying a usable system; the official
  `neon_local` image is a proxy to a cloud project and needs an API key.
  The pageserver has a local-filesystem remote-storage backend, described
  in Neon's docs as "mainly for tests". Multi-architecture images for
  arm64 were not verified.
- **Lakebase is Neon through Databricks**: the same engine, the same gaps,
  plus the lakehouse side (synced tables, Unity Catalog). The driver below
  is indifferent to which storefront a connection string came from.
- **What this repository relies on that the engine constrains:** the bell
  is `NOTIFY` on one `LISTEN` connection per org pool (D43 §2) and the
  invariant already says the events table is the transport and the
  backstop poll keeps every consumer correct when every ring is dropped
  (CLAUDE.md invariant 4); migrations take `pg_advisory_xact_lock`, which
  is transaction-scoped (`crates/hive-schema/src/lib.rs`); no unlogged
  tables, publications or event triggers anywhere in `crates/`
  (`rg` on 2026-10-14).

## Why standard and not driver

The first answer given was "Neon as a D52 driver, plain Postgres the
default". It was wrong for a reason D52 §3 states about drivers: a driver
stores bytes and never changes the shape of what the platform can do. A
branch does change it. Three behaviours the platform is designed around
are only honest with a branch underneath:

1. **`schema.plan` is a rehearsal, not a reading.** D52 §1 has the planner
   return "the migration the daemon would run, plus what it would do to
   existing rows". Without a branch that is static analysis of the SQL,
   which cannot see a `NOT NULL` that fails on row 40,000 or an index that
   takes an hour. With a branch the planner runs the migration on a
   copy of the org's actual data and reports what happened.
2. **An AI builds against real rows and cannot hurt them.** D53's `hive
   dev` and the builder loop (#27) want an app's handlers running against
   data shaped like the org's. A branch per app or per PR gives that with
   the main timeline untouchable from the branch's credential. Plain
   Postgres offers a full copy, which for an org of any size is not a
   thing you do per PR.
3. **Every org has a rewind.** Point-in-time restore per org is the undo
   for mistakes this design has not anticipated, including a bad D51
   rotation. Under D43's CNPG the restore unit is the cluster's PITR, and
   per-org restore is a dump and a hope.

If those three are features of the platform, then an install without them
is a degraded install and must say so, which is what a fallback is. If they
were features of one driver, every surface would have to be written twice,
once assuming a branch and once not, and the second path is the one nobody
tests. So: the branchable store is the standard; the fallback reports the
capability absent; every surface that wants a branch asks the engine and
refuses with the reason when there is none.

## The decision

### 1. A Neon project per organization, provisioned at sign-up

- **One Neon project per org**, created by D43 §6's `org create` as its
  first step through the Neon API: the project, its default branch
  (`main`, the org's timeline), the two roles D43 §2 names (`hive_owner_<org>`,
  `hive_app_<org>`), then the migrations as the owner, then the
  `org_stores` row with host, port, database, role and a **secret
  reference** into the vault (D48) for each role's password, then `live`.
  Idempotent on retry, by the project name being the org id and the
  registry's state column recording how far it got (D43 §6's rule, kept).
  The daemon's Neon API key is itself a vault entry leased to the
  provisioning path and nothing else; request handling never holds it.
- **The registry is a Neon project too**, with scale-to-zero **off**,
  because sign-in resolves a credential against it before any org
  database is open and a cold start there is everyone's cold start. One
  storage behaviour to build against from D43 phase 1 slice 3 onward is
  the point; an exception for the registry would be the first of many.
- **The Roadhouse's own install goes to Neon as well**, by Nate's choice
  and against the latency concern below; the self-hosted engine (§6) is
  the path that brings it home later. The CNPG cluster D43's infra section
  describes stays as the fallback engine and as what a D43 §8 attached
  org most likely brings.
- **Direct endpoint, not Neon's pooler.** The app pool connects to the
  branch's direct endpoint. Criterion: the pool relies on per-connection
  facts the daemon sets and reads (`SET LOCAL hive.*` as the first
  statement of each transaction, `LISTEN` on a dedicated connection), and
  a transaction-mode pooler in front would make the second impossible and
  the first something to re-verify on every upgrade. The daemon already
  pools (D43 §7); a second pooler would be a key with a missing dimension
  waiting to happen (invariant 14).
- **Connect and statement timeouts cover a cold start**, measured and set
  in code with the measurement beside them, not guessed. A first request
  after idle waits; it does not fail.
- **Neon direct, not Lakebase**, assumed until Nate says otherwise.
  Criterion: one console and one bill for the engine the platform uses,
  with the lakehouse side bought only if something else needs it. The
  driver does not care, so the choice is reversible by changing a
  connection string and an API endpoint.

### 2. Branch, reset and restore are storage capabilities, and the pool is keyed on the branch

New verbs on the D52 interface, at the level of a **context** (an org, or
a person's personal org), not per primitive, because a branch forks the
whole database:

| verb | what it does | who |
|---|---|---|
| `storage.branch.create {from, purpose, ttl}` | forks the timeline named by `from` (default `main`) into a new branch, recorded in `org_branches` with its purpose, creator, expiry and the generation it was cut at | a `branch` grant, verb `create` |
| `storage.branch.list` | the org's branches with purpose, age, size, expiry | `branch` grant, verb `list` |
| `storage.branch.reset {branch}` | discards the branch's changes and refreshes it from its parent | `branch` grant, verb `reset` |
| `storage.branch.delete {branch}` | ends it; the pool for it drains | `branch` grant, verb `delete` |
| `storage.restore {to}` | point-in-time restore of `main` to `to`, keeping the prior state as a branch named for the restore, for the retention window | `branch` grant, verb `restore`, step-up required (D50 §4) |

- **Purposes are an enumeration**: `plan` (§3), `dev` (§4), `run` (an
  agent run that asked for one), `restore` (what §2's restore leaves
  behind), `adhoc` (an admin's). Every branch has a **TTL**; the default
  per purpose is short (minutes for `plan`, hours for `dev` and `run`,
  the retention window for `restore`), and a sweeper deletes expired
  branches and emits the event. A branch is never promoted: the engine
  cannot merge one and the design does not pretend to. What comes back
  from a branch is a migration text, a diff, a test result, or code.
- **The pool key gains the branch.** D43 §1 keys pools on `(org, store
  generation)`. A connection to a branch is a connection to a different
  database with the same roles and the same schema, so a pool keyed
  without the branch would hand a rehearsal's connection to a request for
  `main`, and the first such write would land where it must not. The key
  is now `(org, store generation, branch)` with `main` the branch of every
  ordinary request; what it still omits, the principal, is safe for D43
  §1's reason (the connection carries no principal between transactions).
  The credential a branch's caller holds is **pinned to the branch**: a
  `hive dev` session or an agent run that asked for a branch gets an
  install credential whose scope names it, and `Guard` refuses that
  credential on `main` with the message naming the branch. Invariant 14's
  question, asked in advance: a request with no branch in its credential
  goes to `main`, always; there is no "whichever branch came back first".
- **A branch is not a tenant and not a context.** The D45 sync API and the
  D49 projections serve `main` only; a device never receives a branch's
  row and a peer never projects one. A branch inherits the org's D51 key
  version and its sealed ciphertext exactly as cut; a sealed context stays
  sealed on every branch, and an unlock is scoped to one branch as it is
  to one purpose.
- **Events.** Creating, resetting, restoring and deleting a branch are
  events on `main`'s events table, with the purpose, the actor and the
  principal (invariant 2), so "which agent cut a branch of my data, and
  when did it go" is answerable from the one log.

### 3. `schema.plan` rehearses on a branch

D52 §1's planner becomes: cut a `plan` branch from `main`, run the proposed
migration there as the owner role, read back the catalogue (tables,
columns, constraints, indexes, RLS policies) and the row effects (counts
touched, the first N failures if any, wall time), delete the branch, and
return all of it as the plan. `schema.apply` runs the same text on `main`.
A test asserts that the catalogue after apply equals the catalogue the
plan reported (scenario 3), which is the property that makes the plan
worth reading.

On an engine without branches the planner still answers, with the static
analysis D52 described and a line at the top: `plan is static on this
engine: no rehearsal branch is available`. The storage page per context
shows which it was.

### 4. `hive dev` and agent runs get a branch

- `hive dev ./my-app --branch` (D53 §2) cuts a `dev` branch of the
  developer's org and runs the app against it with a credential pinned to
  the branch; `--branch` without an engine that can is refused with the
  reason. The default without the flag is unchanged: the local daemon's
  own data.
- An **agent run** (D35, D27) may declare `storage: branch` in its
  definition; the harness cuts a `run` branch, pins the run's credential
  to it, and deletes the branch when the run ends unless the run asked to
  keep it for review (then its TTL applies). The builder loop (#27) is the
  first user: the agent's proposed schema change and app code are
  exercised on a branch, and what the human reviews is a diff and a plan,
  never a live write.

### 5. The fallback is plain Postgres, and SQLite, reporting the capability absent

- `hive-db`'s `Engine` gains **capabilities** (`branch`, `restore`,
  `scale_to_zero`), answered from the connection (the Neon engine is
  Postgres with a control-plane client beside it; a plain Postgres has
  none). Nothing above `hive-db` tests for a vendor; everything asks for a
  capability.
- Every surface that uses a branch asks first and, absent one, does the
  degraded thing **and says so** in its response and on the storage page:
  the planner's static line (§3), `hive dev --branch` refused, an agent
  definition with `storage: branch` refused at install with the message.
  Silent degradation is the bug this section exists to prevent.
- **The suite runs against both.** D43 §5 already runs every store suite
  on SQLite and Postgres. CI gains a `neon` job against a project the key
  for which is a repository secret; it is a **required** job with
  `HIVE_SANDBOX_REQUIRE_DATABASE_TESTS=1`, so a missing secret or an
  unreachable project is a red gate and never a skipped one (CLAUDE.md,
  *Check the skip*). The plain-Postgres job asserts the capabilities
  report absent and the refusals carry their messages.
- **An attached org (D43 §8)** on plain Postgres is this fallback with the
  org's own keys; the requirements check gains "branches: no" as a fact it
  records, not a failure.

### 6. The engine forces three behaviours, written down here so nobody rediscovers them

- **Reconnect and re-`LISTEN`.** When a compute stops (scale-to-zero,
  restart, failover) every connection in the branch's pool is dead. The
  pool detects it on the next checkout and rebuilds; the bell connection
  is re-established and `LISTEN` re-issued; the backstop poll delivered
  every row meanwhile (invariant 4). Scenario 4 kills the compute and
  asserts both the rows and the ring.
- **No superuser, ever.** D43 §8's rule becomes the hosted default too: the
  provisioner creates the two roles with what the engine allows and the
  migrations are written to need nothing more. A migration that needs
  superuser is a bug in the migration.
- **Served mode puts plaintext in the engine's region.** D51's served mode
  gives the server a working copy of the context key, which means indexed
  content columns are plaintext in Postgres, which under this record is in
  the engine's cloud region. Sealed contexts are ciphertext to the engine
  as D51 intends. The served-mode consent screen names where the engine
  runs, so the org chooses with that fact in front of it.

### 7. The self-hosted engine is a later record

Nate asked, in order: could we self-host Neon, could the durable layer be
a filesystem rather than S3, could two Orange Pis replicate to each other.
The answers, recorded so the later record starts from them:

- **Yes, the engine is open source; the control plane is ours to write.**
  Starting a compute for a branch, handing back its connection string and
  moving it when a node dies is work this platform would own. Much of it
  overlaps D43 §6's provisioning, which is why it is plausible rather than
  absurd.
- **A filesystem backend exists and pins the stack to one pageserver**
  (a Longhorn volume mounts read-write on one node); durability is then
  Longhorn's replication, which is what everything else on the cluster
  already rests on. Garage on both Pis removes the single-pageserver limit
  and is the better first configuration if two Pis are the target.
- **Neon's parts do not replicate to each other.** Durability is the
  safekeeper quorum (three voters to survive one loss) plus the object
  store; pageservers are caches over it. The smallest layout that survives
  a Pi going down is Garage on both Pis, three safekeepers (one per Pi, one
  on a Talos VM), one pageserver with a secondary, computes scheduled by
  the cluster. Failover is then whatever our control plane does; today
  Kubernetes alone takes minutes to give up on a dead node's pod.
- **Not before the driver has a conformance suite** to prove the
  self-hosted engine against, and not before the Pis are in the cluster
  (brh-infra #73). The record that does it is Pia's and mine together: the
  layout and the nodes are hers, the storage behaviour is this crate's.

## Acceptance scenarios, checked in before the code

1. **Sign-up provisions a project.** Enrolling a person leaves a live
   personal org whose `org_stores` row points at a Neon project named for
   the org id, with both roles, the migrations applied, and the passwords
   in the vault as references; a retry after a crash between steps resumes
   and provisions nothing twice (D43 §6, now with the project step).
2. **A branch never reaches `main`.** A `tables.insert` through a
   credential pinned to a `dev` branch is visible on the branch and absent
   on `main`; the same credential's `tables.select` against `main` is
   refused with the message naming the branch; a credential with no branch
   reads `main`. Asserted by reading both databases, not by trusting the
   pool.
3. **The plan is what apply does.** `schema.plan` for a migration adding a
   `NOT NULL` column with a default reports the catalogue and the row
   count; `schema.apply` on `main` yields the identical catalogue and
   touches the same count; the `plan` branch is gone afterwards (asserted
   from the engine's branch list, not the registry's). On plain Postgres
   the same plan carries the static line and apply still works.
4. **The bell survives a dead compute.** With the pool open and the tailer
   running, the branch's compute is stopped through the API; a write after
   it wakes is delivered by the backstop poll within its window and the
   bell rings again for the write after that; no row is lost or
   duplicated (invariant 4's overlap-and-dedupe, exercised against a real
   disconnect rather than a mock).
5. **Expiry deletes.** A `plan` branch past its TTL is deleted by the
   sweeper and its deletion is an event with the purpose and creator; a
   branch whose TTL was extended is not.
6. **Restore keeps the prior state.** After a destructive `tables.delete`
   on `main`, `storage.restore {to}` to a moment before it returns the
   rows, leaves a `restore` branch holding the deleted state, and requires
   step-up (D50 §4) to run; the pool for `main` was drained and rebuilt
   across the restore (asserted by the pool's generation, not timing).
7. **Sealed stays sealed on a branch.** In a sealed context (D51), a
   credential pinned to a branch cannot read content fields without an
   unlock, and an unlock scoped to `main` is refused on the branch with the
   message naming the scope.
8. **Sync and projections serve `main` only.** A device's sync cursor
   (D45) after writes on a branch receives nothing from the branch; a peer's
   projection (D49) likewise. One test runs both.
9. **The capability report is honest.** On the plain-Postgres CI job,
   `Engine::capabilities()` reports `branch: false`; `hive dev --branch`
   and an agent definition with `storage: branch` are refused with their
   messages; on the `neon` job the same calls succeed. Both jobs are
   required and set `HIVE_SANDBOX_REQUIRE_DATABASE_TESTS=1`.

## Order

1. **D43 phase 1 slice 3, `hive-store` on Postgres, against a Neon
   project**: the suite's Postgres tier gets the `neon` CI job beside the
   service container; reconnect and re-`LISTEN` land here because the
   first scale-to-zero in CI will demand them (scenario 4). The pool key
   gains the branch dimension with only `main` in use, so the dimension
   exists before anything depends on it.
2. **Provisioning through the Neon API** under D43 phase 2 (the registry):
   project per org, roles, the vault references, the registry as its own
   always-on project. Scenario 1.
3. **The branch verbs and the pinned credential.** `org_branches`, the
   grant kind, the sweeper, the events. Scenarios 2, 5, 7, 8.
4. **`schema.plan` on a branch**, replacing D52 order 3's static planner
   where the engine allows. Scenario 3, 9.
5. **`hive dev --branch`** (D53 order 3) and `storage: branch` for agent
   runs. Scenario 9's positive half.
6. **Restore.** Scenario 6.
7. **The self-hosted engine**, its own record, after the above and after
   brh-infra #73.

## What lost

- *Neon as one driver among several, plain Postgres the default.* The
  first answer, withdrawn: §*Why standard and not driver*. A capability the
  planner and the dev loop are designed around is a platform fact, and a
  fallback that degrades loudly is honest where an optional driver would be
  a second code path nobody runs.
- *Plain Postgres on CNPG as the standard* (D43's infra as it stood). It
  has a hot standby and failover in seconds, which Neon lacks, and it has
  no branch, which this platform now assumes. Failover in minutes is a
  cost this platform can carry; a planner that guesses is not.
- *YugabyteDB or another distributed SQL engine.* Horizontal write scaling
  at the price of a reimplemented storage and transaction layer, slower
  single-node latency and a longer compatibility list, for a platform
  whose unit is one small database per org that mostly sleeps. If an org
  ever outgrows one writer, D49's answer is projections, not a distributed
  transaction.
- *Lakebase.* The same engine through a second console and bill; chosen
  against unless the lakehouse earns its place for something else.
  Reversible by a connection string.
- *Self-hosting the engine now.* The control plane is the work, the Pis are
  not in the cluster, and nothing exists yet to prove a self-hosted engine
  against. Named as the later record (§7) so the door stays open.
- *Neon's connection pooler.* Transaction-mode pooling breaks `LISTEN` and
  makes `SET LOCAL` a thing to re-verify; the daemon pools already.
- *Promoting a branch to `main`.* The engine cannot merge data and the
  design does not fake it; what a branch yields is text, diffs and code.
- *The registry on scale-to-zero.* Sign-in would pay every cold start.

## Open

- **Numbers, before slice 3 is called done**: round-trip latency from the
  cluster to the chosen region for one `Guard` check and one typical
  request; cold-start wait after idle; connect and statement timeouts set
  from them (D31's rule: measured, with the measurement in the comment).
- **Plan tier and project caps.** One project per org meets a per-plan
  project limit; the tier is chosen when the org count is known, and the
  limit is surfaced in the registry's health so a sign-up that would
  exceed it fails with the reason rather than with the engine's.
- **Branch quotas**: how many live branches per org, how much storage a
  branch may add, who sets the TTL defaults. Assumed small and per-org.
- **Where the public hive's daemon runs.** If its database is in a cloud
  region, the daemon should be near it; that is a hosting decision on
  Pia's side and is not made here.
- **arm64 images** for the self-hosted engine; unverified.
- **Cost per sleeping org**, measured once a few exist.

## Infra (Nate's and Pia's side)

- **Nate**: create the Neon organization, and put an API key for it in
  1Password; the daemon reads it through the vault seam and never writes
  it down. Say Neon direct or Lakebase; this record assumes direct.
- **Pia**: the key reaches the daemon's secret path on the cluster the
  same way the CNPG credentials do; CNPG stays as the fallback engine and
  nothing about it changes; the self-hosted engine waits on #73.

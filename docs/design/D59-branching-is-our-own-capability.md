# D59: branching is the platform's own capability on the cluster's Postgres: a `clone` backing by `CREATE DATABASE ... TEMPLATE` for small plan branches, a `snapshot` backing by CloudNativePG volume snapshots bootstrapped into a branch cluster for the rest; the same D54 verbs, purposes, TTLs and pool key; reported per org with its backing and absent when neither is configured

**Decided** 2026-10-14 by Nate: "let's just stick with postgres for now and
figure out how to do our own branching?" D56 records the first half (the
household's store is the cluster's CNPG, not hosted Neon). This record is
the second half. Options and costs laid out by Forage, the CNPG and
Longhorn facts by Pia, shape by Propolis. Builds on D54 (the branch
capability: `storage.branch.create`, `list`, `reset`, `delete`,
`storage.restore`; purposes `plan`, `dev`, `run`, `restore`, `adhoc`;
TTLs and the sweeper; the pool keyed on `(org, store generation, branch)`;
a branch credential refused on `main`; sync and projections serving
`main` only; the capability reported absent on an engine without it), D43
(a database per org on a cluster that may be shared or the org's own;
`org_stores` with a generation; the move procedure in §8), D52 (`schema.
plan` rehearsing on a branch) and D53 (`hive dev --branch`, agent runs
with `storage: branch`). **Amends nothing in D54's interface**; it adds
two backings behind it.

## What was checked before deciding

- **`CREATE DATABASE ... TEMPLATE <org db>`** copies a whole database.
  The official documentation's limitation, kept here because it shapes
  the backing: no other session may be connected to the template while
  it is copied, so the org's pool is drained for the copy's duration.
  `STRATEGY WAL_LOG` (the default since 15) copies block by block through
  the WAL and suits small databases; `STRATEGY FILE_COPY` copies at the
  filesystem level with a checkpoint before and after and suits large
  ones at the cost of two checkpoints the whole cluster pays. Neither is
  copy-on-write: the copy costs the database's size in disk and time.
- **CloudNativePG volume snapshots** (`backup.volumeSnapshot` on the
  Cluster; a `Backup` with `method: volumeSnapshot`) take a CSI
  VolumeSnapshot of the instance's volumes; a new Cluster bootstraps from
  a consistent set of them with `bootstrap.recovery.volumeSnapshots`
  (and `walStorage` if WAL is on its own volume). CNPG's own documentation
  puts the storage layer's correctness on the CSI driver and says a cold
  snapshot of the primary fences it for the snapshot's duration; the
  online path uses backup start and stop around the snapshot. Longhorn
  has a VolumeSnapshotClass on the cluster already (autoroll uses it, per
  Pia). The snapshot is near-instant; the branch cluster's start is not
  (an instance has to come up and replay).
- **A volume snapshot is of the instance, not of one database.** The
  household's CNPG serves every org's database from one instance (D43's
  infra section: `data/data`), so a branch cluster bootstrapped from its
  snapshot holds every org's bytes.
- **Neon's engine self-hosted** is the third option and is D54 §7's
  later record; not re-argued here.

## The decision

### 1. Two backings behind D54's verbs, chosen per branch

| backing | how | for | cost |
|---|---|---|---|
| `clone` | drain the org's pool, `CREATE DATABASE <org>_<branch> TEMPLATE <org>` in the same instance, resume; the branch's `org_stores` row points at the new database with the same roles | `plan` branches on orgs under a size threshold; `adhoc` when an admin asks for it | the database's size in disk and seconds; the org's writers wait for the copy |
| `snapshot` | a CNPG `Backup` with `method: volumeSnapshot` (online), then a branch `Cluster` bootstrapped from it with one instance, no replicas, no backups of its own, a TTL label; the branch's `org_stores` row points at the branch cluster's service with the same roles | `dev`, `run`, `restore`; `plan` on orgs above the threshold | one Postgres instance per live branch, its memory and CPU on the cluster; the branch's start time |

- **The daemon picks the backing** from the purpose, the org's size (from
  `pg_database_size`, cached) and a per-org setting; the choice and its
  reason are on the branch's `org_branches` row and in the event. A
  caller may ask for one by name and is refused with the reason if it
  is not available.
- **The pool key is unchanged** (`org`, generation, branch). A `clone`
  branch is a different database on the same host; a `snapshot` branch is
  a different host. Both are different `org_stores` rows, so the pool
  cannot confuse them with `main` (invariant 14's reason in D54 §2).
- **Reset** is delete and create again from the current parent, for both
  backings. **Restore** is the `snapshot` path from a CNPG point-in-time
  recovery (WAL archiving and PITR per brh-infra #297): a branch cluster
  recovered to the timestamp, the org's database dumped from it and
  swapped into `main` under a generation bump (D43 §8's move, with the
  pre-restore state kept as a `restore` branch for the retention
  window). Whole-cluster PITR is Pia's; per-org restore is this.
- **The sweeper** deletes expired branches: `DROP DATABASE` for `clone`,
  deleting the branch `Cluster` (and its PVCs) for `snapshot`. Both
  idempotent, both events.

### 2. What a `snapshot` branch holds, said plainly

A branch cluster bootstrapped from the shared instance's snapshot holds
**every org's database**, not only the branching org's. What keeps that
from being a leak:

- the branch's `org_stores` row names only the branching org's database
  and its roles; the daemon never opens another database on it, and the
  only credentials that reach a branch are pinned to it (D54 §2);
- the branch cluster's superuser is CNPG's, held in a secret the daemon
  never reads;
- sealed contexts (D51) are ciphertext on the snapshot as on `main`.

What it does not prevent: an operator with cluster access reading
another org's bytes off a branch volume, which they can already read off
`main`'s. So the honest statement is that a `snapshot` branch has the
same trust boundary as the shared instance, no wider, and **an org that
wants its branches to hold only its own data runs on a CNPG cluster of
its own**, which D43 §1 already allows ("on a cluster of its own") and
which the capability report names as `snapshot: shared-instance` versus
`snapshot: own-cluster`.

### 3. The seam between the daemon and the cluster

- **The daemon's half** (this repository): the verbs, the backing choice,
  the `clone` SQL, the branch rows and events, the sweeper, the pool key,
  and a small client for the cluster side that creates and deletes CNPG
  `Backup` and `Cluster` objects through the Kubernetes API with a
  service account scoped to one namespace and those two kinds. Outside a
  cluster (the single box, the test suite's plain Postgres) the
  `snapshot` backing is reported absent and `clone` is reported present.
- **The cluster's half** (brh-infra, Pia): the VolumeSnapshotClass,
  `backup.volumeSnapshot` on the household's Cluster, the namespace and
  the service account with its RBAC, the branch `Cluster` template the
  daemon fills (one instance, resource caps, a TTL label the cluster can
  garbage-collect on if the daemon dies), and the resource budget for
  live branches. The record names the template's fields; the manifest
  itself lives in `cluster/tofu`.
- **The test for the seam**: the daemon's suite runs the `snapshot` path
  against a kind or k3s cluster in CI with a fake CSI snapshotter only
  if such a job can be made honest; otherwise the `snapshot` backing is
  tested against the real cluster by a checked-in script Pia and I run
  together and whose output is pasted into the PR, and CI covers `clone`
  and the capability report. The PR that lands `snapshot` says which.

### 4. What stays exactly D54, and the one thing D60 changes

The verbs and their shapes, the purposes and default TTLs, the pool key,
the branch credential refused on `main`, `schema.plan` on a `plan` branch
with the static fallback, `hive dev --branch`, `storage: branch` for agent
runs, sync and projections serving `main` only, sealed contexts staying
sealed, and the rule that every surface asks for the capability and
degrades loudly without it. What D54 §2 said about a branch never being
promoted is **amended by D60**: a branch merges back to `main` by
replaying its operation log through the same `Guard`, and the **fork
watermark** D60 needs is recorded here, on the branch row at creation:
for `clone`, the last operation id at the pool drain; for `snapshot`,
the highest operation id in the snapshot's own log, which is correct by
construction because the snapshot carries the log (Pia's point). A Neon-backed org (an attached project, D43
§8, or a hosted hive) reports `branch: neon` and uses D54's path; the
household reports `branch: clone, snapshot: shared-instance`.

## Acceptance scenarios, checked in before the code

1. **`clone` makes a branch the pool cannot confuse.** On the CI Postgres,
   `storage.branch.create {purpose: plan}` on a small org drains the pool,
   creates `<org>_<branch>` by template, and resumes; a write on the
   branch credential is absent on `main` and a `main` write is absent on
   the branch (D54 scenario 2, re-run on this backing); the org's writers
   observed a wait, not an error, during the copy (asserted by a writer
   running throughout).
2. **The size threshold routes.** An org over the threshold asking for a
   `plan` branch gets `snapshot` where it is available and a refusal with
   the message naming the threshold where it is not; the choice is on the
   branch row and in the event.
3. **`clone` refuses what it cannot do.** A `dev` branch requested with
   `backing: clone` on an engine where only `clone` exists is refused
   with the message, because a `dev` branch outlives the pool drain it
   would need on reset.
4. **Expiry sweeps both backings.** An expired `clone` branch is dropped
   and an expired `snapshot` branch's Cluster is deleted; both are events;
   a branch with an extended TTL survives (D54 scenario 5, per backing).
5. **A `snapshot` branch serves only its org.** From a branch credential,
   `tables.select` on the branching org's table answers; a connection
   attempt to another org's database on the branch host with the branch's
   roles is refused by the engine (asserted from the branch cluster, in
   the seam test of §3).
6. **Restore keeps the prior state and bumps the generation.** After a
   destructive write, `storage.restore {to}` yields a `main` with the
   rows back, a `restore` branch holding the deleted state, and a new
   store generation with the old pool drained (D54 scenario 6, on this
   backing).
7. **The capability report is honest.** `Engine::capabilities()` reports
   `branch: clone` on plain Postgres, `branch: clone, snapshot:
   shared-instance` on the household's cluster, `branch: neon` on an
   attached Neon project, and `branch: false` on SQLite; every surface
   that asks degrades with its message (D54 scenario 9, extended).

## Order

After D43 phase 1 slice 3 and the D53 runtime, as D54 and D56 say:

1. **`clone`** and the capability report, with D54's branch verbs, the
   pool key and the sweeper (D54 order 3 lands here). Scenarios 1, 2, 3,
   4 (clone half), 7.
2. **`schema.plan` on a `clone` branch** (D54 order 4). D54 scenario 3.
3. **The cluster half** with Pia: VolumeSnapshotClass confirmed,
   `backup.volumeSnapshot` on the household's Cluster, the namespace,
   service account and branch Cluster template in `cluster/tofu`.
4. **`snapshot`**: the Kubernetes client, the branch Cluster lifecycle,
   the seam test. Scenarios 4 (snapshot half), 5.
5. **Restore** on the `snapshot` path. Scenario 6.
6. `hive dev --branch` and `storage: branch` on whichever backing the org
   has (D54 order 5).

## What lost

- *Self-hosted Neon now.* Real copy-on-write branching at the cost of the
  control plane and the Pis; still D54 §7's later record, and this
  record is what we run until then.
- *`clone` for everything.* A full copy per `dev` branch of nectar's
  library is minutes and gigabytes, and the pool drain on every reset is
  a pause the household would feel.
- *`snapshot` for everything.* One Postgres instance per `plan` branch
  for a migration rehearsal that takes a second is a waste the small-org
  path avoids.
- *Logical replication or `pg_dump` into a branch.* Slower than either
  backing and not a point in time.
- *Pretending a `snapshot` branch holds one org.* §2.
- *The daemon running `kubectl`.* A scoped service account and two
  resource kinds through the API, or nothing.

## Open

- The size threshold between `clone` and `snapshot`, measured on the
  household's databases once slice 3 has them.
- Whether a `snapshot` branch may run with less memory than `main`
  (likely yes; the branch Cluster template's caps are Pia's call).
- Branch cluster start time on Longhorn, measured; it sets the `dev`
  branch's "ready" latency and whether `hive dev --branch` warms one.
- A CNPG cluster per org for the household's larger orgs, if §2's
  boundary is ever not enough; D43 allows it and nothing here precludes
  it.
- Garbage collection of branch Clusters by the cluster itself when the
  daemon is gone (a TTL controller or a CronJob); Pia's side.

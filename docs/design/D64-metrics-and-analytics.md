# D64: time series and reporting are two more shapes on the storage interface, not a columnar engine under `tables`: a `metrics` primitive (points with labels, append-only, retention) with Postgres partitioning as the default driver and a Prometheus-compatible store as an added one, and `analytics` as a derived read model (the operation log and events exported as Parquet on the blob driver, read-only SQL answered by a query engine that runs as a platform process beside the daemon, ClickHouse as an added driver when scale needs it)

**Decided** 2026-10-14 by Nate: "can we add a columanr database driver for
storage and, or prometheous storage for time series data and mass
datalake like reporting". Shape by Propolis. Builds on D52 (the storage
interface, its primitives, drivers per `(org, primitive, generation)`, the
conformance suite, and the rule that a driver never decides access),
D43 and D56 (Postgres per org on the cluster), D60 (every interface write
records an operation, which is the change feed this record consumes),
D40 and D63 (blobs and files, where the exported columns live), D53 (apps
and platform work as processes in containers, so a native query engine
runs beside the daemon and never inside it), D47 (bus items), D45
(replicas) and invariants 1, 12 and 14. **Amends D52 §2**: five
primitives, `metrics` and `analytics` beside blobs, tables, documents and
files.

## Why not a columnar driver under `tables`

The first reading of the ask is a columnar engine (ClickHouse, DuckDB,
Parquet) bound as a driver for the `tables` primitive. It does not fit,
and the reason is the interface's own rule. A `tables` row is updated in
place with a per-field HLC (D45 §3), read back with `RETURNING`, grown by
`schema.plan`, filtered row by row by the `Guard`, and written inside a
transaction that also writes an operation (D60). A columnar engine is
built to append batches and scan columns, and does none of those well or
at all; a driver that passed the conformance suite would be a row store
wearing a columnar name, and one that did not would make `tables` mean two
things. D52 §3 says a driver never changes the shape of a primitive. So
the columnar engine goes where its shape fits: behind a primitive whose
contract is append and scan.

The second reading is Prometheus as a storage backend. Prometheus is a
scrape-and-query server with its own data lifecycle and, on its own, no
tenancy; it is the right *driver* for a time-series primitive whose
contract is "points with labels, written once, read by range and
aggregate", and the wrong thing to hand an app as a database.

## The decision

### 1. `metrics`: points with labels

- **A series** is a name plus a set of labels in a context; **a point** is
  a timestamp and a float on a series. `metrics.write {points}` appends;
  there is no update and no delete of a point. `metrics.query {selector,
  range, step, aggregate}` reads: a selector names a series set by labels,
  the range and step give the samples, the aggregate (`last`, `avg`,
  `min`, `max`, `sum`, `rate`, `count`) folds them. `metrics.series
  {selector}` lists what exists. Retention is per series set, declared by
  the app in its manifest with the platform's default, enforced by the
  driver on the daemon's schedule.
- **Ownership and the Guard.** Every series carries the owner (person or
  org), the install and the author actor the way every row does, and the
  interface injects them as labels (`hive_org`, `hive_install`,
  `hive_owner`) the caller never sets. A selector is rewritten by the
  `Guard` to carry the caller's labels before it reaches any driver, and
  a selector that arrives already carrying them is refused: a label set
  that omits a dimension is a key that omits a dimension (invariant 14),
  and the eighth instance of that rule was a predicate never told which
  install was asking. Absence is deny (invariant 1).
- **Trust.** A point written after an untrusted read is untrusted
  (invariant 12), carried as a label the driver stores and the query
  returns; an aggregate over any untrusted point is untrusted.
- **Default driver: Postgres.** A `metrics` schema per org beside the
  install schemas, one table per series set partitioned by time range,
  a BRIN index on time, retention as `DROP PARTITION`, and rollups the
  daemon computes on its schedule into coarser partitions. This is the
  household's store (D56) and needs no new server.
- **Added driver: a Prometheus-compatible store.** Writes go by
  `remote_write`, queries by the HTTP API with PromQL the interface
  generates from the selector, never from the caller, with the injected
  labels as matchers. A store with tenancy (Mimir, VictoriaMetrics, Thanos
  receive) gets the org as its tenant as well as a label; plain Prometheus
  gets labels only and is for one org or a household. Binding is per
  `(org, metrics, generation)` as D52 §3, after the conformance suite.
  Whether the cluster runs one and which is Pia's side; the record names
  the shape, not the pick.
- **Not this:** the daemon's own operational metrics (request counts,
  pool waits, run durations) are exported by the daemon in Prometheus
  format for the cluster's observability and are not app data; this
  record does not touch them.

### 2. `analytics`: a derived read model, read-only SQL over columns

- **What it is.** A read-only, columnar copy of an org's data, rebuilt
  from the change feed the interface already produces, queried with SQL
  that returns a result set, for reports, dashboards, exports and the
  "what does the whole library look like" questions a row store answers
  slowly. It is derived: nothing written through `analytics`, no app
  owns its shape, and a loss of it is a rebuild, never a data loss.
- **The feed.** D60's operation log carries every write to tables,
  documents and files; the events table carries the rest. An exporter
  role in the daemon (`--run-analytics-export`, on by default like the
  others, D7) folds operations since its cursor into **Parquet** files
  on the org's blob driver (D40), partitioned by org, source (table or
  collection name), owner and day, with a manifest row per partition in
  the org's database (so the catalogue is rows, and rows are what the
  `Guard` decides). Blobs written this way carry references like any
  other (invariant 8), owned by the platform's actor for the org.
- **The engine.** A query engine is a native library, and the daemon
  carries one native engine on purpose (D38; `unsafe` denied at the
  workspace, one named exception). So the engine does not link into the
  daemon: it runs as a **platform process under the harness** (D53's
  shape, one container per org that has asked for analytics recently),
  with the partitions the caller may read mounted read-only (a D63
  mount, assembled per call from the catalogue rows the `Guard` returns)
  and nothing else. DuckDB is the first engine because it runs in that
  container with no server and reads Parquet natively; the process
  answers one query at a time over the daemon's socket, with a deadline
  and a memory cap from the host profile. The `Guard` decides *before* the
  mount what the query may see; the engine cannot ask for more than it was
  given, because the files it was not given are not there.
- **The verbs.** `analytics.query {sql, params}` returns rows and the
  trust of the result (untrusted if any partition read carries an
  untrusted row, invariant 12). `analytics.schema` lists the sources and
  columns the caller may query. `analytics.freshness` says how far the
  export is behind the log, so a report can say which hour it reflects.
  `analytics.export {sql, format}` writes a result as a file into the
  caller's workspace (D63) as CSV or Parquet.
- **Reports.** A saved query with a schedule and a recipient is a
  **report**: the daemon runs it, writes the result as a file, and puts
  a bus item for the person (D47) naming the file. A report is a row in
  the kit's store like a workflow definition, content-addressed, and the
  run log is the checkpoint journal (invariant 6).
- **Added driver: ClickHouse.** When an org's columns outgrow a process
  reading Parquet, the export lands in a ClickHouse database per org
  instead, with row policies carrying the owner, and `analytics.query`
  goes to it over its HTTP interface; same verbs, same `Guard` before the
  call, same catalogue rows. Running it is the cluster's business (Pia's
  side) and the binding is per `(org, analytics, generation)` after the
  conformance suite.
- **Not this:** `tables` do not become columnar, apps do not write to
  `analytics`, and no caller sends SQL to a database the `Guard` has not
  scoped first.

### 3. Where the pieces land

- nectar (D56): playback events, scan durations and transcode queue
  depth become `metrics` series under its install; "every album by decade
  with play counts" is an `analytics.query` over its tables' columns; the
  weekly library report is a report.
- The journal and the kit's own UI: the storage page (D52 §4) gains the
  export's freshness and size, and the settings slot (D55 §3, D62) gains
  the retention of each series set.
- The household's dashboards: Grafana, if the cluster runs it, reads the
  Prometheus-compatible store an org bound and the daemon's operational
  metrics; it never reads an org's database.

## Acceptance scenarios, checked in before the code

1. **A selector cannot omit the tenant.** `metrics.query` from install A
   never returns a point written by install B in the same org, and a
   selector that names `hive_install` itself is refused with the message
   (asserted on both drivers through the conformance suite).
2. **Points are append-only.** No verb mutates a point; a second write of
   the same timestamp on a series is a second point or a refusal per the
   driver's rule, and the record of which is in the conformance suite.
3. **Retention runs.** A series set with one-day retention loses its
   partition on the daemon's schedule, asserted by the partition count on
   Postgres and by the store's own API on the Prometheus driver.
4. **Trust folds.** One untrusted point in a range makes the aggregate
   untrusted (invariant 12's existing test, through `metrics`).
5. **The export is the log.** Writes to a table appear as Parquet rows
   within one export interval, with their HLC and owner columns; a row
   the caller may not read through `tables` is absent from the partitions
   mounted for their `analytics.query` (asserted by listing the container's
   mount).
6. **The engine sees only its mount.** A query naming a partition outside
   the mount fails with the engine's "no such file" and the daemon's event
   names the refusal; the container has no socket but the daemon's and no
   network (D53 scenario re-run).
7. **Freshness is honest.** With the exporter stopped, `analytics.freshness`
   grows and a report's file says which cursor it reflects.
8. **A report delivers.** A saved query on a schedule writes a file into
   the recipient's workspace and a bus item names it; the run log records
   each run once (invariant 6's shape).
9. **Secrets stay out.** A `remote_write` or ClickHouse credential is a
   vault lease (D48) the driver holds and the catalogue never shows.

## Order

1. **`metrics` on Postgres**: the schema, partitions, verbs, the label
   injection and the selector rule. Scenarios 1 to 4.
2. **The exporter** and the Parquet catalogue rows. Scenario 5, 7.
3. **The engine container** with DuckDB and the per-call mount (after
   D53's runtime and D63's mounts). Scenario 6.
4. **Reports** (after D47 and D63). Scenario 8.
5. **The Prometheus-compatible driver**, then ClickHouse, each after its
   conformance run and Pia's side bringing the store up. Scenario 9.

After D43 slice 3 (done today) and the D53 runtime.

## What lost

- *A columnar engine as a `tables` driver.* It cannot carry the
  interface's row semantics, and a driver that changes a primitive's shape
  is two primitives with one name.
- *Prometheus as an app's database.* No tenancy, its own lifecycle; it is
  a driver for one shape.
- *The query engine linked into the daemon.* One native engine, by D38;
  a second is a process.
- *Apps sending SQL to a scoped database.* The `Guard` scopes the mount,
  not the SQL; a query cannot ask for what is not there.
- *A columnar copy as the source of truth.* Derived, rebuildable, behind
  the log by a stated amount.

## Open

- Export interval and partition size per org; measured, not guessed.
- Whether a person's `metrics` (not an install's) exist at all before an
  app asks for them; assumed no.
- The conformance suite's rule for duplicate timestamps (scenario 2).
- Rollups: which aggregates, at which coarseness, kept how long.
- Cross-org analytics for the household (one person's view over two orgs)
  is a grant question on catalogue rows, not designed here.

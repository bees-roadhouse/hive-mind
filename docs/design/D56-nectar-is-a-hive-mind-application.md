# D56: nectar is published as a hive-mind application on the standard interfaces; vectors are a column type on tables and a declared index on documents, scoped per context; the household's store is the cluster's Postgres, not hosted Neon, with branching as the platform's own capability (D59)

**Decided** 2026-10-14 by Nate, relayed by Pia from Forage. The direction:
"so we publish nectar as a hive-mind application.. and point to what
hive-mind is. we have nectar use the hive-mind standard interfaces for
storage compute etc.. (we need to add vector storage for orgs/users/apps)..
best we switch hive-mind to neon now." Then, when Forage put the latency
of a hosted store under a media player to him: "let's just stick with
postgres for now and figure out how to do our own branching?" Shape by
Propolis. nectar is the reference consumer of the kit (D55), and this
record is the first of three that say what the reference consumer needs
from it: this one storage and placement, D57 identity, D58 streaming.
Builds on D55 (one shared instance per deployment; a host install with a
host profile; plugins as D53 process apps), D52 (one storage interface;
the org-owned schema), D41 (vectors and full text on collections), D43
(a database per org; an org may attach its own Postgres; the pool key),
D53 (apps as processes), D54 (the branch capability and its absence).
**Amends D54 §1** (where the Roadhouse's own install stores its data) and
**D52 §2** (vectors on tables); D55 §5 said whether a host's domain
records move onto the kit's storage is a later decision per host, and
this record makes it for nectar.

## What was checked before deciding

- **nectar's data model today**, from `api/migrations` (58 files, read
  2026-10-14): sixty-odd tables. The media core (`libraries`,
  `media_items`, `media_streams`, `media_chapters`, `media_subtitles`,
  `media_people`, `media_keyframes`, `media_transcript_chunks`, `people`,
  `genres`, `studios`, `tags`, `images`, `external_ids`), identity and
  access (`users`, `user_identities`, `sessions`, `api_keys`,
  `oidc_auth_codes`, `user_library_access`, `user_preferences`), playback
  and viewing (`playback_activity`, `playback_progress`, `watch_history`,
  `watch_party_room`, `watch_party_member`), the live transcode ledger
  (`live_hls_streams`, `live_hls_segments`, `live_hls_viewers`,
  `transcode_jobs`, `transcode_job_claims`, `transcoder_workers`,
  `transcoder_slot_leases`), jobs (`job_batches`, `job_batch_items`,
  `job_runs`, `job_run_logs`), the arr and download side (`arr_*`,
  `downloads`, `request`, `quality_profile`, `custom_format`), and vectors
  (`media_items.embedding vector(1536)` with an `ivfflat` cosine index,
  `scene_embeddings`, `audio_embeddings`), plus a `tsvector` trigger on
  titles. Full text and vectors are already Postgres features nectar
  relies on, which is why D41's surface is the right one to extend and
  not a new one.
- **nectar's server is Bun** (nectar#251) and its migrations are plain
  SQL, so "nectar on the standard interfaces" is a rewrite of its data
  access, not a port of a language.
- **D52 already has vectors on documents** (`vector(path, dim)` from D41,
  `near` in `documents.query`) and **none on tables**. The `select` form
  has `where`, `order`, `limit`, `after` and no `near`.
- **D54 §1 put the Roadhouse's own install on hosted Neon** with the
  latency named as a number to measure. The number was never measured;
  nectar's browse, playback decisions and HLS ledger would have paid it on
  every request. D54 §5 already keeps plain Postgres as the fallback
  engine reporting the branch capability absent, and D43 §8 already lets
  an org attach its own Postgres. The cluster's CNPG exists with the
  registry database and the provisioning role (D43's infra section).

## The decision

### 1. nectar is an application on the kit, and a host for its plugins

- **nectar is published as a hive-mind app**: a D53 process app in
  TypeScript on Bun, with a manifest, built to a digest-pinned image, run
  under the harness sandbox, reaching the daemon through the one API with
  an install credential. Its client is served from its routes on its
  per-install origin (D44). It is also a **host install** (D55 §1) for
  its own plugins, with the host profile D55 describes. "Point to what
  hive-mind is": nectar's README names the kit as what it runs on, the
  way a Rails app names Rails.
- **nectar's domain data moves onto the standard storage interface**
  (D52 §2): its tables become tables in nectar's install schema, declared
  in its manifest's `storage` block (D52 §4's app-declared tables, moved
  up the order); its media metadata that is document-shaped stays
  document-shaped as collections; its images and subtitle files are blobs
  by reference (invariant 3). Access is through `hive.tables`,
  `hive.documents` and `hive.blobs` in `@hive/app`, with the constrained
  query forms and never raw SQL. What nectar gives up is a hand-tuned SQL
  path per query; what it gets is one enforcement point, owner and author
  pinned on every row, RLS as the second wall, sync (D45) and projections
  (D49) for free, and sealed contexts (D51) when a person wants their
  watch history private from the operator.
- **The migration is slice by slice with parity tests**, the way nectar's
  own Rust-to-Bun rewrite went (nectar#251): one table group at a time,
  nectar's existing parity harness asserting the same answers from both
  paths, the old table dropped when the new one has carried traffic. The
  order is nectar's (Forage's) to set; the kit's order is below.
- **Compute**: nectar's workers (scan, embed, transcode, transcribe,
  recommend) are the app's own processes under its install; models go
  behind D42's seam, with nectar's ONNX `infer` as an endpoint behind it
  (D55 §5), adding `rerank` and multimodal `embed` to D42's capabilities.
  The transcoder, which must run where the GPU is, is a D53 app with a
  node constraint, which D53 left open and this record names as needed.

### 2. Vectors are a column type on tables and a declared index on documents, per context

- **On tables**: a column type `vector(dim)` in an app-declared or
  org-owned table (D52 §1 and §4), provisioned as `pgvector` with an HNSW
  index by default (`ivfflat` selectable where an org wants it, with the
  trade-off stated by the planner: faster build, lower recall). The
  `select` form gains `near: {column, vector, k}`, ranking by distance the
  way `documents.query`'s `near` does (D41), composable with `where`.
- **On documents**: unchanged from D41 and D52: `vector(path, dim)`
  declared on the collection, `near` in `query`.
- **Scoped per context.** A vector lives in the row it belongs to, so it
  is scoped the way the row is: the install's schema, the org's database,
  the person's owner. There is no shared vector store across contexts; a
  cross-context similarity search is a request to the other side (D43 §4,
  D49), never a join. "Vector storage for orgs/users/apps" is therefore
  not a new store but the existing one with the type on every primitive
  that holds rows.
- **Where the vectors come from** is the caller's business, as D41 said:
  nectar's embed worker, or D42's `embed` capability. The engine never
  computes one.
- **Under D51**, a vector of sealed content is itself content: sealed by
  default, indexable server-side only in served mode or under an unlock,
  and the planner says so (D52 §1's rule, applied to the new type).
- **SQLite's form** (`vec0`, D41) stays the single-box backing; the
  capability `vector` is reported by the engine like `branch` is (D54
  §5), with dimension and index-method limits, so an app declaring a
  column the engine cannot index is refused at install with the message.

### 3. The household's store is the cluster's Postgres; branching is the platform's own capability

- **Amends D54 §1.** The Roadhouse's own install does **not** go to hosted
  Neon. The household org's database is on the cluster's CNPG, where the
  registry and the provisioner already are (D43's infra section). The
  reason is Forage's and it is a good one: with nectar's hot path on the
  kit's store, a cloud round trip per request is a cost on every play
  button in the house. Nate chose the engine he has.
- **D54's standard stands.** The branchable store is still the platform's
  shape; what changes is the backing. On the cluster, the `branch`,
  `reset` and `restore` capabilities are provided by **D59**, the
  platform's own branching on plain Postgres, and reported absent until
  D59's slices land. Nothing above `hive-db` changes.
- **Hosted Neon remains the engine for a hosted hive** and for anyone
  who attaches a Neon project as their org's database (D43 §8). The Neon
  driver and its CI job (D54 §5) are still built, after the cluster
  path, and the Neon org and API key on Nate's board are wanted then,
  not now.
- **D43 slice 3 is written against the cluster's Postgres**, which it
  can be today: the podman test database locally when it is unwedged,
  CI's Postgres service meanwhile. Nothing of Nate's blocks it.

### 4. What nectar keeps and what moves, said once

| nectar today | under this record |
|---|---|
| its own Postgres and migrations | tables and collections declared in its manifest, provisioned per install, migrated by `schema.apply` under its own order |
| its own users, sessions, OIDC codes, API keys | delegated to the kit's identity interface, **D57** |
| byte-range and HLS serving, SSE, Jellyfin-compatible root routes | through the kit's dispatch once it streams, **D58** |
| `infer` in-process ONNX | an endpoint behind D42's seam |
| its plugin host | gone (nectar#288); plugins are D53 process apps on the kit |
| its Solid client | unchanged, served from its install's routes on its per-install origin, rendering plugin slots per D55 §3 |

## Acceptance scenarios, checked in before the code

1. **A vector column on a table.** An app manifest declaring a table
   with `vector(1536)` installs on Postgres with an HNSW index (asserted
   from the catalogue), and `tables.select` with `near` returns rows in
   distance order with distances; the same manifest on SQLite installs
   with `vec0` and answers the same query; a dimension mismatch on
   insert is refused with the message on both.
2. **Vectors are scoped.** Two installs of the same app each hold rows
   with vectors; `near` from one never returns the other's rows, with or
   without a `uses` grant on the other's table (a grant allows `select`,
   and `near` is a `select`; absent, deny).
3. **Sealed vectors stay sealed.** In a sealed context, `near` on a
   content vector is refused without an unlock with the message naming
   served mode; under an unlock it answers.
4. **The engine reports `vector` honestly.** On an engine without the
   extension, an install declaring a vector column is refused at install
   with the message; `Engine::capabilities()` says so.
5. **nectar's media core on the interface.** nectar's checked-in parity
   fixture (its `parity/` harness) runs a browse, a search, a playback
   progress write and a recommendation query against the old SQL path and
   the `@hive/app` path and gets the same answers; this is nectar's test,
   run in nectar's CI against a hive-mind daemon image.
6. **The household store is local.** The registry's `org_stores` row for
   the household org points at the cluster's CNPG, `Engine::capabilities()`
   reports `branch: false` until D59, and `schema.plan` returns its static
   line (D54 §3).

## Order

1. D43 phase 1 slice 3 (the store on Postgres, now against the cluster's
   engine). Unchanged, still first.
2. The D53 runtime and `@hive/app`.
3. **Vectors on tables** (§2) alongside D52 order 2 (the interface over
   the existing engines) and app-declared tables moved up from D52 order
   6. Scenarios 1 to 4.
4. **nectar's manifest and first table group** on the interface, with
   its parity fixture. Scenario 5. nectar's slices continue on nectar's
   side from here.
5. D59 for branching on the cluster; D57 and D58 as their records say.
6. The Neon driver and its CI job (D54 §5), after the cluster path.

## What lost

- *Hosted Neon for the Roadhouse's own install* (D54 §1 as written). A
  cloud round trip under a media player; Nate chose the local engine and
  own branching.
- *A separate vector store.* One more backend keyed on something; vectors
  are rows' columns and scoped as rows are.
- *nectar keeping its own Postgres beside the kit's* (D55 §5's default
  for a host). Allowed for other hosts, not what Nate wants for nectar:
  "use the hive-mind standard interfaces for storage compute etc."
- *Raw SQL for nectar as a special case.* The interface's query forms or
  nothing; where a form is missing, it is added for every app, which is
  how `near` reaches tables here.
- *A big-bang migration of nectar's data.* Slice by slice with parity,
  as nectar#251 did.

## Open

- Node constraints for a D53 app that must run where the GPU is (the
  transcoder); D53 left scale-out open and this adds placement.
- Whether `near` composes with `search` in one `select` (hybrid rank);
  wanted by nectar's recommendations, not designed here.
- Dimension and index limits per engine, written when measured.
- Whether nectar's `scene_embeddings` and `audio_embeddings` (per-segment
  vectors) are tables with vectors or documents with vectors; nectar's
  call under D52's forms.

# D41: vectors and full text live in the engine; one `unsafe` in hive-db, held to by the gate

**Status:** decided 2026-10-03. Needed by the journal and the documents app
(both search; documents embed what a local model transcribes), and the
thing D38 left open under "the vector index method for app collections".

## What the probe measured

Against the bundled engine (`rusqlite` 0.37, SQLite 3.50.2) with
`sqlite-vec` 0.1.9 registered through `sqlite3_auto_extension`, in an
owner file attached inside `BEGIN IMMEDIATE` (D39):

1. FTS5 is in the bundled build; nothing to add.
2. A `vec0` virtual table with a `TEXT PRIMARY KEY` (the document's uuid)
   and a `float[n]` column creates inside the attached file, inside the
   transaction.
3. A trigger on the collection table inserts into the `vec0` table from a
   JSON path (`vec_f32(json_extract(NEW.doc, '$.embedding'))`) with bare
   names in its body, which is the only form the engine accepts there (D39
   measurement 3); delete and update triggers keep it in step.
4. A KNN (`embedding MATCH vec_f32(?) AND k = ?`) answers from the attached
   file, with distances; a vector of the wrong dimension is refused at the
   insert with a message naming both dimensions.
5. FTS5 the same way: a virtual table in the attached file, a trigger from
   a JSON path, `MATCH` with `rank`.
6. **Dropping the collection table drops nothing of its virtual tables.**
   `vec0` and FTS5 each leave a family of shadow tables; the virtual table
   has to be dropped by name, and its shadows go with it.
7. The extension is process-wide: a connection opened without any
   registration call still reads a `vec0` table, so there is exactly one
   place to register it, once, before any file is opened.

## The decision

### 1. The engine holds the indexes; the manifest declares them

`vector(path, dim)` provisions `<schema>__<collection>_<n>_vec`, a `vec0`
table keyed by the document id with `float[dim]`, and three triggers that
mirror the JSON array at `path` into it on insert, update and delete. A
document with no value at the path is simply not in the index. `fts(path)`
provisions `<schema>__<collection>_<n>_fts`, FTS5 with the id unindexed and
the text at `path`, mirrored the same way. `n` is the index's ordinal in
the manifest, so two indexes on one collection cannot collide and a
manifest diff that reorders them is a reprovision (D3.3, as before). The
expression-index placeholder that stood in for `fts` goes; `IndexMethod::
Vector` stops refusing.

`btree` and `gin` stay what they were. Uninstall drops the virtual tables
first, by name, then the collection tables (measurement 6).

### 2. Two more questions a query can ask, decided in the one place

`storage.query` gains `search` and `near`, beside `match`:

- `search: "<words>"` restricts to documents some `fts` index of the
  collection matches. Every token is quoted as a phrase before it reaches
  `MATCH`, so FTS5's own query language is never exposed to a guest: a
  guest asks for words, not for a query.
- `near: {path, vector}` ranks by distance from `vector` in the `vector`
  index declared at `path`, `limit` being `k`. Rows come back with
  `distance`. `k` is taken from the index before the predicate filters, so
  a caller may receive fewer than `limit`; it never receives one it may
  not see, because the rows still go through the same `JOIN entities` and
  the same predicate text as every other read.

Both are filters on the same statement the predicate already governs
(invariant 1), and both resolve the index to consult from the install's
manifest off its build row, never from the request (invariant 11). The
generated `list` tool forwards them. Where the vector comes from is the
next decision (a local embedder, D42); the engine side is complete without
it, and a guest that computes its own embedding can use it today.

### 3. One `unsafe` block, in `hive-db`, and a gate instead of a `forbid`

Registering sqlite-vec is `sqlite3_auto_extension(sqlite3_vec_init)`: an
FFI call, `unsafe` in Rust, once per process. D24's rule was `unsafe_code =
"forbid"` at the workspace, the no-CGo rule in the new language, and D38
already named this call as the one exception it would need.

A `forbid` cannot be overridden by a member crate, so the lint becomes
`deny` at the workspace and `hive-db` carries `#![allow(unsafe_code)]` on
the one module that does the registration. What keeps that from quietly
spreading is not the lint but the gate: `hive-repodocs` asserts that no
crate other than `hive-db` contains `allow(unsafe_code)` or an `unsafe`
block, so adding a second site means editing that test in the same commit
and saying why. That is the same shape as the invariant count: a rule the
file cannot enforce on its own, held by a test that names it.

The alternative, loading `vec0.dll` at runtime through `load_extension`,
is also `unsafe` in `rusqlite` and adds a file to ship and a path to get
wrong; the `sqlite-vec` crate compiles the C into the binary beside the
amalgamation, which is where the engine already is.

## What lost

- *Keep refusing vectors until a model exists.* The journal's search does
  not need a model and the documents app's embeddings are a column the
  seam will fill; the index has to exist first.
- *A separate vector store.* A second native library and a second file
  per owner for the one thing the engine does in the same transaction.
- *Exposing FTS5 syntax.* A query language a guest composes is an
  injection surface and a place for a second access policy to grow; words
  are enough for what the apps need, and a richer form can be added as a
  separate field when one of them asks.

## Open

- Hybrid ranking (text and vector together) is not defined; a caller asks
  one or the other.
- `vec0` is brute-force at this size; sqlite-vec's partitioning and
  quantisation are tuning for when an owner's file has enough rows to
  need it.
- The embedder and the rest of the local model seam: D42.

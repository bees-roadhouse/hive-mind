# Design decisions, snapshotted

The design lives in a Traycer epic on the maintainer's machine: a decision log
D0 to D23 and a plan set. Nothing outside that machine can open it, and several
of those decisions are corrections that explain why the code looks the way it
does. Issue #28 asks for them to be snapshotted here.

This directory starts that, from the first decision made **after** the epic
stopped being the only copy. Earlier entries are back-filled as they are needed
to explain a change; an entry that is referenced from a commit or a doc and is
missing here is a gap worth closing, not a formality.

One file per decision, `D<n>-<slug>.md`. Each records what was decided, the
reasons, what it replaces, and what it deliberately leaves open. Reasons
outlive picks: a named crate or version goes stale, the criterion that chose it
does not.

| entry | decision |
|---|---|
| [D24](D24-rust-rewrite.md) | the daemon is rewritten in Rust, beside the Go tree, tests first |
| [D25](D25-git-for-apps.md) | git is the change management for apps: sources, not modes |
| [D26](D26-five-open-items.md) | the instance repo, host builds, checkout storage, the AI actor, the cookie rule |
| [D27](D27-agents-relate.md) | one agent graph over AI actors, edges as grants, profiles as runtimes, definitions never credentials |
| [D28](D28-profile-context.md) | a profile's CLAUDE.md is a generated view, the briefing is live, trust decides what is inline |
| [D29](D29-accounts-to-profiles.md) | accounts are records with a trust domain, profiles bind to one, domains must match, the four layers named |
| [D30](D30-voice-on-the-client.md) | voice is an interface; Kokoro renders on the client, the daemon never makes audio, the server stack is the fallback |
| [D31](D31-go-removed.md) | the Go tree is removed at parity; migrations move, the TinyGo guest is frozen as the ABI fixture, flags become `--long` |
| [D32](D32-online-first-pluggable.md) | online-first with htmx, every resource a host capability, core entities shared by grant, the journal as a guest, TypeScript as the second guest language, a person's own Claude subscription runs their agents |
| [D33](D33-collection-access-needs-the-asking-install.md) | cross-app collection access is decided on the asking install as well as the principal; grants gain an install target |
| [D34](D34-hive-mind-rename.md) | the repository is Hive Mind; the crate, binary, env vars and test database stay `hive-sandbox`, because a name's cost is its blast radius |
| [D35](D35-the-sign-in-is-the-binarys.md) | a person's subscription signs in to the unmodified CLI inside their own container; the platform keeps a per-principal config volume and never the token; the vault leases API keys; automated runs use keys; the terms and the accepted residual risk are on the record |
| [D36](D36-voice-is-a-server-worker.md) | voice both ways on a server worker, CPU-only today, behind the OpenAI audio shapes; text stays the record and audio is never stored; Parakeet-TDT 0.6B v3 and Kokoro-82M picked against written criteria, measured before final; reverses D30's placement |
| [D37](D37-model-gateway.md) | an Anthropic-format gateway in front of OpenAI as a daemon role, for the Claude Code harness only; a one-run token in the container and the provider key on the daemon's side; the degradations named |
| [D38](D38-libsql-store.md) | the store is SQLite through `rusqlite`, one file per daemon now and one per owner next; libSQL was the first pick and measured out on a heap bug; policy triggers stay in the file, the predicate text moves into the store, the bell is in-process; the client stays htmx |
| [D39](D39-owner-files.md) | an owner's documents live in the owner's file, attached inside the caller's transaction; the control plane, the index rows and the events stay central; the file carries a marker saying whose it is; a layout, not a throughput change |
| [D40](D40-blobs-on-a-mounted-path.md) | blobs live on the path mounted at the blob root and replication is the mount's job (JuiceFS on TiKV); the S3 driver, Garage and their tier go; the store files stay on local disk |
| [D41](D41-vectors-and-full-text.md) | `fts(path)` and `vector(path, dim)` provision FTS5 and vec0 tables in the owner's file, mirrored by triggers; `storage.query` gains `search` (words, never syntax) and `near`; sqlite-vec is the one `unsafe` in the host, held to hive-db by the gate |
| [D42](D42-local-model-seam.md) | local models behind one seam: four capabilities (read, transcribe, generate, embed) bound to OpenAI-shaped endpoints by configuration, a job queue in the store, `--run-models` as the worker role, the `models` guest capability; the worker writes no document and nothing a model says is trusted |
| [D43](D43-postgres-per-org.md) | the server store returns to Postgres, superseding D38/D39 for the server: one database per organization found through a registry keyed on (org, store generation); the predicate back in the database as functions, row-level security as the second wall from `SET LOCAL` facts `hive-db` sets from the credential; cross-org only by a link set up on both sides and a request to the other side's daemon, never a join; SQLite stays behind `hive-db` as one org, with one suite run against both engines |
| [D44](D44-app-ui-isolation.md) | an app's HTML never runs as the shell: app UI in a sandboxed frame on a per-install origin; where no frame, an allowlist sanitizer drops every `hx-*`/`on*`/script and the host re-adds behaviour only for actions the manifest declares, inside `hx-disable`; the shell's mutating routes check an HMAC capability token keyed on session, verb and route instead of `HX-Request` |
| [D45](D45-offline-shell.md) | supersedes D32 §1: the daemon serves APIs only (user and admin), with no server-rendered UI and no htmx; one Solid.js client does everything, with a complete per-person offline replica in OPFS SQLite that the database never gives up for blobs; blobs are cached under one per-device limit, least recently used evicted first; writes go up as an operation log through the normal write API; last-writer-wins per field on a hybrid logical clock, both versions kept for long text; a hash is never a reference; a bounded offline session, wiped on revocation; apps stay in D44 frames |
| [D46](D46-boards-and-encryption.md) | no org journal; boards (org and personal) hold shared cards, placing is not copying, and emergence is links with backlinks; an item lives in one context, and sharing a private item moves it into the org after a warning; three encryption tiers: a person's own content sealed on their devices with per-field associated data, keys wrapped per device and by a recovery key proved before first use; org backups and blobs under a per-org key; infrastructure at rest; personal documents OCR'd and embedded on the device, org documents on the server; one workspace signed in to every context, with labelled side-by-side panes |

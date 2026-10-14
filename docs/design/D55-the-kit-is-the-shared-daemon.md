# D55: hive-mind is the standard resource kit for other apps: one shared instance per deployment by default, each host app registered as a host install with a host profile, a sidecar only when an app runs standalone; a host's plugins are D53 process apps with state in the kit's store; UI slots render declared schemas or framed mini-apps; in-process crates come later, for Rust hosts only, through a builder that keeps storage behind `Guard`

**Decided** 2026-10-14 by Nate. The idea, relayed by Pia from Forage: "can we
make hive-mind's code a standard library crate to use? I wouldn't mind
sharing the logic for any app to self host it's own ecosystem like this
but be focused on certain things." The plugin part first, the same
afternoon: "yeah drop nectar's plugin host. we'll reference to use an app
like windmill or hive-mind for integrations. But i'd love to allow like
javascript to run with UI edits.. and the ability to store plugin database
records to store state." Then the whole, to Pia's "Confirm D55: hive-mind
is the standard kit, run as a sidecar per app?": "yeah let's do that." And
the question that fixed the deployment shape: "so i only need to run
hive-mind once then?" Yes. Forage's correction to the first draft of this
record, taken: hive-mind is already multi-tenant (orgs, installs, grants,
a schema per install), so the household runs one instance and apps
register with it; a sidecar is for an app that ships without the rest of
the Roadhouse. Shape by Propolis (the 2026-10-14 journal page and
nectar#288). nectar is the first consumer. Builds on D53 (Rust is the
framework, apps are processes over the socket and the API), D52 (one
storage interface, declared per install), D44 (an app's markup never runs
as the shell), D45 (APIs only), D47 (the bus), D42 (the model seam), D48
(vault leases), D50 (sign-in), D43 (a database per org, a schema per
install), D7 (one binary, roles as flags) and D33 and invariant 14 (every
tool name carries the install). Amends nothing; it names a new kind of
consumer of what the records already describe and adds the two things
that consumer needs, a host profile on its install and a `ui` block in
the app manifest.

## What was checked before deciding

Read from both trees on 2026-10-14:

- **hive-mind is twenty-three library crates and one binary** composing
  them in about 840 lines of `main`; the workspace is `publish = false` at
  0.1.0. Seven crates have no internal dependency (`hive-db`, `hive-egress`,
  `hive-identity`, `hive-manifest`, `hive-sse`, `hive-trust`, `hive-webui`);
  `hive-wasmhost` and `hive-harness` depend only on identity and trust.
  `hive-store` is the hub and depends on blob, db, harness, identity,
  manifest, registry, schema, trust and wasmhost, so "storage without the
  WASM host and Podman" is not a seam that exists. `hive-httpapi` depends on
  nearly everything. Half the kit lifts out as crates today; the half a
  consumer wants does not.
- **nectar's server is TypeScript on Bun** (nectar#251, live since
  2026-10-14 14:50Z), so it could never link a Rust crate. Its own
  `plugin-host` was wasmtime Component Model with its own WIT and zero
  plugins installed; hive-mind's WASM tier is WASI preview 1 and frozen by
  D53. Neither should host nectar's plugins. Its `infer` is in-process ONNX
  (embeddings, rerank, CLIP, CLAP, Whisper), which is a backend D42's seam
  could bind to rather than a duplicate of `hive-models`.
- **The daemon is already multi-tenant and already runs as roles** (D7):
  `--serve-api`, `--run-workflows`, `--run-chat`, `--run-models`,
  `--run-egress-proxy`, each on or off by flag; D43 gives every org its own
  database and every install its own schema; D33 and invariant 14 qualify
  every tool name with the install. Nothing about serving a second app
  beside the first is new to it.

## Why a daemon and not a crate, said once

The invariants are the product, and the first of them is one enforcement
point. A crate kit hands a consumer the pieces and lets it compose storage
beside `Guard` instead of behind it; every consumer is then its own
enforcement point, which is the defect invariant 1 was written against. A
daemon over a unix socket and HTTP has one `Guard`, one credential shape,
one audit, and a consumer in any language. It also fails on its own (a
crash in the kit is not a crash in the host app), upgrades once for every
app, and is exactly D53's shape with the host app standing where an app
stands. The crate form is not refused forever: for a Rust host that wants
it in-process, a builder crate can compose the same daemon inside the host
process with storage still behind `Guard`. That is later, and only once a
Rust host exists.

## Why one instance and not a daemon per app, said once

The first draft said "a sidecar per app by default". Forage pointed out
what the records already said: the daemon is multi-tenant, so a second
daemon per app would duplicate the registry, the org database, the vault
and the bus for no isolation the install boundary does not already give.
Everything a host app needs to keep separate from another host app is
already keyed on the install: its schema (D43), its grants (D33), its tool
names (invariant 14), its container's cgroup and egress allowlist (D35
§6, D53 §1). Two things are not per install and are named here so nobody
assumes they are: the daemon's own availability (one instance down is
every app's plugins down, which §1 answers by requiring hosts to degrade)
and the daemon's resource budget (a plugin storm from one host competes
with another's; per-install cgroup limits bound it, and the budget is
Pia's to set on the cluster).

## The decision

### 1. One shared instance per deployment; a host app is a host install

- **A deployment runs one `hive-sandbox`.** The household's is the one on
  the cluster. nectar, and later libstack and whatever else, connect to it
  over the unix socket (invariant 13) or HTTP, each as a client with a
  credential like any other (D45 §1): an actor of kind `app` acting for
  the org that installed it, pinned by the credential (invariant 2). There
  is no host-app-only path.
- **A host app registers as a host install** with a **host profile**, the
  thing that says what this app brings to the kit and wants from it:
  - `host`: the app's name, the qualifier on every tool name the daemon
    exposes for it (D33, invariant 14: `nectar/journal.add` and a second
    host's `journal.add` are different tools, and the registry refuses a
    second host with the same name);
  - `surfaces`: which API surfaces this host and its plugins may use
    (storage, bus, models, vault, agents, identities), each on or off; off
    is refused at the route with the message;
  - `slots`: the UI slots the host offers (§3), by name, each with the
    kinds it accepts;
  - `egress`: the hosts its plugins may reach beyond what their own
    manifests name, enforced by the proxy (D35 §6);
  - `org`: the org the host is installed in; a host serves one org per
    install, and a second org is a second host install.
  The profile is validated at install the way a manifest is (D24:
  everything decidable at install is decided at install): an unknown slot
  kind or a surface the daemon does not serve is a refusal with its
  reason.
- **A sidecar is the same binary, standalone.** An app shipped without the
  rest of the Roadhouse (someone's self-hosted nectar) runs `hive-sandbox`
  beside itself with one host install and D7's roles trimmed to what it
  uses. Same enforcement point, same API, same profile; only the
  deployment differs. Nothing in the kit knows which it is.
- **A host keeps working with the kit down.** The host's own data is
  behind the host's own API (§2), so a host with the daemon unreachable
  loses its plugins, its slots and its events, and nothing else. The host
  profile declares it: a host that cannot run without the kit says
  `requires: kit`, and the default is that it can. nectar's playback does
  not depend on a plugin panel.
- **What the kit provides**, all of it existing records: storage (D52:
  blobs, tables, documents, per install, `schema.plan` and `apply` for the
  org), the bus (D47), models (D42), the vault (D48), agent runs and the
  harness (D35, D53), sign-in (D50), sealed contexts (D51), and the app
  runtime (D53) that runs the host's plugins.
- **What the host app keeps**: its own domain data behind its own API,
  its own client, and the three things nectar#288 names as nectar's side:
  the slot renderer in its client, the scoped tokens it issues to plugins
  that call back into its API, and the hook calls it makes.

### 2. A host's plugins are D53 process apps

- A plugin is an app directory with a manifest and source in TypeScript on
  Bun or in Python, built to a digest-pinned image, run one container per
  install under the harness sandbox, reaching the daemon only through the
  one API with an install credential (D53 §1). It is installed **into the
  host**: its manifest names the host it extends, and the registry refuses
  a plugin for a host that is not installed in that org.
- **Plugin state lives in the kit's store**, declared in the manifest's
  `storage` block and provisioned per install (D52 §4); `hive.tables` and
  `hive.documents` are the only way a plugin keeps data (D53 §2). Reasons,
  stated for nectar#288 and kept here: one enforcement point; the install
  credential pins owner and author on every row, so "plugin X wrote this
  for Nate" is a fact the audit holds; plugin state survives the host
  app's schema changes; a plugin reaches another install's collection by
  a `uses` grant (D33) rather than a bespoke host permission; when the
  host has contexts, plugin state is sealed under D51 with everything
  else.
- **The host app's own records are the host's.** A plugin reads them
  through the host's API with the scoped token the host issues, never by
  a join and never by SQL. Whether a host's domain records ever move into
  the kit's storage is a later decision per host and is not part of this
  one.
- **Events and hooks.** The host appends its events to the bus (D47)
  under its credential with its trust; a plugin declares `subscriptions`
  and the daemon dispatches deliveries to its process with `{trust, data}`.
  A hook point (nectar's metadata provider) is a `tool` the plugin exposes
  and the host calls through the daemon's dispatch; a scheduled task is
  the manifest's `cron`. Anything a plugin fetches from the internet comes
  back `untrusted` (invariant 9) and stays labelled through to the host's
  client.

### 3. UI slots: a declared schema by default, a framed mini-app when the plugin needs script

D44 applied to a host's client: a plugin's markup never runs as the host's
shell. The manifest gains a `ui` block:

```
ui:
  - slot: item.panel        # a slot the host's profile offers
    kind: schema            # or: frame
    tool: panel             # schema: the tool whose result is the schema
    # route: /panel         # frame: the route that serves the mini-app
    schema_version: 1
```

- **`kind: schema`.** The plugin's tool returns a small declared
  vocabulary (text, list, card, table, form, action, link, with a
  versioned schema the daemon validates before the host sees it) and the
  host's client renders it natively, themed and offline-capable. Every
  `action` is a tool call into the plugin through the daemon's dispatch,
  so the plugin's JavaScript runs in its container and the client only
  renders. This is "javascript to run with UI edits" with no JavaScript
  in the client, and it covers the slots nectar named (item panel,
  Console node, item or library action, home shelf).
- **`kind: frame`.** For a canvas, a player, a chart library: the
  plugin's `routes` serve its HTML, the daemon serves it on the
  per-install origin (D44), and the host's slot embeds a sandboxed frame
  whose `src` carries a short-lived token bound to the viewer's session.
  Frame-to-shell messages are `postMessage` with a declared schema the
  shell validates; there is no other channel.
- **Validated at install.** `hive-manifest` is pure and decides everything
  decidable at install (D24): a `ui` entry naming a slot the host's
  profile does not offer, or a kind the slot does not accept, is refused
  at install with the message. The slot list is the host's; the contract
  is the kit's.

### 4. The crate form, later and bounded

- A `hive-platform` builder crate that composes what `main` composes today,
  for a Rust host that wants the daemon in-process: `Platform::builder()
  .host(..).store(..).build()`, serving the same socket and the same API
  from inside the host's process. Storage is reachable only through
  `Guard`, because the builder exposes the store the way the API does and
  not the way `hive-store`'s internals do.
- Prerequisites, none of which exist today: a Rust host that wants it;
  `hive-store` no longer depending on the sandboxes (a feature split);
  semver and publishing; `hive-repodocs` travelling with the invariants.
- Not before D43 slice 3 and the D53 runtime, because both change the
  store and the app shape and the builder would be written twice.

### 5. What the first consumer does

nectar (nectar#288): registers as a host install on the household's
instance with a profile naming its slots, surfaces (storage, bus, models)
and egress, and `requires: kit` absent; emits its `AppEvent`s to the bus;
issues scoped API tokens to plugins; renders schema slots in `web-solid`
and embeds frame slots; keeps playing with the kit down. nectar's `infer`
becomes an endpoint behind D42's seam when it wants to, which adds two
capabilities D42 lacks (rerank, multimodal embed); the model code is not
merged into this repository. A self-hosted nectar elsewhere ships the
sidecar form of the same thing.

## Acceptance scenarios, checked in before the code

1. **A host profile is validated at install.** A profile with an unknown
   slot kind or a surface the daemon does not serve is refused with the
   message; a second host with the same `host` name is refused by the
   registry with the message naming the first.
2. **Tool names carry the host.** Two host installs on one daemon expose
   `a/x.y` and `b/x.y` as distinct tools, and a call to one never reaches
   the other's install (invariant 14's eighth case, re-run for the host
   qualifier).
3. **A host app is a client.** The host's credential reaches exactly what
   its grants allow; a surface the profile turned off answers the refusal
   with the message; there is no route the host can call that a device
   cannot.
4. **Plugin state is the install's.** A plugin's `tables.insert` lands
   with the install's owner and author pinned by the daemon; a second
   install of the same plugin cannot read it without a `uses` grant; the
   host app cannot read it through its own API at all; a plugin for host
   `a` cannot be installed into host `b`'s org without `b` present.
5. **A schema slot never runs script.** A plugin returning a schema with
   an element outside the vocabulary, or a string carrying markup, is
   refused by the daemon's validator before the host sees it, with the
   message; the host's renderer receives only validated schema.
6. **A frame slot is isolated.** The mini-app's origin is the per-install
   origin, the frame's token is bound to the session and expires, and a
   `postMessage` outside the declared schema is dropped and counted.
7. **Hooks dispatch with trust.** A metadata-provider tool that fetched
   from the internet returns `untrusted` and the delivery to the host says
   so; the host's event appended to the bus carries the host's trust and
   actor.
8. **The sidecar is the same binary.** The checked-in example host runs
   against the shared test daemon and against a standalone one started
   with one host install and trimmed roles, and every scenario above
   passes on both; the suite names which it ran.

## Order

Unchanged from D53 and D54, with this record's slices after them:

1. D43 phase 1 slice 3 (the store on Postgres, against a Neon project).
2. The D53 runtime, `@hive/app`, `hive dev`.
3. **Host installs and the host profile** (§1), the host qualifier on tool
   names, plugins installed into a host. Scenarios 1, 2, 3, 4.
4. **The `ui` manifest block, the schema vocabulary and its validator, the
   frame token.** Scenarios 5, 6.
5. **nectar as the first consumer**: its host install on the household's
   instance, its slots, its events on the bus, its hook tools. Scenario 7,
   run against nectar's checked-in example plugin.
6. **The standalone form** proved by scenario 8.
7. The builder crate (§4), when a Rust host asks.

## What lost

- *A Rust crate kit first.* Half the crates do not lift, the only consumer
  is Bun, and a library hands out enforcement points.
- *A sidecar per app by default.* The first draft; withdrawn on Forage's
  correction because the install boundary already separates hosts and a
  daemon per app duplicates the registry, the org database, the vault and
  the bus for nothing.
- *Plugin JavaScript in the host's process or client.* Bun has no sandbox;
  an in-process plugin sees the database credentials, every user's data
  and the filesystem (nectar#288's own first requirement); and script in
  the client is D44's hole.
- *A second plugin runtime inside nectar.* Dropped by Nate; the kit is the
  runtime.
- *Merging nectar's model code into `hive-models`.* The seam is what is
  shared; the backends stay where their dependencies (ONNX Runtime) live.
- *Plugin state in the host's schema.* It would put the host app in the
  position of enforcing access to data it did not write and cannot pin an
  author on.
- *A host that cannot run without the kit, by default.* The household's
  media player should not stop when the plugin daemon restarts.

## Open

- The exact schema vocabulary and its versioning, written with the first
  host's slots in front of it.
- Version skew between the daemon and a host's `@hive/app`: the daemon
  reports its API version and the SDK refuses to run against one it does
  not know; the policy for how many versions back is not set.
- The shared instance's resource budget on the cluster and per-host caps
  within it (Pia's side; per-install cgroup limits are the floor).
- Whether a host app may be granted `schema.plan` on its org's schema
  through the kit, or keeps its own migrations; assumed its own.
- The builder crate's shape, deferred with the crate.

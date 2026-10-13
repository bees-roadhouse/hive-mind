# D53: Rust is the framework and apps are processes, in TypeScript on Bun or in Python, one container per install on shared compute, speaking to the host over the socket with an install credential and storing through the standard storage interface; the WASM tier is frozen, not extended

**Decided** 2026-10-13 by Nate: "yeah i want real bun. i think we can
simplify the amount of complex code we need if we just use rust as the
framework and typescript/bun & python for the apps. should be able to run
both." And: "they share the compute and use our standard storage library to
store information easily." Shape by Propolis, after one cost was put to him
and he reaffirmed. Builds on the harness that already runs agent containers
(D12.5, D35 §6: a digest-pinned image, `--network=none`, the daemon's unix
socket bind-mounted, the egress proxy, read-only root with tmpfs, cgroup
limits), on D52 (one storage interface on every surface), D47 (the bus),
D48 (vault leases), D44 (app UI in frames), D45 (one API) and D51 (sealed
and served contexts). **Amends D32 §5** (guest languages: TypeScript through
Javy) and **D24's shape** (apps as WASM micro apps behind a JSON ABI): the
WASM tier stays, frozen, and the process tier is where apps are built from
now.

## What Nate is simplifying, said precisely

Today an app is a WASM module on a JSON ABI: the host compiles it, pools warm
instances keyed on the capability set, interrupts it by epoch, tracks taint
per invocation, and offers it five storage verbs through host modules. That
is a lot of host code (`hive-wasmhost`, the ABI, the guest SDK, the guest
build rules in `scripts/guest-build.md`) whose purpose is to make an
untrusted module safe to run in-process. It also means an app is written in
whatever compiles to `wasm32-wasip1`, which for TypeScript means QuickJS
through Javy with no npm and no Node APIs, and for Python means nothing good.

Nate's point: the household's apps are not untrusted modules from strangers.
They are things he and the identities write, in languages they are fast in,
and the platform's real walls are not in the WASM host at all. They are in
the store: one enforcement point (invariant 1), the credential pinning actor
and principal (invariant 2), row-level security (D43), the sealed context
(D51). A process with a credential hits the same walls a guest does. So the
host code that makes in-process execution safe is not what keeps the data
safe, and the apps can be ordinary programs.

## The one cost, stated once

A process cannot be taint-tracked per invocation (invariant 12) or killed
mid-call by an epoch (invariant 7). The container can be killed, and the
host can refuse its next call, but the host cannot know what the process
read before it wrote. So for process apps **trust is per install, set by the
human who installs it**, not per call, and isolation is the container's, not
the instruction pointer's. The record carries that in §4 rather than
pretending the WASM properties carried over. Nate heard it and said build.

## The decision

### 1. An app is a process in a container, one per install, on shared compute

- **An app** is a directory with a `manifest` (the existing one: name,
  version, `storage` with collections and tables, `tools`, `routes`,
  `subscriptions`, `capabilities`, plus a new `runtime: bun | python` and an
  `entry`) and its source. The registry builds it into a digest-pinned image
  (D35 §6's build path: a Containerfile the platform generates from the
  runtime, or one the app supplies) or pulls a reference the manifest names.
- **One container per install**, started by `hive-harness` with the same
  shape as an agent run: the pinned digest, `--network=none`, the daemon's
  socket bind-mounted at `/run/hive-sandbox/api.sock`, the egress proxy for
  the hosts the manifest's `capabilities` name and nothing else, read-only
  root with tmpfs scratch, `--cap-drop all`, cgroup limits (memory, cpus,
  pids) from the install's settings with platform defaults, uid 65532.
  Nothing about the sandbox is the app's to change (D35 §6's rule: the image
  controls userland only).
- **Shared compute.** Containers run on the daemon's nodes alongside agent
  runs and each other, under their cgroup limits; there is no node per app
  and no reservation unless an org sets one. A container is **started on
  first invocation** and **stopped after an idle timeout** the manifest may
  set (default minutes), or marked `always_on` for apps that subscribe to
  events or hold a schedule. Starting and stopping are events; the storage
  page per context (D52 §2) shows each app's container state and resource
  use beside its data.
- **The host dispatches into the app** over a unix socket the app listens
  on inside its container (`/run/app/app.sock`, bind-mounted from the host's
  per-install directory): an HTTP request per invocation with the same JSON
  shapes the ABI used (`{trust, data}` in, `{trust, data}` out), for a tool
  call, a route, a subscription delivery, a cron tick. Invariant 5's
  "guests hold no sockets" is a rule about WASM guests and network sockets;
  a process app holds exactly one listening socket, a unix socket the host
  created, and no network.
- **The app reaches the host** over the daemon's socket (invariant 13) with
  an **install credential**: minted when the container starts, pinned to the
  install's AI-shaped actor acting for the install's owner (invariant 2),
  passed as environment, revoked when the container stops. Every call the
  app makes is a call the client or an agent could make: the same API (D45
  §1), the same `Guard`, the same grants. There is no app-only path.

### 2. The SDKs: TypeScript on Bun, and Python

Two packages, one shape, both maintained in this repository and tested by
the host's suite (D24's rule, the guests test the host, becomes: the SDK's
example app is a checked-in acceptance test of the dispatch and the API):

- `@hive/app` for Bun: `defineApp({ tools, routes, on, cron })` registers
  handlers; `hive.blobs`, `hive.tables`, `hive.documents` are D52's
  interface with the same verbs and query forms; `hive.events`,
  `hive.bus` (D47), `hive.vault.lease` (D48), `hive.models` (D42) wrap the
  rest. Types are generated from the host's API schema so a renamed field
  is a compile error in the app.
- `hive_app` for Python: the same names in Python's shape, `async` first,
  typed with the same generated schema.
- **The storage client is the standard library** Nate asked for: an app
  declares its collections and tables in the manifest, the registry
  provisions them per install (D52 §4), and the SDK's calls are the only way
  the app keeps data. An app that opens its own database, writes files it
  expects to find again, or reaches a store the manifest did not name is a
  broken app, and the container's read-only root and `--network=none` make
  it a loud one.
- **The dev loop:** `hive dev ./my-app` runs the app from its source
  directory against a local daemon with the runtime's own reload (Bun
  `--watch`, a Python reloader), the same manifest and the same dispatch,
  so what works in dev is what runs installed. The daemon is the framework;
  the app is a handler file and a manifest.

### 3. Built-in apps are process apps

The journal, the boards, documents, the things D46 and D47 describe, are
written as Bun apps on `@hive/app`, shipped as images the daemon knows the
digests of, and installed like any other app. That makes the built-ins the
first users of the SDK and the proof that it is enough. It also means **a
single-box install needs Podman**, which the harness already needed for
agent runs; the record says so rather than hiding a new dependency inside
an old one.

### 4. Trust and isolation for a process app

- **Trust is per install.** At install a human sets the app's trust
  (`trusted` for first-party and household apps, `untrusted` default for
  anything else). Every write the app makes carries the install's trust
  level, and every read it makes returns `trust` on each row as today.
  Invariant 9 holds where it always held, at the host: untrusted rows do not
  reach instruction position in a hosted agent run, whichever app wrote
  them. What is lost against the WASM tier is said plainly: an untrusted app
  that reads a trusted row and writes a derived one writes it as
  `untrusted` (its install level), which is right; a trusted app that reads
  an untrusted feed and writes a derived row writes it as `trusted`, which
  the WASM tier would have caught and this tier cannot. Trusted installs are
  therefore the household's own code, and the storage page shows the trust
  level beside every app.
- **Isolation is the container's**: no network, the egress allowlist, the
  read-only root, the cgroup limits, the uid, the per-install credential
  that reaches only what the install's grants allow. A compromised app can
  do what its credential can do and nothing else, which is the same bound
  the client and the agents live under.
- **Killing is the supervisor's**: a deadline per invocation enforced by the
  host closing the request and, on repeated overruns, restarting the
  container; a hung app costs its own container and nothing else.
- **Sealed contexts (D51)**: a process app in a sealed context sees
  ciphertext through the API like every server-side reader, and works on
  content only under an unlock or in served mode. An app that needs content
  says so in its manifest, and the install screen shows it.

### 5. The WASM tier is frozen

`hive-wasmhost`, the ABI, the Rust guest SDK and `apps/hello` stay, tested,
as they are: the per-call sandbox for a module from a stranger, if that case
ever arrives. They are **not extended**: no Javy SDK (#88 is re-pointed at
`@hive/app`), no new capabilities, no new guest language. The ABI
conformance fixture (D31) stays frozen. A later record decides whether the
tier is removed; this one only stops building on it, because two app models
is more code than one and the one Nate wants is the process.

### 6. Where this leaves the earlier records

- **D32 §5** (TypeScript through Javy): superseded; TypeScript runs on Bun
  as a process.
- **D44** (app UI in sandboxed frames): unchanged, and simpler: a process
  app serves its own UI from its container through the app route, framed
  on its per-install origin.
- **D52** (one storage interface): the "guest ABI" surface becomes the
  SDK's client over the API; blobs, tables and documents are unchanged.
- **D47, D48, D42**: each is one more client of the same API, through the
  SDK.
- **The house standard (DevOps page 1921)**: Rust for the host stands; apps
  in TypeScript and Python are the app side, which the standard already
  separated. Pia records the clarification if she wants it on the page.

## Acceptance scenarios, checked in before the code

1. **The hello app in Bun.** `apps/hello-bun` with a tool, a route, a
   subscription and a document collection, installed from source, answers a
   tool call through the dispatch socket with `{trust, data}`, and its
   writes land with the install's trust and owner (invariant 2 pinned by the
   host, never by the app).
2. **The hello app in Python.** The same manifest and the same assertions,
   through `hive_app`.
3. **No network.** From inside the container, a connect to any IP fails;
   a request to the one allowlisted host succeeds through the proxy; a
   request to any other host is refused by the proxy with its message.
4. **The credential is the bound.** The app's `tables.select` on a table
   its install was not granted is refused by the predicate with the grant
   message; a grant makes it succeed; stopping the container revokes the
   credential and the next call with it is the one 401.
5. **Idle stop and first-call start.** A container stops after the idle
   timeout and the next invocation starts it again within a bounded time;
   both are events.
6. **A hung handler costs only itself.** A handler that sleeps past the
   deadline is cut off with the deadline message; the container is restarted
   after N overruns; other installs' calls proceed meanwhile.
7. **Trust is the install's.** An untrusted install's write is `untrusted`
   whatever it read; a trusted install's write is `trusted`; the storage
   page shows the level.
8. **Sealed stays sealed.** A process app in a sealed context cannot read
   content fields without an unlock (D51 scenario 2 through the app path).

## Order

1. **The app runtime**: `runtime` and `entry` in the manifest, the per-install
   container under `hive-harness` with the dispatch socket, the install
   credential minted and revoked with the container, start-on-call and
   idle-stop. Scenarios 3, 4, 5, 6.
2. **`@hive/app`** with generated types, and `apps/hello-bun`. Scenario 1
   and 7. #88 is closed into this.
3. **`hive dev`** against a local daemon.
4. **`hive_app`** for Python and `apps/hello-py`. Scenario 2.
5. **The first built-in in Bun**: the journal (#21), on the SDK, replacing
   the plan to write it as a guest.
6. Scenario 8 when D51's seal lands.

D43 phase 1 slice 3 is unchanged and still first; the runtime lands after
it.

## What lost

- *A Go backend.* A third rewrite in two months, back to wazero; answered
  on 2026-10-13 and not reopened here.
- *Bun inside the host process* (embedding JavaScriptCore). It would put a
  JavaScript engine inside a Rust process whose whole point is to hold no
  native library it does not need, and it would not run Python.
- *Apps as bare processes without containers.* No network boundary, no
  egress allowlist, no read-only root; "trust per install" would then be
  trust in the whole host.
- *Extending the WASM tier to TypeScript and Python.* QuickJS without npm,
  and no Python worth shipping; the thing Nate asked for is real Bun.
- *Removing the WASM tier now.* It costs nothing frozen, and the case it
  serves (a module from a stranger with per-call taint) may return.
- *An app-only API.* Two surfaces for one data model is two enforcement
  points (invariant 1); the SDK wraps the API every client uses.

## Open

- Container start latency for an idle-stopped Bun app, and whether a warm
  pool per runtime is worth its memory; measured, not assumed.
- Whether an app may run more than one container (scale out) and how the
  dispatch picks one; not until an app needs it.
- Where images are built and stored on the cluster (D35 §6 left it to Pia;
  it stays there).
- Python packaging in the generated image (uv is the candidate); criteria:
  reproducible lockfile, no network at runtime, a small image.

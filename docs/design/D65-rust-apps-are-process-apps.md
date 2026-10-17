# D65: an app may be written in Rust: `runtime: rust` is D53's third process runtime, a static binary in the same per-install container under the same sandbox; the SDK is a crate whose wire types are the host's own, moved into a dependency-free protocol crate both sides compile against; no Rust app code ever runs inside the daemon

**Decided** 2026-10-17 by Nate: "can we extend this so the wasm runtime or
rust runtime can run code as native rust too?", and, asked whether he meant
apps written in Rust or Rust code running inside the runtime itself: "apps
written in rust". Shape by Propolis. **Amends D53 §1** (`runtime` gains
`rust`) **and §2** (a third SDK). Leaves D55 §4 (the in-process builder for a
Rust *host*) where it was.

## What was asked, said precisely

D53 made apps processes so the household could write them in the languages
it is fast in, and named two: TypeScript on Bun and Python. Rust is the
language the host, the MCP servers and the guests are already written in, and
some apps want what it gives: CPU-bound work (parsing, media, crypto, search
over local data), a small resident footprint on the arm64 nodes, and a crash
that cannot happen. Today the only way to run Rust as an app is the WASM tier,
which D53 §5 froze.

There were two readings of "native Rust" and only one was asked for:

- **Apps written in Rust**, run like any other process app. This record.
- **Rust app code running inside the daemon** (a plugin trait, `dlopen`, or
  apps linked into `main`). Not asked for, and refused below under *What
  lost*, so the next reader does not have to rediscover why.

## The decision

### 1. `runtime: rust` is a process runtime like the other two

- `AppRuntime` gains `Rust`, and `ALL` becomes three. The manifest is
  otherwise unchanged: the same `process` block (runtime, entry, idle
  timeout, `always_on`, image), the same `storage`, `tools`, `routes`,
  `subscriptions` and `capabilities`.
- **`entry` names the binary**: the path of the built executable relative to
  the app root (`bin/hello`), not a source file. `validate_entry` already
  refuses an absolute path and an escape; nothing about the rule changes,
  only what the path points at.
- **The container is D53 §1's, unchanged**: the pinned digest, the
  launcher's uid mapping, `--network=none`, the daemon's socket and the app
  socket bind-mounted, the egress proxy for the hosts `capabilities` names,
  read-only root with tmpfs, `--cap-drop all`, cgroup limits, start on first
  call and stop when idle. `hive-apps` dispatches into it exactly as it
  dispatches into Bun: `POST /invoke` with `{trust, data}`, `GET /ready`
  before the first call after a start, the deadline, the restart after N
  overruns. **Nothing on the host's side of the socket learns which language
  answered.** That is the test of this record: if a Rust app needs a host
  change other than the enum and the image path, the runtime abstraction is
  wrong and that is the bug.

### 2. The image: built from source, a static binary on a static base

- `docker/app-rust/Containerfile`, one for every Rust app, the shape of
  `docker/app-bun` (the SDK crates from this repository and the one app,
  copied at their paths): a builder stage on the toolchain `rust-toolchain.toml`
  pins (by digest), `cargo build --release --locked` for the
  `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl` target the
  build is asked for, and a runtime stage that copies the one binary to
  `/app/<entry>` on a distroless static base pinned by digest. No shell, no
  package manager, no libc to patch.
- `scripts/app-build.sh` learns the runtime from the manifest and picks the
  Containerfile, and pins the digest into `<app-dir>/image.json` as it does
  for Bun. The registry takes this over when it builds on install (D53 §1),
  as it will for the other two.
- **The image runs as the launcher's uid** (1001 in `hive-harness`), set by
  `USER` in the runtime stage like the Bun image; the static base's own
  default user is not relied on.
- **Building costs what cargo costs.** A Rust app build is a cargo build
  inside Podman and is subject to the desktop's rule like any other (one
  linker at a time, four jobs: `CARGO_BUILD_JOBS=4` is passed in as a build
  argument). On the cluster it is a build job like the Bun one; where images
  are built and stored stays D35 §6's open item, which is Pia's.

### 3. The SDK: `hive-app`, sharing the host's wire types instead of mirroring them

`@hive/app`'s `protocol.ts` says of itself "these types mirror that file
field for field": the wire is defined once in Rust (`hive-apps`'
`protocol.rs`) and copied by hand into TypeScript, with the Rust side's
envelope tests as the contract. A Rust SDK does not have to copy. So:

- **`crates/hive-app-protocol`**: the wire types (`InvokeKind`,
  `InvokeRequest`, `InvokeResponse`, `CallerFacts`, `RouteFacts`, the trust
  word, `INVOKE_PATH`, `READY_PATH`, the environment variable names) move
  out of `hive-apps` into a crate that depends on `serde` and nothing in the
  host. `hive-apps` re-exports them and keeps the conversions to and from
  `Credential` and `hive_trust::Level` on its side. A renamed field is then a
  compile error in the host, the SDK and every Rust app at once, which is
  stronger than the TypeScript mirror, and the TypeScript mirror keeps its
  own test against the same JSON.
- **The protocol crate depending on nothing in the host is a gate check,
  not a convention.** `hive-repodocs` asserts its `[dependencies]` name no
  `hive-*` crate. Without that, the first convenient `use hive_store::...`
  links the store into every app binary. A container with no database
  credential and no network could not use it, but an SDK that compiles the
  store into apps teaches apps that the store is theirs to reach, which is
  invariant 1's failure in slow motion.
- **`crates/hive-app`**, the SDK, is a workspace member: `unsafe_code` is
  denied there as everywhere in the host workspace, and the gate builds and
  lints it. Its shape matches `@hive/app`'s in Rust's idiom:
  - `App::new().tool("add", handler).route(..).on("event", ..).cron(..)`,
    then `.serve().await`, which listens on `HIVE_APP_SOCKET`, answers
    `/ready`, and decodes and encodes the envelope. A handler takes the
    request's `data` as a type it chooses (`serde::Deserialize`) and returns
    `Result<Reply<T>, AppError>`; `Reply` carries the trust word and
    defaults to the input's trust, so a handler that forgets trust lowers
    nothing and raises nothing (the host floors it anyway, invariant 12).
  - `Host` reaches the daemon over `HIVE_SANDBOX_API_SOCKET` with
    `HIVE_APP_CREDENTIAL`, the way `@hive/app`'s does, and `Collection`
    wraps the generated CRUD tools. The API types come from the same
    schema #168 slice 1 serves; the SDK generates them at build time rather
    than depending on the host crates that produce the schema, for the same
    reason the protocol crate depends on nothing.
  - tokio and hyper, the same as the host, so the SDK adds no runtime the
    workspace does not already build.
- **Apps live outside the host workspace**, as `apps/*` already are: an app
  depends on `hive-app` by path inside this repository and by git tag
  outside it. `apps/hello-rust` is the reference app, its own small
  workspace with its own lockfile so the image builds from it alone.
- **`unsafe` inside a Rust app is the app's business.** The host workspace
  denies it because a host with no `unsafe` cannot smuggle a native library
  in; a process app's native code is contained by its container and its
  credential, which is D53 §4's whole point. `apps/hello-rust` forbids it
  anyway, because the reference app sets the example.

### 4. Trust and isolation are D53 §4's, word for word

A Rust app is a process: trust per install set by the human who installs it,
isolation by the container, killing by the supervisor. Memory safety is a
property of the app's code and does not raise its trust, and nothing in the
host treats a Rust app differently from a Bun one. A trusted Rust install that
reads an untrusted feed and writes a derived row still writes it `trusted`,
the same loss D53 §4 stated for every process app.

## Acceptance scenarios, checked in before the code

1. **The hello app in Rust.** `apps/hello-rust` with the same manifest
   surface as `apps/hello-bun` (a tool, a route, a subscription, a document
   collection), built by `scripts/app-build.sh` and run from its pin in the
   container tier: a tool call through the dispatch socket answers
   `{trust, data}`, and its writes land with the install's trust and owner
   (D53 scenario 1).
2. **The host cannot tell.** The `hive-apps` container-tier suite that runs
   against hello-bun runs the same assertions against hello-rust, from one
   table of apps, so a scenario that passes for one and not the other is a
   red test rather than a missing one. D53 scenarios 3 to 7 (no network, the
   credential is the bound, idle stop and first-call start, a hung handler
   costs only itself, trust is the install's) each name both apps.
3. **The wire is one definition.** `hive-app-protocol` builds with no
   `hive-*` dependency (the `hive-repodocs` check, which fails against a
   mutation that adds one); the TypeScript mirror's test decodes the same
   JSON fixtures the Rust envelope tests encode.
4. **The image is what it says.** The built image holds the app's binary at
   `/app/<entry>` and nothing else of the app's (no source, no `target/`),
   runs as uid 1001, and answers `/ready` within the launcher's start
   timeout; the image size and the cold start are recorded in the PR,
   measured, not assumed.
5. **The SDK's trust default holds.** A handler that returns a `Reply`
   without setting trust answers with the input's trust; the host's floor
   test sees `untrusted` in, `untrusted` out, whatever the handler tried.

## Order

1. **The protocol crate**: move the wire types out of `hive-apps`, re-export,
   the `hive-repodocs` check, the shared JSON fixtures. A refactor with no
   behaviour change; scenario 3.
2. **`runtime: rust`** in `hive-manifest`, `docker/app-rust`,
   `scripts/app-build.sh` choosing by runtime. Scenario 4 against a
   fixture binary.
3. **`hive-app`** and `apps/hello-rust`; the container tier's table of apps.
   Scenarios 1, 2 and 5.

It comes after #168 (the API schema, `@hive/app`, the Bun image and
hello-bun), because slice 3 needs the schema and the container tier's
hello-bun test is what scenario 2 turns into a table. It does not wait on
`hive_app` for Python; the two are independent and either may land first.

## What lost

- *Rust app code inside the daemon* (a plugin trait object, `dlopen` of a
  `cdylib`, or apps compiled into `main`). An in-process app sees the
  database credentials and every user's data, so invariant 1's one
  enforcement point becomes a convention every app has to keep; a panic that
  aborts, an `unsafe` block or a leak takes the daemon and every other
  install with it; Rust has no stable ABI for a `dylib` across compiler
  versions; and the host's `unsafe_code = "deny"` exists precisely so
  nothing native is smuggled in. D53 rejected bare processes and D55
  rejected in-process plugins for the same reasons. Speed is not a reason
  either: the dispatch is one request over a unix socket, and anything that
  needs to be in the host is a host capability, written as host code and
  reviewed as host code.
- *Un-freezing the WASM tier for Rust apps.* Rust compiles to
  `wasm32-wasip1` today and the guest SDK exists, but D53 §5 froze the tier
  because two app models is more code than one; adding the most capable
  language to the frozen one would make it the live one again.
- *Mirroring the wire types in Rust the way `protocol.ts` does.* The
  mirror exists because TypeScript cannot compile against Rust; Rust can,
  so a second copy would be drift with no reason.
- *The SDK depending on `hive-apps` directly.* It would compile the
  dispatcher, the store's client types and their dependency tree into every
  app, and invite the reach §3 refuses.
- *A glibc base image.* A static musl binary on a distroless static base
  has nothing to patch and nothing to pin besides the base digest; an app
  that needs a C library it cannot link statically supplies its own
  Containerfile, which D53 §1 already allows.

## Open

- **musl's allocator.** It is slow under contention; an app that measures
  it as its bottleneck can link mimalloc. Whether the SDK's template does so
  by default is measured on hello-rust, not assumed.
- **arm64 images.** The cluster's arm64 nodes want an `aarch64` build;
  whether that is a cross build in one job or a native build on an arm64
  runner is the image-build location question D35 §6 left to Pia.
- **`hive dev` for Rust** (D53 §2's dev loop): `cargo watch` re-running the
  binary against a local daemon is the obvious shape; it comes with
  `hive dev`, not before.

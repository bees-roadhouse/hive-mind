# Development

From nothing to a passing test suite. Assumes you have none of this installed.

## Install

| What                | Why                                          | Linux / macOS                          | Windows                                    |
| ------------------- | -------------------------------------------- | -------------------------------------- | ------------------------------------------ |
| **Rust** via rustup | the daemon; `rust-toolchain.toml` pins 1.98 with clippy and rustfmt, rustup installs it on first `cargo` | https://rustup.rs | `winget install Rustlang.Rustup` |
| **`wasm32-wasip1`** | building guests (`rustup target add wasm32-wasip1`); not needed to run the tests, the built guests are checked in | same | same |
| **Podman 5+**       | the harness, egress and blob-store tiers only (Docker works too); the store is SQLite files and needs nothing | `brew install podman` / your package manager | `winget install RedHat.Podman-Desktop` |
| **Node 20+**        | the Playwright suite only | `brew install node` / nvm | `winget install OpenJS.NodeJS.LTS` |

One PATH note that has already bitten someone: rustup installs to
`~/.cargo/bin`, and a shell opened before the install does not have it. The
scripts prepend it when they find it; a terminal that cannot see `cargo` needs
`export PATH="$HOME/.cargo/bin:$PATH"` or a new shell.

## Clone

```bash
git clone https://github.com/bees-roadhouse/hive-mind
cd hive-mind
cargo fetch
```

## The store

There is nothing to bring up. The store is SQLite (D38): the daemon keeps two
files under `--data-dir` (`hive.db` and `hive-audit.db`), creates them on first
boot and migrates them on every boot, and every integration test makes a
private pair of its own under the temp directory and deletes them on the way
out. `HIVE_SANDBOX_TEST_DB_DIR` moves that directory if the temp directory is
the wrong place (a RAM disk, a slower disk you want to keep off).

`cargo test --workspace` therefore runs every database test on a bare machine.
The tiers that still skip without a backend (Podman, Garage, chromium) print
`SKIPPED: <name> <why>` so the gate can name them.

## Run the gate

```bash
./scripts/gate-rust.sh
```

`cargo fmt --check`, `cargo clippy -D warnings`, `cargo build --all-targets`,
`cargo test --workspace`, then a named list of every test that printed
`SKIPPED:`. It prints `GATE GREEN` or `GATE RED: <steps>`. Nothing has to be
running first.

Read the output, not an exit code. A piped `| tail` or a chained `&&` reports
the status of the last thing in the pipe, which is how a red gate gets pushed.

No toolchain? `./scripts/gate-container.sh` builds a Podman image with Rust,
clippy, rustfmt and the wasm target, and runs the same script inside it.
Anything after `--` runs there in place of the gate:

```bash
./scripts/gate-container.sh -- cargo test -p hive-store --test grants
```

Nothing becomes a red PR ... but read the fleet-desktop rules below before you
reach for the full gate. **On a machine somebody else is using, the full gate
is CI's job and the targeted suites are yours.** These two sections used to
disagree: this one said "run this before you push" and the box rules said
"targeted suites, CI runs the whole gate", and the contradiction was resolved
by whoever read this one first ... four full workspace gates in an evening
while the desktop was swapping and its owner was mid-game. The rule:

- **Nobody else on the box, and you have a linker to yourself:** run
  `./scripts/gate-rust.sh`. It is the best signal available locally.
- **Anyone else on the box, or you cannot tell:** `CARGO_BUILD_JOBS=2`, run the
  suites that cover what you changed (`cargo test -p hive-store --test grants`),
  `cargo clippy -p <crate>` and `cargo fmt --check`, and let CI be the gate.
- Either way, **say in the PR which one you ran.** A reviewer reading "gate
  green" and a reviewer reading "targeted suites green, CI is the gate" should
  not have to guess which they were given.

## Write an integration test

`crates/hive-testdb` hands each test a migrated store of its own: a `Db` for
the main file and one for the override audit file, both deleted when the
`TestDb` drops. No shared fixture and no ordering between tests.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thing() {
    let db = TestDb::new("thing").await;
    let store = Store::from_dbs(db.db().clone(), db.audit().clone());
    ...
}
```

Three things worth knowing before you write a store test:

- **Drop the `TestDb` last.** Its drop deletes the files, and on Windows a file
  with an open handle cannot be deleted. Declare it as the last field of a
  fixture, after every `Store`, `Catalog` or `Bus` that holds a pooled
  connection, so the connections close first.
- **One writer at a time.** A `Transaction` is `BEGIN IMMEDIATE` and holds the
  file's write lock until it commits or drops; a second writer waits (up to the
  busy timeout, ten seconds) rather than failing. A test that opens a
  transaction and then calls a store function on another connection is waiting
  on itself.
- **The override audit is a second file** (`hive-audit.db`), because its row
  has to survive the rollback of the transaction that caused it, and a second
  connection on the same file would wait on the caller's lock forever. Read it
  through `TestDb::audit()`, not `db()`.

`crates/hive-store/tests/invariants.rs` is the reference: the invariant tests
were written against the migrations alone, before any Rust behaviour existed,
which is the tests-first rule of D24 in practice.

## The browser client

There is nothing to build. Pages are rendered by `crates/hive-httpapi` from
`crates/hive-httpapi/templates/`; the stylesheet, the vendored htmx and the two
scripts live in `crates/hive-webui/assets/` and are embedded at compile time.
Edit a template or an asset and `cargo build`; `docs/chat.md` says how the
pieces fit.

## Working on the fleet desktop (trh-lib-dsk001)

Rules that were paid for, so they are in git rather than in one profile's
memory:

- Rust lives at `~/.cargo/bin`, which a Claude shell does not have on `PATH`.
  `export PATH="$HOME/.cargo/bin:$PATH"` first.
- There is no test database to start any more (D38): the tests write their
  store files under the temp directory. The `hive-sandbox-pg-rust` container
  from the Postgres era can be removed.
- **The box dies on disk, not CPU.** Three sessions linking Rust at once on the
  single LUKS NVMe froze the desktop with the CPU half idle. Pinning cargo to
  four cores was the wrong dimension. The rule: one cargo at a time across
  every session, `-j 4` (or `CARGO_BUILD_JOBS=4`), `pgrep -x rust-lld` before
  starting and wait if another session is linking, and targeted suites
  (`cargo test -p crate --test file`) rather than `--workspace` loops; CI runs
  the whole gate.
- **A person uses this machine.** It is Nate's desktop, not a builder. The
  failure is not slowness: 60 GB of RAM across five agent sessions, a game and
  a browser overflows into an 8 GB swapfile on the NVMe, and what he sees is
  his video stuttering on swap-in stalls while the CPU sits at 0.1% pressure.
  Check before a long build ... `free -g` and `/proc/loadavg`, and if swap is
  near full, do not start. Stop your own containers when you are not using
  them; a test database you left up for five hours is 240 MB of somebody else's
  video.
- Keep the whole output of a long run in a file and grep it afterwards. A
  `| tail` on the gate threw away the one failure and cost a rerun.
- `pkill -f` with a pattern that appears in your own command line kills the
  Claude shell itself (exit 144). Kill by pid.
- No chromium here, so the e2e suite is typechecked locally and run by CI.

## Run the e2e tests

```bash
cd test/e2e
npm install
npm run browsers    # one-time chromium download, ~115 MB
npm test
```

The suite builds the daemon (`cargo build -p hive-sandbox`), starts it on an
ephemeral port per worker, and shuts it down after. Nothing to start by hand and
no fixed port to collide with. `HIVE_SANDBOX_E2E_BINARY` points it at a binary
built elsewhere.

It needs nothing else running. Every worker gives its daemon a store directory
of its own under the temp directory and deletes it afterwards; the specs write
events into that file with `node:sqlite`, so there is no native module to
build either.

Debugging:

```bash
npx playwright test --headed          # watch it
npx playwright test --debug           # step through
npm run report                        # last HTML report
npm run typecheck                     # tsc --noEmit
```

`test/e2e/README.md` covers the fixtures and how to write an SSE spec.

## Build the guests

```bash
rustup target add wasm32-wasip1
./scripts/build-guests.sh
```

Writes `crates/hive-wasmhost/testdata/<app>.wasm` for each app under `apps/`.
The built files are committed; CI rebuilds them and refuses a diff. The
profile every guest builds with is explained in `scripts/guest-build.md`.

## Build the agent harness images

Optional. Only needed to run an agent, or to exercise the harness container
tests ... everything else skips without them, by name.

```bash
./scripts/harness-build.sh
```

Three tags off one Containerfile under rootless Podman, taking a few minutes the
first time and seconds after. See [`harness.md`](harness.md) for the isolation
defaults, the network modes and the run-record seam.

A run that needs the internet also needs the egress proxy image:

```bash
./scripts/egress-build.sh
```

## Build the daemon image

```bash
./scripts/image-build.sh
```

A Rust builder stage over the whole workspace, a `distroless/cc` runtime with
no shell. The script reads the version back out of the image and refuses a
mismatch, because a pin that names a version the binary does not report is a
lie that gets discovered during an incident.

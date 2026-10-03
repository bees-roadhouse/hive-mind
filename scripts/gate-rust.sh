#!/usr/bin/env bash
# The born-green gate. Run before pushing; read the OUTPUT, never the exit code
# of a piped command.
#
#   ./scripts/gate-rust.sh
#
# fmt, clippy, build, test, and a NAMED list of every test that skipped, because
# a skip is a test saying it is not answering the question and that only helps
# if somebody hears it.
#
# Nothing has to be running first. The store is SQLite (D38) and every
# database test makes its own files under the temp directory; the tiers that
# still need a backend (Podman, chromium) skip by name without one.
set -uo pipefail
cd "$(dirname "$0")/.."

# rustup installs to ~/.cargo/bin and a fresh shell does not always have it.
if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found. rustup.rs installs it; rust-toolchain.toml pins the version." >&2
  exit 1
fi
failed=()
step() {
  local name=$1; shift
  echo "==> $name"
  "$@" || failed+=("$name")
}

step fmt cargo fmt --all -- --check
step clippy cargo clippy --workspace --all-targets -- -D warnings
step build cargo build --workspace --all-targets

echo "==> test"
log=$(mktemp)
# --nocapture so a SKIPPED: line reaches this script; the test harness would
# otherwise swallow it along with everything else a passing test printed.
# --no-fail-fast so one red crate does not hide the others' results: the point
# of a gate is the whole picture, not the first thing that broke.
cargo test --workspace --no-fail-fast -- --nocapture 2>&1 | tee "$log"
test_status=${PIPESTATUS[0]}
[ "$test_status" -eq 0 ] || failed+=("test")

skipped=$(grep -E '^SKIPPED: ' "$log" | sed -E 's/^SKIPPED: ([^ ]+).*/  \1/' | sort -u)
rm -f "$log"
if [ -n "$skipped" ]; then
  echo
  echo "SKIPPED ($(echo "$skipped" | wc -l)) ... these did NOT run:"
  echo "$skipped"
fi

echo
if [ ${#failed[@]} -ne 0 ]; then
  echo "GATE RED: ${failed[*]}"
  exit 1
fi
echo "GATE GREEN"

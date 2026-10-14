#!/usr/bin/env bash
# Builds the container tier's test app image (docker/app-test) and pins its
# digest into docker/app-test/digest.json, the way scripts/egress-build.sh
# pins the proxy. The image is a fixture: the smallest process that speaks
# hive-apps' protocol over the socket the host mounts (D53, #163 slice 4).
# CI's containers job runs this before `cargo test -p hive-apps --test podman`.
set -uo pipefail
cd "$(dirname "$0")/.."
export PODMAN_COMPOSE_WARNING_LOGS=false

containerfile="docker/app-test/Containerfile"
lockfile="docker/app-test/digest.json"
repo="localhost/hive-sandbox-app-test"
tag="${repo}:latest"

if ! command -v podman >/dev/null 2>&1; then
  echo "podman not found. App containers run under rootless Podman (D53)." >&2
  exit 1
fi

version=$(git describe --tags --always --dirty 2>/dev/null || echo dev)
echo "==> building $tag (version $version)"
if ! podman build --tag "$tag" -f "$containerfile" .; then
  echo "build failed" >&2
  exit 1
fi

digest=$(podman image inspect "$tag" --format '{{.Digest}}' 2>/dev/null)
if [ -z "$digest" ]; then
  echo "could not read a digest for $tag" >&2
  exit 1
fi

cat >"$lockfile" <<JSON
{
  "repository": "${repo}",
  "digest": "${digest}",
  "version": "${version}"
}
JSON

echo "    app-test  $digest  ($version)"
echo
echo "APP TEST IMAGE BUILT"
echo "  pin recorded in $lockfile"

#!/usr/bin/env bash
# Builds the image for one `runtime: bun` app from docker/app-bun/Containerfile
# and pins its digest into <app-dir>/image.json (D53 §1, #168 slice 3). The
# registry will do this on install; until then this script is the build path,
# and CI's containers job runs it for apps/hello-bun before the container
# tier's test.
#
#   scripts/app-build.sh apps/hello-bun
set -uo pipefail
cd "$(dirname "$0")/.."
export PODMAN_COMPOSE_WARNING_LOGS=false

app_dir="${1:-}"
if [ -z "$app_dir" ] || [ ! -f "$app_dir/manifest.json" ]; then
  echo "usage: scripts/app-build.sh <app-dir with a manifest.json>" >&2
  exit 1
fi
slug=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["name"])' "$app_dir/manifest.json" 2>/dev/null \
  || bun -e 'console.log(JSON.parse(await Bun.file(process.argv[1]).text()).name)' "$app_dir/manifest.json")
if [ -z "$slug" ]; then
  echo "could not read the app's name from $app_dir/manifest.json" >&2
  exit 1
fi

containerfile="docker/app-bun/Containerfile"
lockfile="$app_dir/image.json"
repo="localhost/hive-sandbox-app-${slug//_/-}"
tag="${repo}:latest"

if ! command -v podman >/dev/null 2>&1; then
  echo "podman not found. App containers run under rootless Podman (D53)." >&2
  exit 1
fi

version=$(git describe --tags --always --dirty 2>/dev/null || echo dev)
echo "==> building $tag from $app_dir (version $version)"
if ! podman build --tag "$tag" --build-arg "APP_DIR=$app_dir" -f "$containerfile" .; then
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

echo "    $slug  $digest  ($version)"
echo
echo "APP IMAGE BUILT"
echo "  pin recorded in $lockfile"

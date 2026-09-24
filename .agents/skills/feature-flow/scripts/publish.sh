#!/usr/bin/env bash
# Publish every SCV crate at the workspace version, in AGENTS.md dependency
# order. Crates already on crates.io are skipped, so an interrupted run can be
# repeated. `--check` only reports which crates still need publishing.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ -z "${FEATURE_FLOW_HOST:-}" ]; then
  exec env FEATURE_FLOW_HOST=1 "$here/host.sh" "$here/$(basename "$0")" "$@"
fi

check_only=false
case "${1:-}" in
  "") ;;
  --check) check_only=true ;;
  *) echo "usage: publish.sh [--check]" >&2; exit 2 ;;
esac

crates=(scv-core scv-protocol scv-client scv-provider-openai scv-tools
        scv-channels scv-clawbot scv-server scv-tui scv-cli)

cd "$(git rev-parse --show-toplevel)"
version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
if [ -z "$version" ]; then
  echo "publish.sh: cannot read [workspace.package] version" >&2
  exit 1
fi

published() {
  local status
  status=$(curl -sS -o /dev/null -w '%{http_code}' -A 'scv-feature-flow (publish.sh)' \
    "https://crates.io/api/v1/crates/$1/$version")
  case "$status" in
    200) return 0 ;;
    404) return 1 ;;
    *) echo "publish.sh: crates.io lookup for $1 returned HTTP $status" >&2; exit 1 ;;
  esac
}

pending=()
for crate in "${crates[@]}"; do
  if published "$crate"; then
    echo "  $crate $version: on crates.io"
  else
    echo "  $crate $version: not published"
    pending+=("$crate")
  fi
done
if $check_only; then
  exit 0
fi
if [ "${#pending[@]}" -eq 0 ]; then
  echo "All crates are already published at $version."
  exit 0
fi

# Publish only what has landed: a clean tree exactly at origin/main.
git fetch --quiet origin
if [ -n "$(git status --porcelain)" ]; then
  echo "publish.sh: the working tree has uncommitted changes" >&2
  exit 1
fi
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
  echo "publish.sh: HEAD is not origin/main; land first, then run: git switch --detach origin/main" >&2
  exit 1
fi

for crate in "${pending[@]}"; do
  echo "== publishing $crate $version"
  # Cargo waits until each version is visible in the index before returning,
  # so dependents never race their dependencies.
  cargo publish --locked -p "$crate"
done
echo "Published ${#pending[@]} crate(s) at $version."

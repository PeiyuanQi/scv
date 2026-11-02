#!/usr/bin/env bash
# Publish every SCV crate at the workspace version, in AGENTS.md dependency
# order. Crates already on crates.io are skipped, so an interrupted run can be
# repeated. `--check` only reports which crates still need publishing.
#
# Publishing cannot be undone. Before the first `cargo publish`, the whole
# release goes through SCV's publish rule (`scv policy check publish`): no
# question unless a crate would pass `[policy] publish_crate_versions_per_day`
# (20) versions in the last 24 hours on crates.io. Past it, or when a count
# cannot be fetched, it asks the owner yes or no in chat (the chat that
# started the work, or the `[notify]` owner chat) and publishes only on yes.
# A crate already at crates.io's own limit of 20 stops the release unasked.
# SCV_CONFIRM_TIMEOUT sets how many seconds no answer waits before it counts
# as no (default 1800). An installed `scv` without `scv policy` falls back to
# asking with `scv confirm` whenever an agent SCV delegated to (SCV_PARENT is
# set) publishes, so the first release that carries the rule is not published
# unasked.
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
        scv-channels scv-server scv-tui scv-cli)

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

# The publish rule decides; past the daily limit it asks the owner.
if "$here/host.sh" scv policy --help >/dev/null 2>&1; then
  set +e
  "$here/host.sh" scv policy check publish --version "$version" "${pending[@]}"
  answer=$?
  set -e
  case $answer in
    0) ;;
    1)
      echo "publish.sh: the publish rule or the owner said no; nothing was published" >&2
      exit 1
      ;;
    *)
      cat >&2 <<EOF
publish.sh: the publish rule could not decide or could not ask the owner
(scv policy exited $answer, for the reason above); nothing was published. Tell
the owner why. Do not publish around the rule.
EOF
      exit 1
      ;;
  esac
elif [ -n "${SCV_PARENT:-}" ]; then
  # An `scv` that predates the rule: an agent SCV delegated to publishes only
  # on the owner's yes in chat.
  if ! "$here/host.sh" scv confirm --help >/dev/null 2>&1; then
    installed=$("$here/host.sh" scv --version 2>/dev/null || echo "scv of an unknown version")
    cat >&2 <<EOF
publish.sh: the installed $installed cannot ask the owner in chat: \`scv confirm\`
arrived in 0.3.0, so nothing was published. Publish this release from a
terminal: run scripts/publish.sh there, outside SCV. Tell the owner; do not
publish around the question.
EOF
    exit 1
  fi
  confirm_timeout=${SCV_CONFIRM_TIMEOUT:-1800}
  names=$(printf '%s, ' "${pending[@]}")
  question="Publish SCV $version to crates.io from origin/main $(git log -1 --format='%h %s' HEAD)?
Crates: ${names%, }.
Publishing cannot be undone."
  echo "Asking the owner in chat before publishing; no answer in ${confirm_timeout}s counts as no."
  set +e
  "$here/host.sh" scv confirm --timeout "$confirm_timeout" -- "$question"
  answer=$?
  set -e
  case $answer in
    0) ;;
    1)
      echo "publish.sh: the owner did not say yes; nothing was published" >&2
      exit 1
      ;;
    *)
      cat >&2 <<EOF
publish.sh: could not ask the owner (scv confirm exited $answer, for the reason
above); nothing was published. If the running daemon is too old to ask (before
0.3.0), publish this release from a terminal; otherwise tell the owner why.
Do not publish around the question.
EOF
      exit 1
      ;;
  esac
fi

for crate in "${pending[@]}"; do
  echo "== publishing $crate $version"
  # Cargo waits until each version is visible in the index before returning,
  # so dependents never race their dependencies.
  cargo publish --locked -p "$crate"
done
echo "Published ${#pending[@]} crate(s) at $version."

#!/usr/bin/env bash
# Install a published scv-cli release and restart the local SCV daemon into it.
#
# The daemon restarts itself when it is safe (`scv restart --when-idle`): once
# the delegation running this script, if any, has finished and its report is
# stored, and no owner message is being answered, or at the latest after
# SCV_RESTART_MAX_WAIT seconds. A watchdog outside the daemon restarts the
# unit, checks the new release, and puts the previous binary back if it fails.
# The new daemon announces the outcome in the chat that asked, or through the
# `[notify]` accounts. A daemon too old for that is restarted by this script
# instead: from inside the daemon, 60 seconds later through systemd-run.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ -z "${FEATURE_FLOW_HOST:-}" ]; then
  exec env FEATURE_FLOW_HOST=1 "$here/host.sh" "$here/$(basename "$0")" "$@"
fi

version=${1:?usage: deploy.sh <version>}
unit=${SCV_UNIT:-scv.service}
restart_delay=${SCV_RESTART_DELAY:-60}
max_wait=${SCV_RESTART_MAX_WAIT:-600}
inside=false
if grep -q "/$unit" /proc/self/cgroup; then
  inside=true
fi

# Keep the running release beside the new one, for a rollback by hand or by
# the watchdog (which refreshes this copy from the running daemon).
if bin=$(command -v scv) && [ -x "$bin" ]; then
  cp -p "$bin" "$bin.prev.tmp" && mv -f "$bin.prev.tmp" "$bin.prev"
fi

# A fresh publish can take a little while to reach the index.
for attempt in 1 2 3 4 5 6; do
  if cargo install scv-cli --version "$version" --locked; then
    break
  fi
  if [ "$attempt" -eq 6 ]; then
    echo "deploy.sh: could not install scv-cli $version" >&2
    exit 1
  fi
  echo "deploy.sh: install attempt $attempt failed; retrying in 20s" >&2
  sleep 20
done
installed=$(scv --version | awk '{print $2}')
if [ "$installed" != "$version" ]; then
  echo "deploy.sh: scv on PATH reports $installed, expected $version" >&2
  exit 1
fi
echo "Installed scv $installed."

# The commit the release was packaged from, for the announcement. The
# checkout running this script can be at another commit, so read what Cargo
# recorded in the published crate; without it, announce no commit.
commit=
for info in "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/"scv-cli-$version"/.cargo_vcs_info.json; do
  [ -f "$info" ] || continue
  commit=$(sed -n 's/.*"sha1": *"\([0-9a-f]\{7\}\)[0-9a-f]*".*/\1/p' "$info")
  [ -n "$commit" ] && break
done

started=$(date +%s)
set +e
scv restart --when-idle --version "$version" ${commit:+--commit "$commit"} --max-wait "$max_wait"
requested=$?
set -e
case $requested in
  0)
    if $inside; then
      echo "The daemon restarts into $version once this job's report is delivered (at most ${max_wait}s),"
      echo "then announces the outcome in the chat that asked. Finish cleanup and your report now."
      exit 0
    fi
    wait_seconds=$((max_wait + 240))
    ;;
  3)
    if $inside; then
      systemd-run --user --quiet --collect --on-active="$restart_delay" \
        --unit="scv-deploy-restart-$$" systemctl --user restart "$unit"
      echo "Running inside $unit: its restart is scheduled in ${restart_delay}s."
      echo "Afterwards check: scv status; scv channels status"
      exit 0
    fi
    systemctl --user restart "$unit"
    wait_seconds=60
    ;;
  *)
    echo "deploy.sh: the daemon did not schedule its restart; it still runs the old release" >&2
    exit 1
    ;;
esac
echo "Waiting for scv $version and connected accounts..."

# Capture status output before matching: an early-closing reader in a
# pipefail pipeline would turn SIGPIPE into a false result.
status() { scv status 2>/dev/null || true; }

daemon_ready=false
for _ in $(seq 1 $((wait_seconds / 2))); do
  if [[ $(status) == *"version $version,"* ]]; then
    daemon_ready=true
    break
  fi
  sleep 2
done
if ! $daemon_ready; then
  scv status || true
  echo "deploy.sh: the daemon did not report version $version" >&2
  echo "The restart watchdog logs to: journalctl --user -u 'scv-update-*'" >&2
  exit 1
fi

# Enabled channel accounts need one long poll (up to ~35s) to connect.
# `scv status` summarizes them as "Channels: <connected> of <enabled> ...".
connected=false
for _ in $(seq 1 40); do
  if [[ $(status) =~ Channels:\ ([0-9]+)\ of\ ([0-9]+)\  ]] &&
    [ "${BASH_REMATCH[1]}" = "${BASH_REMATCH[2]}" ]; then
    connected=true
    break
  fi
  sleep 3
done
scv status
if ! $connected; then
  echo "deploy.sh: an enabled component has not connected yet; check 'scv channels status'" >&2
fi
# Releases before 0.2.1 coloured their log lines; strip escape codes first.
warnings=$(journalctl --user -u "$unit" --since "@$started" --no-pager -o cat 2>/dev/null |
  sed -E 's/\x1b\[[0-9;]*m//g' | grep -E ' (WARN|ERROR) ' | tail -20 || true)
if [ -n "$warnings" ]; then
  echo "Journal warnings since restart:"
  echo "$warnings"
fi
$connected

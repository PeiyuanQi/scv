#!/usr/bin/env bash
# Install a published scv-cli release and restart the local SCV daemon.
#
# From inside the daemon (an SCV-delegated agent), an immediate restart would
# kill this process and its turn, so the restart is scheduled outside the
# daemon instead and verification is left to the user.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ -z "${FEATURE_FLOW_HOST:-}" ]; then
  exec env FEATURE_FLOW_HOST=1 "$here/host.sh" "$here/$(basename "$0")" "$@"
fi

version=${1:?usage: deploy.sh <version>}
unit=${SCV_UNIT:-scv.service}
restart_delay=${SCV_RESTART_DELAY:-60}

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

if grep -q "/$unit" /proc/self/cgroup; then
  systemd-run --user --quiet --collect --on-active="$restart_delay" \
    --unit="scv-deploy-restart-$$" systemctl --user restart "$unit"
  echo "Running inside $unit: its restart is scheduled in ${restart_delay}s."
  echo "Afterwards check: scv status; scv channels status"
  exit 0
fi

started=$(date +%s)
systemctl --user restart "$unit"
echo "Restarted $unit; waiting for scv $version and connected accounts..."

# Capture status output before matching: an early-closing reader in a
# pipefail pipeline would turn SIGPIPE into a false result.
status() { scv status 2>/dev/null || true; }

daemon_ready=false
for _ in $(seq 1 30); do
  if [[ $(status) == *"version $version,"* ]]; then
    daemon_ready=true
    break
  fi
  sleep 2
done
if ! $daemon_ready; then
  scv status || true
  echo "deploy.sh: the daemon did not report version $version" >&2
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
warnings=$(journalctl --user -u "$unit" --since "@$started" --no-pager -o cat 2>/dev/null |
  grep -E ' (WARN|ERROR) ' | tail -20 || true)
if [ -n "$warnings" ]; then
  echo "Journal warnings since restart:"
  echo "$warnings"
fi
$connected

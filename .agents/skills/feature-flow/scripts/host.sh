#!/usr/bin/env bash
# Run a command with the user's real home, even from an SCV-delegated agent.
#
# SCV starts delegated agents with HOME, XDG_* and SCV_HOME pointing at a
# private agent home (<scv-home>/agents/<agent>). Git identity and SSH
# keys, gh, rustup/cargo, the crates.io token, and the daemon socket all live
# under the real home, so landing steps run through this wrapper. Outside an
# SCV agent home it passes the command through unchanged.
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: host.sh <command> [args...]" >&2
  exit 2
fi

case "${SCV_HOME:-}" in
  */agents/*) ;;
  *) exec "$@" ;;
esac

real_home=$(getent passwd "$(id -un)" | cut -d: -f6)
if [ -z "$real_home" ] || [ ! -d "$real_home" ]; then
  echo "host.sh: cannot resolve the real home directory" >&2
  exit 1
fi

# The daemon's own instance home is the parent of agents/. The default
# instance runs without SCV_HOME, which also keeps its unit name scv.service.
instance=${SCV_HOME%/agents/*}
unset SCV_HOME CODEX_HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME
if [ "$instance" != "$real_home/.scv" ]; then
  export SCV_HOME="$instance"
fi
export HOME="$real_home"
export PATH="$real_home/.cargo/bin:$real_home/.local/bin:${PATH:-/usr/local/bin:/usr/bin:/bin}"
exec "$@"

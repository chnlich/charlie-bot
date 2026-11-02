#!/usr/bin/env bash
# Prints the local work guard's refusal for a toolchain run on this host, and exits 97. Every refusal entry point calls
# this script, so the refusal text has one owner.
#
#   refuse.sh <tool>
set -euo pipefail

if (( $# != 1 )); then
  printf 'usage: %s <tool>\n' "$0" >&2
  exit 2
fi
tool=$1
here=$(cd -P -- "$(dirname -- "$0")" && pwd)
checkout=$(cd -P -- "$here/../.." && pwd)
# The build host is read from config.yaml at each refusal. While remote_run.host is unset, the text names
# the key, so the refusal still prints in full; the runner it points to reports the CLI's error itself.
if ! host=$(charliebot config get remote_run.host 2>/dev/null) || [[ -z "$host" ]]; then
  host='the build host (set remote_run.host in config.yaml)'
fi
printf 'refused: %s compiles or runs tests on this host.\n' "$tool" >&2
printf 'Agents run that work on %s, so one run does not slow every other agent.\n' "$host" >&2
printf 'Run the same command from this directory through %s/dev/remote-run/run.sh (Cargo: cargo-slot.sh beside it).\n' \
  "$checkout" >&2
printf 'When the runner exits 86 or 87, end this run and report the blocker with the link it prints.\n' >&2
exit 97

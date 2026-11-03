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
host=${REMOTE_RUN_HOST:?the local work guard sets REMOTE_RUN_HOST, which the refusal names}
printf 'refused: %s compiles or runs tests on this host.\n' "$tool" >&2
printf 'Agents run that work on %s, so one run does not slow every other agent.\n' "$host" >&2
printf 'Run the same command from this directory through %s/dev/remote-run/run.sh (Cargo: cargo-slot.sh beside it).\n' \
  "$checkout" >&2
printf 'When the runner exits 86 or 87, end this run and report the blocker with the link it prints.\n' >&2
exit 97

#!/usr/bin/env bash
set -euo pipefail

prefix=/tmp/charliebot-script-
leaked=""
for entry in /proc/[0-9]*/stat; do
  pid=${entry#/proc/}
  pid=${pid%/stat}
  # A process can exit between the glob and the read; its entry then fails to parse and it cannot be a leak this
  # check reports, so the scan skips it.
  ppid=$(sed 's/^.*) //' "$entry" 2>/dev/null | cut -d' ' -f2) || continue
  [ "$ppid" = "1" ] || continue
  if tr '\0' '\n' < "/proc/$pid/cmdline" 2>/dev/null | grep -q "^${prefix}"; then
    leaked="$leaked $pid"
  fi
done
if [ -n "$leaked" ]; then
  echo "leaked script-test processes:$leaked" >&2
  for pid in $leaked; do
    printf '  %s: %s\n' "$pid" "$(tr '\0' ' ' < "/proc/$pid/cmdline")" >&2
  done
  exit 1
fi

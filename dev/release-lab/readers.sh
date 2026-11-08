#!/usr/bin/env bash
# Background disk readers for the release lab: a fixed number of processes that keep the root disk busy while a
# measurement runs.
#
#   readers.sh <file> <count> <seconds> [sleep_ms]
#
# Every read is one 1 MiB block through O_DIRECT, at a rotating offset, so no reader is served from the page cache and
# the reads land in /proc/diskstats where the switch's disk gate samples them. Each reader sleeps SLEEP_MS between
# blocks, which holds its rate near 1000/SLEEP_MS MB/s: 5 ms gives about 200 MB/s, well above the gate's 40 MB/s
# threshold. The readers stop after SECONDS, or on SIGTERM/SIGINT, and no reader outlives this script.
set -uo pipefail

if [ $# -lt 3 ] || [ $# -gt 4 ]; then
  echo "usage: readers.sh <file> <count> <seconds> [sleep_ms]" >&2
  exit 2
fi
FILE=$1
COUNT=$2
SECONDS_TO_READ=$3
SLEEP_MS=${4:-5}
[[ -f $FILE ]] || {
  echo "readers.sh: no file at $FILE" >&2
  exit 2
}
[[ $COUNT =~ ^[0-9]+$ && $SECONDS_TO_READ =~ ^[0-9]+$ && $SLEEP_MS =~ ^[0-9]+$ ]] || {
  echo "readers.sh: count, seconds and sleep_ms are whole numbers" >&2
  exit 2
}

SECONDS_TO_READ=${SECONDS_TO_READ:-0}
BLOCKS=$(( $(stat -c %s "$FILE") / 1048576 ))
if (( BLOCKS < 1 )); then
  echo "readers.sh: $FILE is smaller than one 1 MiB block" >&2
  exit 2
fi
DELAY=$(awk -v ms="$SLEEP_MS" 'BEGIN { printf "%.3f\n", ms / 1000 }')
PIDS=()

stop() {
  local pid
  trap - TERM INT EXIT
  for pid in "${PIDS[@]}"; do
    kill -TERM "$pid" 2>/dev/null || true
  done
  for pid in "${PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  exit 0
}
trap stop TERM INT

# reader INDEX: one reader's block loop. The offset rotates through the file from a per-reader start, so readers cover
# different blocks instead of repeating one cached block.
reader() {
  local index=$1 block=$(( ($1 * 977) % BLOCKS ))
  while :; do
    dd if="$FILE" of=/dev/null iflag=direct bs=1M count=1 skip="$block" status=none 2>/dev/null || break
    block=$(( (block + 1) % BLOCKS ))
    sleep "$DELAY"
  done
}

for ((index = 0; index < COUNT; index++)); do
  reader "$index" &
  PIDS+=("$!")
done
printf 'readers.sh: %s reader(s) of %s for %s s\n' "$COUNT" "$FILE" "$SECONDS_TO_READ" >&2
sleep "$SECONDS_TO_READ"
stop

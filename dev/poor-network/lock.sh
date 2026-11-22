#!/usr/bin/env bash
# The host's bench lock: every bench run on a host holds this one file around each side attempt, so two runs never
# measure side by side. This file is the only place that names the file and calls flock on it.
#
# The holders are the poor-network bench's sides (run.sh), the remote-wake bench's container run
# (dev/remote-wake/lock.sh) and each improve trial side that holds the lock (dev/improve/trial-side.sh).
#
# A side attempt is one trial's start, measurement and stop, not the run's builds: a run builds before it queues, and a
# run with two sides takes the lock once per side. A side that cannot take the lock within its wait exits with
# BENCH_LOCK_TIMEOUT_EXIT, the status flock uses when its wait runs out.
#
# Sourced by run.sh and the other holders; a test sources it to drive the lock on its own.

readonly BENCH_LOCK_FILE="$HOME/pn-runs/.bench.lock"
# The bench's own wait. A holder with another wait passes it to bench_lock_within.
readonly BENCH_LOCK_WAIT_SECONDS=7200
# The status flock uses when the wait runs out.
readonly BENCH_LOCK_TIMEOUT_EXIT=75

# bench_lock_within <wait seconds> <command> [args...]: exec the command under the bench lock, waiting up to the wait.
bench_lock_within() {
  local wait_seconds=$1
  shift
  # The bench creates the lock's directory, so the lock is the bench's own and no caller prepares it.
  mkdir -p "$(dirname "$BENCH_LOCK_FILE")" || return $?
  # -o keeps the lock in flock's own process, so no process of the command holds it past the command's end.
  exec flock -o -w "$wait_seconds" -E "$BENCH_LOCK_TIMEOUT_EXIT" "$BENCH_LOCK_FILE" "$@"
}

# bench_lock <command> [args...]: exec the command under the bench lock, waiting up to BENCH_LOCK_WAIT_SECONDS for it.
bench_lock() {
  bench_lock_within "$BENCH_LOCK_WAIT_SECONDS" "$@"
}

# bench_lock_hold <wait seconds>: hold the bench lock on this shell's file descriptor 9 until the shell exits, so every
# process the shell starts lives under it. Returns flock's status: BENCH_LOCK_TIMEOUT_EXIT when the wait runs out.
bench_lock_hold() {
  mkdir -p "$(dirname "$BENCH_LOCK_FILE")" || return $?
  exec 9>"$BENCH_LOCK_FILE"
  flock -w "$1" -E "$BENCH_LOCK_TIMEOUT_EXIT" 9
}

# bench_lock_holder: print the holder of the bench lock as `<pid> <command line>`, or print nothing when no process
# holds it. The holder is the process that took the lock, so its command line names the work that holds it. A
# waiting process is listed with `->` in /proc/locks and is not the holder. The lock file is read for its device and
# inode only, and the lock is never taken or released here.
bench_lock_holder() {
  [[ -e $BENCH_LOCK_FILE ]] || return 0
  local device inode major minor _id kind rest pid device_inode dev_major dev_minor dev_inode cmdline
  device=$(stat -c '%D' -- "$BENCH_LOCK_FILE") || return $?
  inode=$(stat -c '%i' -- "$BENCH_LOCK_FILE") || return $?
  # st_dev packs the major and minor the way glibc's makedev does; /proc/locks prints them in hex.
  device=$((16#$device))
  major=$(( ((device >> 8) & 0xfff) | ((device >> 32) & ~0xfff) ))
  minor=$(( (device & 0xff) | ((device >> 12) & ~0xff) ))
  while read -r _id kind rest; do
    [[ $kind == FLOCK ]] || continue
    read -r _advisory _mode pid device_inode _ <<<"$rest"
    IFS=: read -r dev_major dev_minor dev_inode <<<"$device_inode"
    if (( 16#$dev_major == major && 16#$dev_minor == minor )) && [[ $dev_inode == "$inode" ]]; then
      if ! cmdline=$(exec 2>/dev/null; tr '\0' ' ' <"/proc/$pid/cmdline"); then
        cmdline='(exited before its command line was read)'
      fi
      printf '%s %s\n' "$pid" "${cmdline% }"
      return 0
    fi
  done </proc/locks || return $?
  return 0
}

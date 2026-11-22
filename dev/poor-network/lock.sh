#!/usr/bin/env bash
# The host's bench lock: every bench run on a host holds this one file while it measures, so two runs never measure side
# by side. This file is the only place that names the file and calls flock on it.
#
# The holders are the poor-network bench (bench.mjs, which takes the lock for one step at a time), the remote-wake bench's
# container run (dev/remote-wake/lock.sh) and each improve trial side that holds the lock (dev/improve/trial-side.sh).
#
# A holder that owns its descriptor (bench.mjs) takes the lock with bench_lock_fd_take and releases it with
# bench_lock_fd_release, and it finds the waiters with bench_lock_waiter_pids. A holder with a fixed hold (run.sh's
# callers, the improve sides) runs its command under bench_lock_within, which is the same lock.
#
# A side attempt that cannot take the lock within its wait exits with BENCH_LOCK_TIMEOUT_EXIT, the status flock uses when
# its wait runs out.
#
# Sourced by run.sh, bench.mjs and the other holders; a test sources it to drive the lock on its own.

# The host's lock file. A run exports the file it resolved (bench_lock_export): the bench runs inside a trial, and the
# trial replaces HOME with its own, so the default below would name a private lock that no other holder shares.
readonly BENCH_LOCK_FILE="${BENCH_LOCK_FILE:-$HOME/pn-runs/.bench.lock}"
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

# bench_lock_path: print the lock file's path, creating its directory. A holder that opens the file itself (bench.mjs)
# takes the path from here, so it never names the file.
bench_lock_path() {
  mkdir -p "$(dirname "$BENCH_LOCK_FILE")" || return $?
  printf '%s\n' "$BENCH_LOCK_FILE"
}

# bench_lock_export: pass the lock file this shell resolved to the processes it starts, so a process under a changed
# HOME (a trial's) takes the same lock.
bench_lock_export() {
  export BENCH_LOCK_FILE
}

# bench_lock_fd_take <fd> <wait seconds>: wait for the bench lock on an open descriptor. The lock belongs to the open file
# description, not to this flock process, so it stays held after flock exits, until the holder releases it or closes the
# descriptor's last copy. Returns BENCH_LOCK_TIMEOUT_EXIT when the wait runs out.
bench_lock_fd_take() {
  flock -w "$2" -E "$BENCH_LOCK_TIMEOUT_EXIT" "$1"
}

# bench_lock_fd_release <fd>: release the bench lock held through an open descriptor.
bench_lock_fd_release() {
  flock -u "$1"
}

# _bench_lock_entries: print `hold <pid>` and `wait <pid>` for each FLOCK entry on the bench lock's file in /proc/locks.
# A blocked request is listed with `->` first, so its line names the waiter that has not taken the lock. The lock file is
# read for its device and inode only; the lock is never taken or released here.
_bench_lock_entries() {
  [[ -e $BENCH_LOCK_FILE ]] || return 0
  local device inode major minor _id kind rest state pid device_inode dev_major dev_minor dev_inode
  device=$(stat -c '%D' -- "$BENCH_LOCK_FILE") || return $?
  inode=$(stat -c '%i' -- "$BENCH_LOCK_FILE") || return $?
  # st_dev packs the major and minor the way glibc's makedev does; /proc/locks prints them in hex.
  device=$((16#$device))
  major=$(( ((device >> 8) & 0xfff) | ((device >> 32) & ~0xfff) ))
  minor=$(( (device & 0xff) | ((device >> 12) & ~0xff) ))
  while read -r _id kind rest; do
    state=hold
    if [[ $kind == -\> ]]; then
      state=wait
      read -r kind rest <<<"$rest"
    fi
    [[ $kind == FLOCK ]] || continue
    read -r _advisory _mode pid device_inode _ <<<"$rest"
    IFS=: read -r dev_major dev_minor dev_inode <<<"$device_inode"
    if (( 16#$dev_major == major && 16#$dev_minor == minor )) && [[ $dev_inode == "$inode" ]]; then
      printf '%s %s\n' "$state" "$pid"
    fi
  done </proc/locks || return $?
  return 0
}

# bench_lock_waiter_pids: print the pid of each process queued for the bench lock and not yet holding it, one per line.
bench_lock_waiter_pids() {
  local state pid
  while read -r state pid; do
    if [[ $state == wait ]]; then
      printf '%s\n' "$pid"
    fi
  done < <(_bench_lock_entries) || return $?
  return 0
}

# bench_lock_holder: print the holder of the bench lock as `<pid> <command line>`, or print nothing when no process
# holds it. The holder is the process that took the lock, so its command line names the work that holds it. A
# waiting process is not the holder. The lock file is read for its device and inode only, and the lock is never taken or
# released here.
bench_lock_holder() {
  local state pid cmdline
  while read -r state pid; do
    [[ $state == hold ]] || continue
    if ! cmdline=$(exec 2>/dev/null; tr '\0' ' ' <"/proc/$pid/cmdline"); then
      cmdline='(exited before its command line was read)'
    fi
    printf '%s %s\n' "$pid" "${cmdline% }"
    return 0
  done < <(_bench_lock_entries) || return $?
  return 0
}

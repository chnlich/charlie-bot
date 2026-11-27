#!/usr/bin/env bash
# The host's bench lock: every bench run on a host holds this one file while it measures, so two runs never measure side
# by side. This file is the only place that names the file and calls flock on it.
#
# The holders are the poor-network bench (bench.mjs, which takes the lock for one step at a time), the remote-wake bench's
# container run (dev/remote-wake/lock.sh) and each improve trial side that holds the lock (dev/improve/trial-side.sh).
#
# A holder that owns its descriptor (bench.mjs) takes the lock with bench_lock_fd_take and releases it with
# bench_lock_fd_release. It finds the waiters with bench_lock_waiter_pids and the holder with bench_lock_hold_pid. A
# holder with a fixed hold (run.sh's callers, the improve sides) runs its command under bench_lock_within, which is the
# same lock.
#
# A side attempt that cannot take the lock within its wait exits with BENCH_LOCK_TIMEOUT_EXIT, the status flock uses when
# its wait runs out.
#
# Sourced by run.sh, bench.mjs and the other holders; a test sources it to drive the lock on its own.

# The host's lock file, under the user's home from the passwd entry. A trial replaces HOME for the bench it runs, so a
# lock named under HOME would be a private file that no other holder shares. An explicit BENCH_LOCK_FILE names another
# file: the tests' scratch lock.
readonly BENCH_USER_HOME="$(getent passwd "$(id -un)" | cut -d: -f6)"
readonly BENCH_LOCK_FILE="${BENCH_LOCK_FILE:-$BENCH_USER_HOME/pn-runs/.bench.lock}"
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

# _bench_lock_dev_inode: print the bench lock file's device and inode as /proc/locks and fdinfo write them,
# `<major>:<minor>:<inode>`, with the device numbers in hex. The lock file is read for its device and inode only.
_bench_lock_dev_inode() {
  local device inode major minor
  device=$(stat -c '%D' -- "$BENCH_LOCK_FILE") || return $?
  inode=$(stat -c '%i' -- "$BENCH_LOCK_FILE") || return $?
  # st_dev packs the major and minor the way glibc's makedev does; the kernel prints both in hex.
  device=$((16#$device))
  major=$(( ((device >> 8) & 0xfff) | ((device >> 32) & ~0xfff) ))
  minor=$(( (device & 0xff) | ((device >> 12) & ~0xff) ))
  printf '%02x:%02x:%s\n' "$major" "$minor" "$inode"
}

# _bench_lock_entries: print `hold <pid>` and `wait <pid>` for each FLOCK entry on the bench lock's file in /proc/locks.
# A blocked request is listed with `->` first, so its line names the waiter that has not taken the lock. The lock file is
# read for its device and inode only; the lock is never taken or released here.
_bench_lock_entries() {
  [[ -e $BENCH_LOCK_FILE ]] || return 0
  local wanted _id kind rest state pid _advisory _mode device_inode
  wanted=$(_bench_lock_dev_inode) || return $?
  while read -r _id kind rest; do
    state=hold
    if [[ $kind == -\> ]]; then
      state=wait
      read -r kind rest <<<"$rest"
    fi
    [[ $kind == FLOCK ]] || continue
    read -r _advisory _mode pid device_inode _ <<<"$rest"
    if [[ $device_inode == "$wanted" ]]; then
      printf '%s %s\n' "$state" "$pid"
    fi
  done </proc/locks || return $?
  return 0
}

# _bench_lock_fd_holders <device:inode>: print the pid of each process that holds the bench lock through a
# descriptor, one per line, from the `lock:` lines of /proc/<pid>/fdinfo. The kernel lists a held lock in every
# descriptor that shares its open file description, so a process that inherited the descriptor is listed too. The pid
# inside the line is the flock helper's and may have exited, so each process is named from the fdinfo path its line
# came from. grep exits 2 when it cannot read a process that another user owns and still prints every match, so that
# status is not checked; a holder it cannot read leaves no candidate, and bench_lock_holder then fails.
_bench_lock_fd_holders() {
  local file
  while read -r file; do
    file=${file#/proc/}
    printf '%s\n' "${file%%/*}"
  done < <(grep -rlsE "^lock:.* FLOCK .* $1 " /proc/[0-9]*/fdinfo)
}

# _bench_lock_stat_fields <pid>: print the parent pid and the start time of a process, which are fields 4 and 22 of
# /proc/<pid>/stat. The start time is in clock ticks since boot. It prints nothing and exits 1 when the process is gone.
_bench_lock_stat_fields() {
  local stat fields
  if ! stat=$(exec 2>/dev/null; cat "/proc/$1/stat"); then
    return 1
  fi
  # The command name in field 2 is in parentheses and may hold spaces, so the fields start after its last ")". The first
  # of them is field 3, the state.
  read -r -a fields <<<"${stat##*) }"
  printf '%s %s\n' "${fields[1]}" "${fields[19]}"
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

# bench_lock_hold_pid: print a pid that /proc/locks lists as holding the bench lock, or nothing when free. The step gate
# reads it while a process queued at its release waits, to tell a held lock from a free one.
bench_lock_hold_pid() {
  local state pid holder_pid=''
  while read -r state pid; do
    if [[ $state == hold ]]; then
      holder_pid=$pid
    fi
  done < <(_bench_lock_entries) || return $?
  if [[ -n $holder_pid ]]; then
    printf '%s\n' "$holder_pid"
  fi
}

# bench_lock_holder: print the holder of the bench lock as `<pid> start=<ticks> <command line>`, or print nothing
# when no process holds it. The holder is the process that opened the lock's descriptor, so it lives for the whole
# hold. The flock helper that took the lock exits at once, and /proc/locks names that helper, so the helper is never
# the holder; nor is a process that only inherited the descriptor. ticks is field 22 of /proc/<pid>/stat, the holder's
# start time in clock ticks since boot: a live process at <pid> with another start time is a reused pid. A waiting
# process is not the holder. The lock file is read for its device and inode only, and the lock is never taken or
# released here.
bench_lock_holder() {
  local cand ppid ticks cmdline dev_inode
  local best='' best_ticks=0 holder_pid=''
  local -a candidates=()
  local -A started=() parent=()
  holder_pid=$(bench_lock_hold_pid) || return $?
  [[ -n $holder_pid ]] || return 0
  dev_inode=$(_bench_lock_dev_inode) || return $?
  mapfile -t candidates < <(_bench_lock_fd_holders "$dev_inode")
  for cand in "${candidates[@]}"; do
    # A candidate that exits during the scan no longer holds the lock, so it is not named.
    read -r ppid ticks < <(_bench_lock_stat_fields "$cand") || continue
    started[$cand]=$ticks
    parent[$cand]=$ppid
  done
  for cand in "${!started[@]}"; do
    # A process whose parent holds the descriptor too inherited it. A parent and the child it forks can share a clock
    # tick, so the parent test decides before the start time does.
    ppid=${parent[$cand]}
    if [[ -n ${started[$ppid]:-} ]]; then
      continue
    fi
    ticks=${started[$cand]}
    if [[ -z $best ]] || (( ticks < best_ticks )); then
      best=$cand
      best_ticks=$ticks
    fi
  done
  if [[ -z $best ]]; then
    # The fdinfo scan takes about a second; a lock released during the scan is now free, not unreadable.
    holder_pid=$(bench_lock_hold_pid) || return $?
    [[ -n $holder_pid ]] || return 0
    printf 'bench lock: /proc/locks lists pid %s as the holder, but no process holds the descriptor of %s\n' \
      "$holder_pid" "$BENCH_LOCK_FILE" >&2
    return 1
  fi
  if ! cmdline=$(exec 2>/dev/null; tr '\0' ' ' <"/proc/$best/cmdline"); then
    printf 'bench lock: holder pid %s exited while its command line was read\n' "$best" >&2
    return 1
  fi
  printf '%s start=%s %s\n' "$best" "$best_ticks" "${cmdline% }"
}

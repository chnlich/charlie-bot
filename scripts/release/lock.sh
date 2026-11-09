# The host release lock that scripts/release.sh and scripts/switch-server.sh hold in turn. This file is the one
# definition of the lock: one lock file per host, outside the repository, and the holder record beside it.
#
#   release_lock_take SCRIPT COMMIT   take the lock without waiting; 0 this caller holds it, 1 it does not
#   release_lock_release              remove the holder record of a lock this process took
#
# The lock is `flock` on an open descriptor, so the kernel frees it when the holder dies, and a stale record from a
# dead holder blocks nobody. The holder writes its record beside the lock: one line naming the script, the commit,
# the pid and the start time, which a refused caller prints.
#
# CHARLIEBOT_RELEASE_LOCK replaces the lock file's path; the tests name a path under their sandbox, and an operator
# does not set it by hand.

RELEASE_LOCK_FILE="${CHARLIEBOT_RELEASE_LOCK:-$HOME/charliebot-release.lock}"
RELEASE_LOCK_RECORD="$RELEASE_LOCK_FILE.holder"
# RELEASE_LOCK_MINE: this process took the lock and owns the record, so it is the one that removes it.
RELEASE_LOCK_MINE=0
# release_lock_take SCRIPT COMMIT: takes the host release lock for SCRIPT working on COMMIT, and returns 0 when this
# caller holds it. It returns 1, prints the holder's record and changes nothing when another caller holds the lock,
# and 1 with the reason when the lock cannot be taken. It never waits and never retries.
release_lock_take() {
  local script=$1 commit=$2 line
  command -v flock > /dev/null 2>&1 || {
    echo "$script: flock is on PATH for the host release lock" >&2
    return 1
  }
  local fd
  exec {fd}>"$RELEASE_LOCK_FILE" || {
    echo "$script: cannot open the host release lock $RELEASE_LOCK_FILE" >&2
    return 1
  }
  if ! flock -n "$fd"; then
    echo "$script: the host release lock $RELEASE_LOCK_FILE is held:" >&2
    if [ -e "$RELEASE_LOCK_RECORD" ]; then
      while IFS= read -r line; do
        echo "$script:   $line" >&2
      done < "$RELEASE_LOCK_RECORD"
    else
      echo "$script:   (the holder wrote no record)" >&2
    fi
    exec {fd}>&-
    return 1
  fi
  printf 'script=%s commit=%s pid=%s started=%s\n' "$script" "$commit" "$$" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    > "$RELEASE_LOCK_RECORD"
  RELEASE_LOCK_MINE=1
  return 0
}

# release_lock_release: removes the holder record of a lock this process took. The lock itself stays with the open
# descriptor until the process exits. A caller that took nothing removes nothing: the holder's record is its own.
release_lock_release() {
  if [ "$RELEASE_LOCK_MINE" = 1 ]; then
    rm -f "$RELEASE_LOCK_RECORD"
  fi
}

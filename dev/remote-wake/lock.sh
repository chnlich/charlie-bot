#!/usr/bin/env bash
# The remote-wake bench lock: the host's one bench lock, whose file the poor-network bench's lock module names. The lock
# is held from after the builds until this shell exits, so the container's start, its measured segments and its cleanup
# all run under it, and no poor-network side measures while the container's network interfaces change.
#
# A run that cannot take the lock within BENCH_LOCK_WAIT_SECONDS exits with BENCH_LOCK_TIMEOUT_EXIT, a code no
# measurement uses.
#
# Sourced by run.sh; a test sources it to drive the lock on its own.

# The file, the wait and the timeout exit come from the module that owns them, so one place names the shared file.
_remote_wake_lock_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
source "$_remote_wake_lock_dir/../poor-network/lock.sh"
unset _remote_wake_lock_dir

# remote_wake_lock: hold the shared lock until this shell exits, waiting up to the limit. bench_lock_hold owns the flock.
remote_wake_lock() {
  bench_lock_hold "$BENCH_LOCK_WAIT_SECONDS"
}

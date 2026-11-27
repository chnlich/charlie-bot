#!/usr/bin/env bash
# Runs cargo for one round's checks. The round builds only in its clone, into the direction's pinned target directory,
# so the clone's products carry over to the next round. A build from any other checkout would write products that name
# that checkout's path, and the clone's next build would reuse them: code that bakes its checkout path at compile time
# (shell-harness's repo_root() reads CARGO_MANIFEST_DIR) would then point at a path that no longer exists.
#
#   dev/improve/round-cargo.sh check --workspace --locked
#   dev/improve/round-cargo.sh test -p charliebot-store --locked
#   dev/improve/round-cargo.sh prepare-target
#
# Cargo commands require a working directory in the round clone, or the script exits 2 and runs nothing. The round's
# runner exports IMPROVE_DIRECTION, which names the clone and the target directory, and calls prepare-target once
# inside the direction's scope before starting the agent.
set -euo pipefail

# Host3's perf target measured 79 GiB after incremental data was removed; 128 GiB leaves about 49 GiB for one release
# and debug target to grow before the next round trims it.
readonly TARGET_CACHE_CAP_MIB=$((128 * 1024))

target_size_mib() {
  local directory=$1 output
  if [[ ! -e $directory ]]; then
    printf '0\n'
    return
  fi
  output=$(du -sm -- "$directory")
  printf '%s\n' "${output%%$'\t'*}"
}

prepare_target_cache() {
  local target_dir=$HOME/.cache/improve-target/${IMPROVE_DIRECTION:?the round runner exports IMPROVE_DIRECTION}
  local before after action=none
  before=$(target_size_mib "$target_dir")
  after=$before
  if (( before > TARGET_CACHE_CAP_MIB )); then
    find "$target_dir" -mindepth 2 -maxdepth 2 -type d -name incremental -prune -exec rm -rf -- {} +
    after=$(target_size_mib "$target_dir")
    action=incremental
    if (( after > TARGET_CACHE_CAP_MIB )); then
      rm -rf -- "$target_dir"
      after=0
      action=incremental+directory
    fi
  fi
  printf 'improve target cache: before=%sMiB action=%s after=%sMiB cap=%sMiB\n' \
    "$before" "$action" "$after" "$TARGET_CACHE_CAP_MIB" >&2
}

if [[ ${1:-} == prepare-target ]]; then
  [[ $# -eq 1 ]] || { printf 'usage: %s prepare-target\n' "${BASH_SOURCE[0]}" >&2; exit 2; }
  prepare_target_cache
  exit 0
fi

clone=$HOME/improve/${IMPROVE_DIRECTION:?the round runner exports IMPROVE_DIRECTION}/charlie-bot
top=$(git rev-parse --show-toplevel)
if [[ $(realpath -- "$top") != "$(realpath -- "$clone")" ]]; then
  printf 'round-cargo: refuses a build in %s: a round builds only in its clone, %s\n' "$top" "$clone" >&2
  exit 2
fi

export CARGO_TARGET_DIR="$HOME/.cache/improve-target/$IMPROVE_DIRECTION"
export CARGO_BUILD_JOBS=4
exec nice -n 19 cargo "$@"

#!/usr/bin/env bash
# Runs cargo for one round's checks. The round builds only in its clone, into the direction's pinned target directory,
# so the clone's products carry over to the next round. A build from any other checkout would write products that name
# that checkout's path, and the clone's next build would reuse them: code that bakes its checkout path at compile time
# (shell-harness's repo_root() reads CARGO_MANIFEST_DIR) would then point at a path that no longer exists.
#
#   dev/improve/round-cargo.sh check --workspace --locked
#   dev/improve/round-cargo.sh test -p charliebot-store --locked
#
# The working directory must lie in the round clone, or the script exits 2 and runs nothing. The round's runner exports
# IMPROVE_DIRECTION, which names the clone and the target directory.
set -euo pipefail

clone=$HOME/improve/${IMPROVE_DIRECTION:?the round runner exports IMPROVE_DIRECTION}/charlie-bot
top=$(git rev-parse --show-toplevel)
if [[ $(realpath -- "$top") != "$(realpath -- "$clone")" ]]; then
  printf 'round-cargo: refuses a build in %s: a round builds only in its clone, %s\n' "$top" "$clone" >&2
  exit 2
fi

export CARGO_TARGET_DIR=$HOME/.cache/improve-target/$IMPROVE_DIRECTION
export CARGO_BUILD_JOBS=4
exec nice -n 19 cargo "$@"

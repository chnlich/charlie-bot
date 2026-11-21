# The direction clone's identity, sync and pre-push check. remote-runner.sh sources this file from its launch
# directory and calls sync_clone, then check_clone, before the round's CLI build. clone-check.test.sh sources it from
# the repository. The functions read ROUND_ROOT and direction, and they call the sourcing script's fail and
# round_is_live.
#
# The round's agent works only in the direction clone, so the clone must be this protocol's clone before the round
# starts. check_clone_identity refuses a clone that is absent, whose core.hooksPath is not the runner's hooks, or whose
# origin is not charlie-bot. Each refusal exits 4.
#
# sync_clone sets the clone's tree to origin/main: it fetches the clone's origin and detaches HEAD at origin/main.
# It keeps every local branch and every uncommitted change. Git refuses the checkout when a file with uncommitted
# work would be overwritten, and that refusal exits 4. A clone whose previous round still runs keeps its checkout
# and exits 5.
#
# check_clone requires the pre-push hook in the clone's tree, because the round's pushes run it.

# Exit 4 unless the clone is this protocol's clone: present, with core.hooksPath at dev/git-hooks, and with an origin
# whose repository is charlie-bot.
check_clone_identity() {
  local clone="$ROUND_ROOT/charlie-bot"
  local hooks origin
  [[ -d $clone ]] || fail 4 "the direction clone is missing: $clone"
  hooks=$(git -C "$clone" config --get core.hooksPath 2>/dev/null) || hooks=
  # The clone points at the runner checkout's hooks (an absolute path), so a clone from any layout runs them.
  [[ $hooks == dev/git-hooks || $hooks == */dev/git-hooks ]] \
    || fail 4 "the direction clone does not set core.hooksPath to dev/git-hooks: $clone"
  origin=$(git -C "$clone" remote get-url origin 2>/dev/null) || origin=
  [[ $(basename -- "${origin:-/nonexistent}" .git) == charlie-bot ]] \
    || fail 4 "the direction clone's origin does not point at charlie-bot: ${origin:-no origin}"
}

# Set the clone's tree to origin/main. The identity check runs first, so a foreign clone is refused before anything
# fetches into it. The fetch goes to the clone's own origin.
sync_clone() {
  local clone="$ROUND_ROOT/charlie-bot"
  check_clone_identity
  if round_is_live "$direction"; then
    fail 5 "the previous round still runs, so the direction clone keeps its checkout: $clone"
  fi
  git -C "$clone" fetch --quiet --no-tags origin main \
    || fail 1 "cannot fetch origin/main into the direction clone: $clone"
  git -C "$clone" -c advice.detachedHead=false checkout --quiet --detach origin/main \
    || fail 4 "cannot set the direction clone to origin/main; git refused the checkout: $clone"
}

# Exit 4 unless the clone is this protocol's clone and its tree carries an executable pre-push hook. The hook checks
# each push's diffs and messages for the round's credential values.
check_clone() {
  local clone="$ROUND_ROOT/charlie-bot"
  check_clone_identity
  [[ -x $clone/dev/git-hooks/pre-push ]] \
    || fail 4 "the direction clone has no executable dev/git-hooks/pre-push: $clone"
}

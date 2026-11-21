#!/usr/bin/env bash
# Runs clone-check.sh against real git clones. Each case builds its origin and its clone under one temporary
# directory outside the repository, and the script deletes that directory on exit.
#
#   dev/improve/clone-check.test.sh
set -euo pipefail

# The runner's helpers that the module calls. These match the runner's message prefix and its exit codes.
fail() {
  local code=$1
  shift
  printf 'improve runner: %s\n' "$*" >&2
  exit "$code"
}

# The runner's live-round check. TEST_ROUND_LIVE=1 makes the direction's round look live.
round_is_live() {
  [[ ${TEST_ROUND_LIVE:-0} == 1 ]]
}

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$SCRIPT_DIR/clone-check.sh"

TMP_ROOT=$(mktemp -d)
trap 'rm -rf -- "$TMP_ROOT"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid
direction=dead_code
case_name=

# Builds the case's origin, charlie-bot.git, and a clone of it on local main at the commit without the hooks.
# Then origin main moves to the commit that adds them: the shape of a clone that an earlier landing left behind.
setup_stale_clone() {  # setup_stale_clone <case name>
  local case_dir=$TMP_ROOT/$1 seed
  case_name=$1
  seed=$case_dir/seed
  ROUND_ROOT=$case_dir/improve/$direction
  mkdir -p -- "$case_dir/remotes" "$ROUND_ROOT"
  git init -q --bare --initial-branch=main "$case_dir/remotes/charlie-bot.git"
  git init -q --initial-branch=main "$seed"
  printf 'base\n' >"$seed/README.md"
  git -C "$seed" add -A
  git -C "$seed" commit -q -m "base without the hooks"
  git -C "$seed" push -q "$case_dir/remotes/charlie-bot.git" main
  git clone -q "$case_dir/remotes/charlie-bot.git" "$ROUND_ROOT/charlie-bot"
  git -C "$ROUND_ROOT/charlie-bot" config core.hooksPath dev/git-hooks
  mkdir -p -- "$seed/dev/git-hooks"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$seed/dev/git-hooks/pre-push"
  chmod 755 -- "$seed/dev/git-hooks/pre-push"
  git -C "$seed" add -A
  git -C "$seed" commit -q -m "add the hooks"
  git -C "$seed" push -q "$case_dir/remotes/charlie-bot.git" main
}

head_of() {
  git -C "$ROUND_ROOT/charlie-bot" rev-parse HEAD
}

# Runs one module function in a subshell, as the runner runs it, and checks its exit status and its stderr text.
expect_exit() {  # expect_exit <status> <stderr text, empty for none> <function>
  local want=$1 text=$2 function_name=$3 got=0
  ( "$function_name" ) 2>"$TMP_ROOT/stderr" || got=$?
  if (( got != want )) || { [[ -n $text ]] && ! grep -qF -- "$text" "$TMP_ROOT/stderr"; }; then
    printf 'FAIL %s: %s exited %s, want %s; stderr:\n' "$case_name" "$function_name" "$got" "$want" >&2
    cat -- "$TMP_ROOT/stderr" >&2
    exit 1
  fi
  printf 'ok %s: %s exits %s\n' "$case_name" "$function_name" "$want"
}

assert_equal() {  # assert_equal <what> <got> <want>
  if [[ $2 != "$3" ]]; then
    printf 'FAIL %s: %s is %s, want %s\n' "$case_name" "$1" "$2" "$3" >&2
    exit 1
  fi
}

assert_detached() {
  if git -C "$ROUND_ROOT/charlie-bot" symbolic-ref -q HEAD >/dev/null; then
    printf 'FAIL %s: HEAD still names a branch\n' "$case_name" >&2
    exit 1
  fi
}

# The stale clone fails the check, the sync brings it to origin/main, and then the check passes. The sync detaches HEAD
# and leaves local main at the commit it had.
check_stale_clone_passes_after_sync() {
  local origin_main old_head
  setup_stale_clone stale
  origin_main=$(git -C "$TMP_ROOT/stale/remotes/charlie-bot.git" rev-parse main)
  old_head=$(head_of)
  expect_exit 4 "has no executable dev/git-hooks/pre-push" check_clone
  expect_exit 0 "" sync_clone
  expect_exit 0 "" check_clone
  assert_equal "HEAD" "$(head_of)" "$origin_main"
  assert_detached
  assert_equal "local main" "$(git -C "$ROUND_ROOT/charlie-bot" rev-parse main)" "$old_head"
}

check_wrong_hooks_path_exits_4() {
  local head
  setup_stale_clone hooks-path
  git -C "$ROUND_ROOT/charlie-bot" config core.hooksPath .githooks
  head=$(head_of)
  expect_exit 4 "does not set core.hooksPath to dev/git-hooks" check_clone
  expect_exit 4 "does not set core.hooksPath to dev/git-hooks" sync_clone
  assert_equal "HEAD" "$(head_of)" "$head"
}

check_foreign_origin_exits_4() {
  local head
  setup_stale_clone foreign-origin
  git init -q --bare "$TMP_ROOT/foreign-origin/remotes/other.git"
  git -C "$ROUND_ROOT/charlie-bot" remote set-url origin "$TMP_ROOT/foreign-origin/remotes/other.git"
  head=$(head_of)
  expect_exit 4 "origin does not point at charlie-bot" check_clone
  expect_exit 4 "origin does not point at charlie-bot" sync_clone
  assert_equal "HEAD" "$(head_of)" "$head"
}

# An untracked file at a path that origin/main tracks would be overwritten by the checkout, so git refuses it and the
# file keeps its content.
check_uncommitted_work_blocks_the_sync() {
  local head
  setup_stale_clone uncommitted
  mkdir -p -- "$ROUND_ROOT/charlie-bot/dev/git-hooks"
  printf 'local work\n' >"$ROUND_ROOT/charlie-bot/dev/git-hooks/pre-push"
  head=$(head_of)
  expect_exit 4 "cannot set the direction clone to origin/main" sync_clone
  assert_equal "HEAD" "$(head_of)" "$head"
  assert_equal "the uncommitted file" "$(cat -- "$ROUND_ROOT/charlie-bot/dev/git-hooks/pre-push")" "local work"
}

check_live_round_keeps_its_clone() {
  local head
  setup_stale_clone live-round
  head=$(head_of)
  TEST_ROUND_LIVE=1 expect_exit 5 "the previous round still runs" sync_clone
  assert_equal "HEAD" "$(head_of)" "$head"
}

check_missing_clone_exits_4() {
  case_name=missing-clone
  ROUND_ROOT=$TMP_ROOT/missing-clone/improve/$direction
  mkdir -p -- "$ROUND_ROOT"
  expect_exit 4 "the direction clone is missing" sync_clone
  expect_exit 4 "the direction clone is missing" check_clone
}

check_stale_clone_passes_after_sync
check_wrong_hooks_path_exits_4
check_foreign_origin_exits_4
check_uncommitted_work_blocks_the_sync
check_live_round_keeps_its_clone
check_missing_clone_exits_4
printf 'clone-check: every case passed\n'

#!/usr/bin/env bash
# Runs round-cargo.sh in real git checkouts with a stub cargo on PATH, so no build runs. Each case makes its clone and
# its other checkouts under one temporary directory, and the script deletes that directory on exit.
#
#   dev/improve/round-cargo.test.sh
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
HELPER=$SCRIPT_DIR/round-cargo.sh

TMP_ROOT=$(mktemp -d)
trap 'rm -rf -- "$TMP_ROOT"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid

export HOME=$TMP_ROOT/home
export IMPROVE_DIRECTION=decoupling
export ROUND_CARGO_STUB_LOG=$TMP_ROOT/stub.log
mkdir -p -- "$HOME/bin"
export PATH=$HOME/bin:$PATH

# The stub cargo records its working directory, target directory, build jobs and arguments. It builds nothing.
cat >"$HOME/bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n%s\n%s\n%s\n' "$PWD" "${CARGO_TARGET_DIR:-unset}" "${CARGO_BUILD_JOBS:-unset}" "$*" >"$ROUND_CARGO_STUB_LOG"
EOF
chmod 755 -- "$HOME/bin/cargo"

case_name=
round_clone=$HOME/improve/$IMPROVE_DIRECTION/charlie-bot

# Builds the round clone with a tracked rust/ directory, so a worktree of the clone has that directory too.
setup_round_clone() {
  mkdir -p -- "$HOME/improve/$IMPROVE_DIRECTION"
  git init -q --initial-branch=main "$round_clone"
  mkdir -p -- "$round_clone/rust"
  printf 'keep\n' >"$round_clone/rust/keep"
  git -C "$round_clone" add -A
  git -C "$round_clone" commit -q -m "base"
}

# Runs the helper in one directory, and checks its exit status and its stderr text. The stub log exists only after a run
# that reached cargo.
expect_exit() {  # expect_exit <status> <stderr text, empty for none> <directory>
  local want=$1 text=$2 directory=$3 got=0
  rm -f -- "$ROUND_CARGO_STUB_LOG"
  ( cd -- "$directory" && "$HELPER" check --workspace --locked ) 2>"$TMP_ROOT/stderr" || got=$?
  if (( got != want )) || { [[ -n $text ]] && ! grep -qF -- "$text" "$TMP_ROOT/stderr"; }; then
    printf 'FAIL %s: exited %s, want %s; stderr:\n' "$case_name" "$got" "$want" >&2
    cat -- "$TMP_ROOT/stderr" >&2
    exit 1
  fi
}

assert_no_build() {
  if [[ -e $ROUND_CARGO_STUB_LOG ]]; then
    printf 'FAIL %s: cargo ran\n' "$case_name" >&2
    exit 1
  fi
}

assert_stub_run() {  # assert_stub_run <working directory> <target directory> <jobs> <arguments>
  local want got
  want=$(printf '%s\n%s\n%s\n%s\n' "$1" "$2" "$3" "$4")
  got=$(<"$ROUND_CARGO_STUB_LOG")
  if [[ $got != "$want" ]]; then
    printf 'FAIL %s: cargo ran with\n%s\nwant\n%s\n' "$case_name" "$got" "$want" >&2
    exit 1
  fi
}

# A build from the round clone runs cargo there, with the direction's pinned target directory and four build jobs.
check_round_clone_builds_in_its_pinned_target() {
  case_name=round-clone
  setup_round_clone
  expect_exit 0 "" "$round_clone/rust"
  assert_stub_run "$round_clone/rust" "$HOME/.cache/improve-target/$IMPROVE_DIRECTION" 4 "check --workspace --locked"
  printf 'ok %s: the round clone builds into its pinned target\n' "$case_name"
}

# A worktree of the round clone holds the same tree but another path, so its build is refused and cargo does not run.
check_worktree_build_is_refused() {
  case_name=worktree
  git -C "$round_clone" worktree add -q --detach "$TMP_ROOT/scratch-worktree"
  expect_exit 2 "refuses a build in $TMP_ROOT/scratch-worktree" "$TMP_ROOT/scratch-worktree/rust"
  assert_no_build
  printf 'ok %s: a worktree of the round clone is refused\n' "$case_name"
}

# A separate clone of the same repository is refused too, because its top level is not the round clone.
check_separate_clone_build_is_refused() {
  case_name=separate-clone
  git clone -q -- "$round_clone" "$TMP_ROOT/scratch-clone"
  expect_exit 2 "refuses a build in $TMP_ROOT/scratch-clone" "$TMP_ROOT/scratch-clone/rust"
  assert_no_build
  printf 'ok %s: a separate clone is refused\n' "$case_name"
}

# Without IMPROVE_DIRECTION the helper cannot name the round clone, so it stops before cargo runs.
check_missing_direction_fails_fast() {
  local got=0
  case_name=missing-direction
  ( unset IMPROVE_DIRECTION; cd -- "$round_clone/rust" && "$HELPER" check ) 2>"$TMP_ROOT/stderr" || got=$?
  if (( got == 0 )) || ! grep -qF -- "IMPROVE_DIRECTION" "$TMP_ROOT/stderr"; then
    printf 'FAIL %s: exited %s; stderr:\n' "$case_name" "$got" >&2
    cat -- "$TMP_ROOT/stderr" >&2
    exit 1
  fi
  assert_no_build
  printf 'ok %s: an unset IMPROVE_DIRECTION exits %s\n' "$case_name" "$got"
}

# Runs the helper with cargo arguments that name `clean`, and checks that it exits 2 with the refusal and runs no cargo.
expect_clean_refused() {  # expect_clean_refused <cargo arguments>
  local got=0
  rm -f -- "$ROUND_CARGO_STUB_LOG"
  ( cd -- "$round_clone/rust" && "$HELPER" "$@" ) 2>"$TMP_ROOT/stderr" || got=$?
  if (( got != 2 )) || ! grep -qF -- "refuses cargo clean" "$TMP_ROOT/stderr"; then
    printf 'FAIL %s: exited %s for %s; stderr:\n' "$case_name" "$got" "$*" >&2
    cat -- "$TMP_ROOT/stderr" >&2
    exit 1
  fi
  assert_no_build
}

# A `clean` removes the pinned target directory, so the helper refuses it, wherever it sits among cargo's arguments,
# and the cached products stay.
check_clean_is_refused() {
  local target=$HOME/.cache/improve-target/$IMPROVE_DIRECTION
  case_name=clean
  mkdir -p -- "$target/debug/deps"
  printf 'cached\n' >"$target/debug/deps/libcached.rlib"
  expect_clean_refused clean
  expect_clean_refused +stable clean --locked
  if [[ ! -f $target/debug/deps/libcached.rlib ]]; then
    printf 'FAIL %s: a refused clean removed the cached products\n' "$case_name" >&2
    exit 1
  fi
  printf 'ok %s: cargo clean is refused and the pinned cache stays\n' "$case_name"
}

check_round_clone_builds_in_its_pinned_target
check_worktree_build_is_refused
check_separate_clone_build_is_refused
check_missing_direction_fails_fast
check_clean_is_refused

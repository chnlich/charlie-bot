#!/usr/bin/env bash
# The key-file growth guard: fail when a diff adds more lines than allowed to a key file. A key file is a tracked
# source file under rust/, web/, dev/ or scripts/ of KEY_FILE_LINES lines or more at HEAD. The source files are Rust,
# JavaScript, ES module and shell files. The guard reads the lines that the diff from the base ref to HEAD adds to each
# file. CI runs it on each pull request with the base commit as the argument; run it by hand from anywhere inside
# the checkout:
# `dev/check-key-file-growth.sh <base-ref>`. With --self-test, it also proves on synthetic repositories that the guard
# passes a diff that keeps the rule and fails a diff that breaks it.
#
# The rule it enforces is `New code in its own module` in prompts/task_base.md. The guard counts lines, so it cannot
# tell whether a piece of code could stand as its own module; the reviewer judges that.
#
# Test code is out of scope: test directories and files whose names mark them as tests.
set -euo pipefail
shopt -s inherit_errexit

# A file of this many lines or more at HEAD is a key file.
KEY_FILE_LINES=1000
# A key file may gain at most this many added lines in one diff.
MAX_ADDED_LINES=20

# Print one failure per key file that the diff from <base> to HEAD of the checkout at <root> grows past the limit: its
# path, its added lines, its line count at HEAD and the limit. Return 1 after printing every failure.
check_root() {  # <root> <base>
  local root=$1 base=$2 status=0 numstat added deleted path lines
  numstat=$(git -C "$root" diff --numstat --no-renames "$base...HEAD" -- rust web dev scripts)
  while IFS=$'\t' read -r added deleted path; do
    [[ -n $path ]] || continue
    case "$path" in
      */test/*|*/tests/*|*_tests.rs|*_test.rs|*/tests.rs|*.test.js|*.test.mjs|\
        *.test.sh|*_test.sh|*_tests.sh) continue ;;
      *.rs|*.js|*.mjs|*.sh) ;;
      *) continue ;;
    esac
    # A binary file reports `-` where the added count goes.
    if [[ $added == - ]]; then
      continue
    fi
    (( added > MAX_ADDED_LINES )) || continue
    lines=$(git -C "$root" show "HEAD:$path" | wc -l)
    (( lines >= KEY_FILE_LINES )) || continue
    printf '%s: %s lines added to a key file of %s lines; the limit is %s\n' \
      "$path" "$added" "$lines" "$MAX_ADDED_LINES" >&2
    status=1
  done <<< "$numstat"

  if (( status != 0 )); then
    printf 'Rule: `New code in its own module` in prompts/task_base.md: %s\n' \
      'new code that can stand as its own module goes into that module, and the key file keeps short calls to it.' >&2
  fi
  return "$status"
}

# Check the copy at <root> against <base> and compare its exit status, and the text it prints, with what the case
# expects. A failure must also print the rule it enforces.
expect() {  # <label> <root> <base> <wanted status: 0 or 1> <needle, empty for none>
  local label=$1 root=$2 base=$3 wanted=$4 needle=$5 got=0 out
  out=$(check_root "$root" "$base" 2>&1) || got=$?
  if [[ $got -ne $wanted ]] || { [[ -n $needle ]] && ! grep -qF -- "$needle" <<< "$out"; } ||
    { [[ $wanted -eq 1 ]] && ! grep -qF -- 'New code in its own module' <<< "$out"; }; then
    printf 'self-test FAILED: %s (exit %s, wanted %s, needle %s)\n%s\n' "$label" "$got" "$wanted" "$needle" "$out" >&2
    return 1
  fi
  printf 'self-test ok: %s\n' "$label"
  return 0
}

# Commit every change in the copy at <root> under a fixed identity.
commit_all() {  # <root> <message>
  git -C "$1" add -A
  git -C "$1" -c user.name=key-file-growth-check -c user.email=key-file-growth-check@example.invalid \
    -c commit.gpgSign=false commit -q -m "$2"
}

# Prove the guard on synthetic repositories. Each case starts from the base commit, so one case's edits never reach the
# next case. The copy lives in a global so the EXIT trap can still name it after the function returns.
self_test() {
  local status=0 base
  self_test_dir=$(mktemp -d)
  trap 'rm -rf "$self_test_dir"' EXIT
  git init -q "$self_test_dir"
  mkdir -p "$self_test_dir/rust/src/tests" "$self_test_dir/web/static" "$self_test_dir/scripts" "$self_test_dir/dev"
  seq 1 1000 > "$self_test_dir/rust/src/key.rs"
  seq 1 1000 > "$self_test_dir/rust/src/tests/big.rs"
  seq 1 1000 > "$self_test_dir/web/static/key.js"
  seq 1 1000 > "$self_test_dir/scripts/key.sh"
  seq 1 1000 > "$self_test_dir/dev/key.mjs"
  seq 1 1000 > "$self_test_dir/rust/src/data.json"
  seq 1 1000 > "$self_test_dir/dev/check.test.mjs"
  seq 1 1000 > "$self_test_dir/scripts/check.test.sh"
  seq 1 10 > "$self_test_dir/rust/src/small.rs"
  commit_all "$self_test_dir" base
  base=$(git -C "$self_test_dir" rev-parse HEAD)

  # A short call into the key file, with the new code in its own module, keeps the rule.
  printf '%s\n' 'module_call();' 'module_call_again();' 'module_call_last();' >> "$self_test_dir/rust/src/key.rs"
  seq 1 200 > "$self_test_dir/rust/src/module.rs"
  commit_all "$self_test_dir" keep
  expect 'a short call and a new module pass' "$self_test_dir" "$base" 0 '' || status=1
  git -C "$self_test_dir" reset -q --hard "$base"

  # New code that stays in a key file breaks the rule at the file's path.
  seq 1001 1050 >> "$self_test_dir/rust/src/key.rs"
  commit_all "$self_test_dir" grow
  expect 'a key file that gains 50 lines fails at its path' "$self_test_dir" "$base" 1 \
    'rust/src/key.rs: 50 lines added' || status=1
  git -C "$self_test_dir" reset -q --hard "$base"

  # The same growth in a key file under web/ breaks the rule too.
  seq 1001 1050 >> "$self_test_dir/web/static/key.js"
  commit_all "$self_test_dir" grow-web
  expect 'a web key file that gains 50 lines fails at its path' "$self_test_dir" "$base" 1 \
    'web/static/key.js: 50 lines added' || status=1
  git -C "$self_test_dir" reset -q --hard "$base"

  # Maintenance source under dev/ uses the same limit.
  seq 1001 1050 >> "$self_test_dir/dev/key.mjs"
  commit_all "$self_test_dir" grow-dev
  expect 'a dev key file that gains 50 lines fails at its path' "$self_test_dir" "$base" 1 \
    'dev/key.mjs: 50 lines added' || status=1
  git -C "$self_test_dir" reset -q --hard "$base"

  # The same growth in an operator shell script is covered by the same rule.
  seq 1001 1050 >> "$self_test_dir/scripts/key.sh"
  commit_all "$self_test_dir" grow-shell
  expect 'a shell key file that gains 50 lines fails at its path' "$self_test_dir" "$base" 1 \
    'scripts/key.sh: 50 lines added' || status=1
  git -C "$self_test_dir" reset -q --hard "$base"

  # A file under the key-file size passes, and so do a test file and a data file of any size.
  seq 11 60 >> "$self_test_dir/rust/src/small.rs"
  seq 1001 1050 >> "$self_test_dir/rust/src/tests/big.rs"
  seq 1001 1050 >> "$self_test_dir/rust/src/data.json"
  seq 1001 1050 >> "$self_test_dir/dev/check.test.mjs"
  seq 1001 1050 >> "$self_test_dir/scripts/check.test.sh"
  commit_all "$self_test_dir" small-test-and-data
  expect 'a small file, test files and a data file pass' "$self_test_dir" "$base" 0 '' || status=1
  git -C "$self_test_dir" reset -q --hard "$base"

  return "$status"
}

case "${1:-}" in
  '')
    printf 'usage: dev/check-key-file-growth.sh <base-ref> | --self-test\n' >&2
    exit 2
    ;;
  --self-test) self_test ;;
  *)
    cd "$(git rev-parse --show-toplevel)"
    check_root "$PWD" "$1"
    ;;
esac

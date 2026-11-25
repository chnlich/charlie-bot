#!/usr/bin/env bash
# The session-origin guard: fail on a tracked non-test source line under rust/ or web/ that branches a session's core
# capability on its origin. CI runs it in the frontend-test job, before the installs; run it by hand from anywhere
# inside the checkout. With --self-test, it also proves on a copy of the tracked sources that the guard passes the
# tree, fails a re-added predicate, and fails a stale allowlist entry.
#
# The rule it enforces is `One general mechanism` in prompts/task_base.md: the execution role alone decides a
# session's core capabilities, and the origin attaches only a channel or schedule binding. Two groups of names fail:
# - banned names: the origin gates of the web and cron kinds, and the web UI check isWebSession. No file may name them;
# - binding names: the thread-origin predicates. They stay allowed only in the files of BINDING_ALLOWLIST.
# An allowlist entry whose file no longer names its predicate fails as stale, so each removal tightens the list.
#
# Test code is out of scope: test directories, `*_tests.rs` and `tests.rs` files, and each file's inline test module.
# That module starts at a column-0 `#[cfg(test)]` before a `mod name {` line and runs to the end of the file. Clippy's
# items_after_test_module lint keeps every Rust test module last, and the clippy gate checks every Rust file.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Names that gate a session on its origin.
BANNED_PATTERN='is_web_session|require_web_session|fn turn_owner|\.turn_owner\('
BANNED_PATTERN+='|is_cron_session|require_cron_session|isWebSession'

# Names that read the thread origin.
BINDING_PATTERN='is_thread_session|require_thread_session|THREAD_SESSION|\.thread_session([^A-Za-z0-9_]|$)'

# Each entry is a channel binding: it defines the thread origin, loads or reads the thread session for a reply, or
# filters the Threads view on it. The Threads view is a view over the binding, not a core capability.
BINDING_ALLOWLIST=(
  'rust/crates/runtime/registry/src/thread_origin.rs' # defines the thread origin that the bindings read
  'rust/crates/features/chat_threads/src/thread_entry.rs' # loads the thread session on the platform reply path
  'rust/crates/features/discord/src/discord_commands.rs' # the Discord reply command loads the thread session
  'rust/crates/features/feishu/src/adapter.rs' # the Feishu reply transport loads the thread session
  'rust/crates/features/feishu/src/commands.rs' # the Feishu reply commands load the thread session
  'rust/crates/features/chat_threads/src/sidebar.rs' # the Threads view filter reads the binding
)

# Print each hit of one pattern in the tracked sources of the checkout at <root>, one tab-separated line per hit:
# file, line number, matched text, and the line's text. A file's hits stop at its inline test module.
pattern_hits() {  # <root> <pattern>
  (
    cd "$1"
    local -a files=()
    local file
    while IFS= read -r -d '' file; do
      case "$file" in
        web/tests/*|*/tests/*|*_tests.rs|*/tests.rs) ;;
        *) files+=("$file") ;;
      esac
    done < <(git ls-files -z -- rust web)
    if (( ${#files[@]} == 0 )); then
      printf 'no tracked sources under rust/ or web/ in %s\n' "$1" >&2
      exit 2
    fi
    LC_ALL=C ORIGIN_PATTERN=$2 awk '
      FNR == 1 { pending = 0; in_tests = 0 }
      in_tests { next }
      /^#\[cfg\(test\)\]$/ { pending = 1; next }
      pending && /^mod [A-Za-z0-9_]+ *\{/ { in_tests = 1; next }
      {
        pending = 0
        rest = $0
        while (match(rest, ENVIRON["ORIGIN_PATTERN"])) {
          hit = substr(rest, RSTART, RLENGTH)
          step = RLENGTH
          # A trailing non-identifier character is read again by the next match, so it is left in the text.
          if (hit ~ /[^A-Za-z0-9_]$/) {
            hit = substr(hit, 1, RLENGTH - 1)
            step = RLENGTH - 1
          }
          printf "%s\t%d\t%s\t%s\n", FILENAME, FNR, hit, $0
          rest = substr(rest, RSTART + step)
        }
      }
    ' "${files[@]}"
  )
}

# Succeed when <file> is an entry of the allowlist array named <name>.
listed() {  # <file> <allowlist array name>
  local -n allowlist=$2
  local entry
  for entry in "${allowlist[@]}"; do
    if [[ $entry == "$1" ]]; then
      return 0
    fi
  done
  return 1
}

# Print one failure: its file, line and matched text, with the reason for its kind.
report() {  # <file> <line> <matched text> <text of the line> <banned|binding>
  local reason
  case "$5" in
    banned) reason='is a banned origin gate; the execution role alone decides core capabilities' ;;
    binding) reason='reads the thread origin outside BINDING_ALLOWLIST; an origin attaches only its binding' ;;
  esac
  printf '%s:%s: `%s` %s\n    %s\n' "$1" "$2" "$3" "$reason" "$4" >&2
}

# Check the tracked sources of the checkout at <root>. Return 1 after printing every failure.
check_root() {  # <root>
  local status=0 file line token text entry
  local -A binding_seen=()
  local banned binding
  banned=$(pattern_hits "$1" "$BANNED_PATTERN")
  binding=$(pattern_hits "$1" "$BINDING_PATTERN")

  while IFS=$'\t' read -r file line token text; do
    [[ -n $file ]] || continue
    report "$file" "$line" "$token" "$text" banned
    status=1
  done <<< "$banned"

  while IFS=$'\t' read -r file line token text; do
    [[ -n $file ]] || continue
    if listed "$file" BINDING_ALLOWLIST; then
      binding_seen[$file]=1
      continue
    fi
    report "$file" "$line" "$token" "$text" binding
    status=1
  done <<< "$binding"

  for entry in "${BINDING_ALLOWLIST[@]}"; do
    if [[ -z ${binding_seen[$entry]-} ]]; then
      printf 'stale allowlist entry %s: it no longer names a thread-origin predicate; %s\n' "$entry" \
        'remove it from BINDING_ALLOWLIST in dev/check-session-origin.sh' >&2
      status=1
    fi
  done

  if (( status != 0 )); then
    printf 'Rule: `One general mechanism` in prompts/task_base.md: %s; %s\n' \
      'the execution role alone decides core capabilities' \
      'the origin attaches only its channel or schedule binding.' >&2
  fi
  return "$status"
}

# Check the copy at <root> and compare its exit status, and the text it prints, with what the case expects. A failure
# must also print the rule it enforces.
expect() {  # <label> <root> <wanted status: 0 or 1> <needle, empty for none>
  local label=$1 root=$2 wanted=$3 needle=$4 got=0 out
  out=$(check_root "$root" 2>&1) || got=$?
  if [[ $got -ne $wanted ]] || { [[ -n $needle ]] && ! grep -qF -- "$needle" <<< "$out"; } ||
    { [[ $wanted -eq 1 ]] && ! grep -qF -- 'One general mechanism' <<< "$out"; }; then
    printf 'self-test FAILED: %s (exit %s, wanted %s, needle %s)\n%s\n' "$label" "$got" "$wanted" "$needle" "$out" >&2
    return 1
  fi
  printf 'self-test ok: %s\n' "$label"
  return 0
}

# Put the given lines at the top of a file of the self-test copy.
prepend() {  # <file> <line>...
  local file=$1
  shift
  { printf '%s\n' "$@"; cat "$file"; } > "$file.new"
  mv "$file.new" "$file"
}

# Prove the guard on a copy of the tracked sources. Each case edits the copy, then restores it from the index.
# The copy lives in a global so the EXIT trap can still name it after the function returns.
self_test() {
  local status=0 status_rs=rust/crates/features/web_reply/src/status.rs
  self_test_dir=$(mktemp -d)
  trap 'rm -rf "$self_test_dir"' EXIT
  git ls-files -z -- rust web | xargs -0 cp --parents -t "$self_test_dir"
  git init -q "$self_test_dir"
  git -C "$self_test_dir" add -- rust web

  expect 'the tracked sources pass' "$self_test_dir" 0 '' || status=1

  # status.rs holds an inline test module; a predicate there is test code and passes.
  printf '%s\n' 'fn is_web_session_fixture() {}' >> "$self_test_dir/$status_rs"
  expect 'a predicate inside the inline test module passes' "$self_test_dir" 0 '' || status=1
  git -C "$self_test_dir" checkout -q -- rust web

  prepend "$self_test_dir/$status_rs" 'fn is_web_session(meta: &SessionMetadata) -> bool {' \
    '  meta.profile == TaskProfile::Manager' '}'
  expect 'a re-added is_web_session fails at its line' "$self_test_dir" 1 "$status_rs:1:" || status=1
  git -C "$self_test_dir" checkout -q -- rust web

  prepend "$self_test_dir/$status_rs" 'let session_thread = is_thread_session(meta);'
  expect 'a thread-origin call outside the binding allowlist fails at its line' "$self_test_dir" 1 "$status_rs:1:" \
    || status=1
  git -C "$self_test_dir" checkout -q -- rust web

  local modals=web/static/js/sidebar/modals.js
  prepend "$self_test_dir/$modals" 'function isWebSession(session) { return session.profile === "manager"; }'
  expect 'isWebSession in an unallowlisted web file fails at its line' "$self_test_dir" 1 "$modals:1:" || status=1
  git -C "$self_test_dir" checkout -q -- rust web

  local sidebar=rust/crates/features/chat_threads/src/sidebar.rs
  sed -i '/is_thread_session/d' "$self_test_dir/$sidebar"
  expect 'a binding entry that no longer names its predicate is stale' "$self_test_dir" 1 \
    "stale allowlist entry $sidebar" || status=1
  git -C "$self_test_dir" checkout -q -- rust web

  return "$status"
}

case "${1:-}" in
  '') check_root "$PWD" ;;
  --self-test) self_test ;;
  *)
    printf 'usage: dev/check-session-origin.sh [--self-test]\n' >&2
    exit 2
    ;;
esac

#!/usr/bin/env bash
# Runs scripts/release/trial.sh --serve through its socket preflight with stubs for the CharlieBot CLI and charlie-code.
# The server is a copy of /bin/true, which exits at once, so a case that passes the preflight ends at the server start
# (exit 1), and a case that fails it ends before anything is written (exit 2).
#
#   dev/release/trial-sockets.test.sh
#
# The default-tree case uses the round's TMPDIR, /tmp/improve-ux-tmp. The test creates that directory when it is absent
# and removes it afterwards only in that case.
set -uo pipefail

REPO=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
TRIAL="$REPO/scripts/release/trial.sh"
SOCKET_LIMIT=$(sed -n 's/^readonly SUN_PATH_LIMIT=//p' "$TRIAL")
if [[ ! $SOCKET_LIMIT =~ ^[0-9]+$ ]]; then
  printf 'trial.sh does not declare its socket-path limit\n' >&2
  exit 2
fi
ROUND_TMPDIR=/tmp/improve-ux-tmp
USER_ID=$(id -u)
COMMIT=$(git -C "$REPO" rev-parse --verify --quiet 'origin/main^{commit}') \
  || { printf 'the test needs origin/main in %s\n' "$REPO" >&2; exit 2; }
HEX=${COMMIT:0:12}
SANDBOX=$(mktemp -d "${TMPDIR:-/tmp}/trial-sockets.XXXXXX") || exit 2
created_round_tmpdir=0
cleanup() {
  rm -rf -- "$SANDBOX"
  if (( created_round_tmpdir == 1 )); then
    rmdir -- "$ROUND_TMPDIR"
  fi
}
trap cleanup EXIT

mkdir -p -- "$SANDBOX/bin" "$SANDBOX/release"
cat >"$SANDBOX/bin/charlie-code" <<'STUB'
#!/bin/bash
exit 0
STUB
# The CLI beside the server: the verbs trial.sh calls before the server starts.
cat >"$SANDBOX/release/charliebot" <<'STUB'
#!/bin/bash
case "$*" in
  'release-helper --help') ;;
  'config get server.enable_local_work_guard') echo false ;;
  'backend resolve --call probe') echo '{"option":{"id":"stub-probe","model":"stub-model","api_base":"http://127.0.0.1:9/v1"}}' ;;
  'release-helper stand-in-options '*) printf -- '- id: stub-stand-in\n  type: cc-claude\n  model: stub-model\n' ;;
  'release-helper session-weights '*) printf 'true\ntrue\n' ;;
  'release-helper free-port '*) echo 18650 ;;
  'release-helper redact-stdin') cat ;;
  *) echo "charliebot stub: unexpected arguments: $*" >&2; exit 1 ;;
esac
STUB
cp -- "$(type -P true)" "$SANDBOX/release/charliebot-server" || exit 2
chmod 755 "$SANDBOX/bin/charlie-code" "$SANDBOX/release/charliebot"

# run_trial TMPDIR OUTPUT [trial options...]: one serve trial of COMMIT with the stubs first on PATH. The exit code is the
# trial's.
run_trial() {
  local tmpdir=$1 output=$2
  shift 2
  env PATH="$SANDBOX/bin:$PATH" TMPDIR="$tmpdir" CHARLIEBOT_HOME="$SANDBOX/live-home" \
    CHARLIEBOT_RELEASE_RUN_DIR="$SANDBOX/evidence" \
    bash "$TRIAL" --serve "$@" "$COMMIT" "$SANDBOX/release/charliebot-server" -- true >"$output" 2>&1
}

failures=0
# check DESCRIPTION COMMAND...: runs COMMAND, prints its result and counts a failure.
check() {
  local description=$1
  shift
  if "$@"; then
    printf 'ok    %s\n' "$description"
  else
    printf 'FAIL  %s\n' "$description"
    failures=$((failures + 1))
  fi
}

# ---- predicates over a trial's output -----------------------------------------------------------------------------------

# passed_socket OUTPUT PATH: the trial's preflight printed a pass line for PATH.
passed_socket() {
  grep -qF "PREFLIGHT PASS socket: $2 (" "$1"
}

# refused_with_socket OUTPUT PATH BYTES: the preflight refusal names PATH and its byte count.
refused_with_socket() {
  grep -F 'preflight failed' "$1" | grep -qF "$2 ($3 bytes)"
}

# under_socket_limit BYTES: a byte count was read and it is under trial.sh's socket-path limit.
under_socket_limit() {
  [[ -n $1 ]] && (( $1 < SOCKET_LIMIT ))
}

# reached_server_start OUTPUT: the trial got past the preflight and failed at its server start, not at a refusal.
reached_server_start() {
  grep -qF 'FAIL serve Startup' "$1" && ! grep -qF 'preflight failed' "$1"
}

path_is_absent() {
  [[ ! -e $1 ]]
}

root_is_empty() {
  [[ -z $(ls -A -- "$1") ]]
}

# ---- the default tree under the round's TMPDIR -------------------------------------------------------------------------

if [[ ! -d $ROUND_TMPDIR ]]; then
  mkdir -m 700 -- "$ROUND_TMPDIR" || exit 2
  created_round_tmpdir=1
fi
tree="$ROUND_TMPDIR/charliebot-release-trial-$USER_ID-$HEX"
temp="$ROUND_TMPDIR/trial-tmp-$USER_ID-$HEX"
chrome_socket="$temp/org.chromium.Chromium.XXXXXX/SingletonSocket"
run_trial "$ROUND_TMPDIR" "$SANDBOX/default.out" --in-caller-scope
default_code=$?
default_out="$SANDBOX/default.out"

check "the default tree passes the preflight and reaches the server start" reached_server_start "$default_out"
check "the default tree's trial exits 1 at the server start" [ "$default_code" -eq 1 ]
check "the default tree's tmux socket passes" passed_socket "$default_out" "$tree/tmux/tmux-$USER_ID/charliebot"
check "the default tree's Chrome singleton socket passes" passed_socket "$default_out" "$chrome_socket"
chrome_line=$(grep -F "PREFLIGHT PASS socket: $chrome_socket (" "$default_out")
chrome_bytes=${chrome_line##* (}
chrome_bytes=${chrome_bytes%% bytes)}
check "the Chrome singleton socket path under the trial's TMPDIR is under the socket-path limit" \
  under_socket_limit "$chrome_bytes"
printf '      Chrome singleton socket under the trial TMPDIR: %s (%s bytes)\n' "$chrome_socket" "$chrome_bytes"
check "the default tree and its temp root are removed when the run ends" path_is_absent "$tree"
check "the default tree's temp root is removed when the run ends" path_is_absent "$temp"

# ---- a base that is too long refuses the run ----------------------------------------------------------------------------

long_base="$SANDBOX/$(printf 'long-%.0s' {1..10})"
mkdir -p -- "$long_base"
long_temp="$long_base/trial-tmp-$USER_ID-$HEX"
long_chrome_socket="$long_temp/org.chromium.Chromium.XXXXXX/SingletonSocket"
long_chrome_bytes=$(printf '%s' "$long_chrome_socket" | wc -c)
run_trial "$long_base" "$SANDBOX/long.out" --in-caller-scope
long_code=$?
check "a long base refuses the run with exit 2" [ "$long_code" -eq 2 ]
check "the refusal names the Chrome singleton socket path and its byte count" \
  refused_with_socket "$SANDBOX/long.out" "$long_chrome_socket" "$long_chrome_bytes"
check "the refused run wrote no tree or temp root" path_is_absent "$long_base/charliebot-release-trial-$USER_ID-$HEX"
check "the refused run wrote no temp root" path_is_absent "$long_temp"

# ---- --tree-root keeps its temp root inside the tree ---------------------------------------------------------------------

root="$SANDBOX/tree-root"
mkdir -p -- "$root"
root_chrome_socket="$root/trial-$HEX/tmp/org.chromium.Chromium.XXXXXX/SingletonSocket"
run_trial "$SANDBOX" "$SANDBOX/root.out" --in-caller-scope --tree-root "$root"
root_code=$?
check "--tree-root passes the preflight and exits 1 at the server start" \
  [ "$root_code" -eq 1 ]
check "--tree-root's trial reaches the server start" reached_server_start "$SANDBOX/root.out"
check "--tree-root's Chrome singleton socket passes under its tree" passed_socket "$SANDBOX/root.out" "$root_chrome_socket"
check "--tree-root removes its tree" root_is_empty "$root"

printf 'trial-sockets: %s failure(s)\n' "$failures"
(( failures == 0 ))

#!/usr/bin/env bash
# The release lab: one run of CharlieBot's release flow, from the start of scripts/release.sh to the server_ready of a
# stand-in production server, timed and printed as one JSON line.
#
#   dev/release-lab/run.sh --from <commit> --to <commit> --flow-ref <commit> --runs N --readers K
#       run the flow N times and print one JSON line per attempt
#   dev/release-lab/run.sh --mode starts --to <commit> [--flow-ref <commit>] --runs 3 --readers 0,1,4,8
#       print one JSON line per server start on the seed home, cold page cache, under K background readers
#   dev/release-lab/run.sh --prepare --to <commit>
#       write the lab's generated seed home, its git origin and flow checkout, its tools, its live home and its reader file
#   dev/release-lab/run.sh --teardown
#       delete the lab's copy of the model key and every stand-in home it wrote
#
# The stand-in production server is the mock home that scripts/release/startup-lab.sh generate writes (seed 1, a live
# process for each live run), served by the --from release from a tmux pane on the lab's own socket and port. The flow's
# scripts run against it through environment overrides: CHARLIEBOT_HOME, CHARLIEBOT_RELEASES_DIR,
# CHARLIEBOT_RELEASE_LOG_DIR, CHARLIEBOT_LOG_DIR and CHARLIEBOT_TMUX_SOCKET name the lab's own paths.
#
# The lab runs either shape of the flow. A --flow-ref whose release.sh runs a trial and a rehearsal gets those two stage
# scripts from it, with the lab's override applied, and CHARLIEBOT_TRIAL_LIVE_URL pointing the trial's live-server probe
# at the stand-in; a --flow-ref whose release.sh runs neither gets neither stage script. The JSON line's model_stages
# says which shape ran, and its probe_s and delegation_s time the two checks that need a model.
#
# The JSON line's stages: download, units, install, then the switch's preflight_s, gate_s, stop_s, start_s, and
# after server_ready the probe_s and delegation_s. downtime_s is the port's unserved span, from stopping the old server
# to the last start reaching ready; a switch back serves the port again at its own start. release_to_live_s runs from
# the release's start to that same ready.
#
# The lab never reads or writes a live CharlieBot home, release root or tmux server, and it copies no real session
# directory: every home under it is generated or a copy of the generated seed. The model key reaches only the lab's
# stand-in home and its startup-lab live home, both mode 600; --teardown deletes the lab's copy.
#
# Host-specific values arrive as environment, never from this file:
#   RELEASE_LAB_DIR             the lab's state directory (required)
#   RELEASE_LAB_REPO_URL        the charlie-bot clone URL the lab's git origin comes from (required for --prepare)
#   RELEASE_LAB_API_BASE        the model endpoint the stand-in home's backend names (required)
#   RELEASE_LAB_CREDENTIAL      the credentials.yaml section that holds that endpoint's key (required)
#   RELEASE_LAB_KEY_FILE        a mode-600 file holding the endpoint's key (required)
#   RELEASE_LAB_PORT            the stand-in server's port (default 18900)
#   RELEASE_LAB_TMUX_SOCKET     the lab's tmux socket name (default release-lab)
#   RELEASE_LAB_SCOPE_MEMORY    the lab's systemd user scope memory cap (default 24G)
#   RELEASE_LAB_MODEL           the stand-in backend's model (default openai/glm-5.3-flash)
#   RELEASE_LAB_READER_SECONDS  the cap on how long the flow's readers run from the gate's start (default 1800); they
#                               stop at the gate's first waiting sample
#   RELEASE_LAB_READER_SLEEP_MS the pause between one reader's 1 MiB reads (default 12)
#   RELEASE_LAB_KEEP            keep the run's stand-in home and logs for inspection (default 0)
#
# Exit: 0 every requested run completed with a live verdict; 1 a run failed or a cleanup failed; 2 the lab refused to
# start.
set -uo pipefail
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_TREE="$(cd -- "$SCRIPT_DIR/../.." && pwd -P)"
SELF="$SCRIPT_DIR/$(basename -- "${BASH_SOURCE[0]}")"
REAL_GH=$(command -v gh 2> /dev/null || true)
ORIGINAL_ARGS=("$@")

usage() {
  awk 'NR > 1 && /^set -/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$SELF"
}

log() { printf '%s release-lab: %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }
fail() {
  printf 'release-lab: %s\n' "$1" >&2
  exit 2
}
ms_now() { date +%s%3N; }
json_num() {
  if [ -n "${1:-}" ]; then printf '%s' "$1"; else printf 'null'; fi
}
json_bool() {
  case ${1:-} in
    1 | true) printf 'true' ;;
    0 | false) printf 'false' ;;
    *) printf 'null' ;;
  esac
}
elapsed_s() {
  awk -v a="${1:-}" -v b="${2:-}" 'BEGIN { if (a != "" && b != "" && a > 0 && b > 0) printf "%.3f", (b - a) / 1000 }'
}

MODE=flow
ACTION=run
FROM=
TO=
FLOW_REF=
RUNS=1
MAX_ATTEMPTS=
READERS=1
while [ $# -gt 0 ]; do
  case $1 in
    --from) FROM=${2:-}; shift 2 ;;
    --to) TO=${2:-}; shift 2 ;;
    --flow-ref) FLOW_REF=${2:-}; shift 2 ;;
    --runs) RUNS=${2:-}; shift 2 ;;
    --readers) READERS=${2:-}; shift 2 ;;
    --max-attempts) MAX_ATTEMPTS=${2:-}; shift 2 ;;
    --mode) MODE=${2:-}; shift 2 ;;
    --prepare) ACTION=prepare; shift ;;
    --teardown) ACTION=teardown; shift ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      printf 'run.sh: unknown argument %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

# ---- the lab's values and paths --------------------------------------------------------------------------------------

# require_env: the values only the host knows. Nothing here names a host or a path of one.
require_env() {
  local name
  for name in RELEASE_LAB_DIR RELEASE_LAB_API_BASE RELEASE_LAB_CREDENTIAL RELEASE_LAB_KEY_FILE; do
    [ -n "${!name:-}" ] || fail "$name is unset; the host supplies it"
  done
  LAB=${RELEASE_LAB_DIR}
  [[ $LAB == /* ]] || fail "RELEASE_LAB_DIR is an absolute path: $LAB"
  [[ $LAB != / && $LAB != "$HOME" ]] || fail "RELEASE_LAB_DIR names a directory of its own: $LAB"
  KEY_FILE=$RELEASE_LAB_KEY_FILE
  [[ $KEY_FILE == /* ]] || fail "RELEASE_LAB_KEY_FILE is an absolute path: $KEY_FILE"
  PORT=${RELEASE_LAB_PORT:-18900}
  SOCK=${RELEASE_LAB_TMUX_SOCKET:-release-lab}
  SCOPE_MEMORY=${RELEASE_LAB_SCOPE_MEMORY:-24G}
  MODEL=${RELEASE_LAB_MODEL:-openai/glm-5.3-flash}
  READER_SECONDS=${RELEASE_LAB_READER_SECONDS:-1800}
  READER_SLEEP_MS=${RELEASE_LAB_READER_SLEEP_MS:-12}
  KEEP=${RELEASE_LAB_KEEP:-0}
  BACKEND=charlie-code-release-lab
  [[ $PORT =~ ^[0-9]+$ ]] || fail "RELEASE_LAB_PORT is a number: $PORT"
  [[ $READER_SECONDS =~ ^[0-9]+$ && $READER_SLEEP_MS =~ ^[0-9]+$ ]] \
    || fail 'RELEASE_LAB_READER_SECONDS and RELEASE_LAB_READER_SLEEP_MS are whole numbers'
}

lab_paths() {
  STARTUP="$LAB/startup"
  SEED="$STARTUP/home"
  FLOW="$LAB/flow"
  ORIGIN="$LAB/origin.git"
  RELEASES="$LAB/releases"
  REL_LOGS="$LAB/logs/release"
  SERVER_LOGS="$LAB/logs/server"
  RUNS_DIR="$LAB/runs"
  STANDIN="$LAB/standin"
  PATH_DIR="$LAB/path"
  READER_FILE="$LAB/reader.bin"
  UNIT_DIR=${CHARLIEBOT_SYSTEMD_USER_DIR:-$HOME/.config/systemd/user}
  TOOLS="$LAB/bin/$TO"
  FROM_TOOLS="$LAB/bin/$FROM"
}

# enter_scope: run the lab inside a systemd user scope with a memory cap, so a runaway server or trial is reclaimed
# instead of pressing the host into swap. RELEASE_LAB_IN_SCOPE=1 marks the inner run.
enter_scope() {
  [ "${RELEASE_LAB_IN_SCOPE:-0}" = 1 ] && return 0
  [ "$ACTION" = teardown ] && return 0
  command -v systemd-run > /dev/null || fail 'systemd-run is on PATH for the lab scope'
  export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
  export RELEASE_LAB_IN_SCOPE=1
  log "entering the lab scope with MemoryMax=$SCOPE_MEMORY"
  exec systemd-run --user --scope --quiet -p "MemoryMax=$SCOPE_MEMORY" \
    --unit="release-lab-$(date -u +%Y%m%dT%H%M%SZ)-$$" -- "$SELF" "${ORIGINAL_ARGS[@]}"
}

# ---- preparation -----------------------------------------------------------------------------------------------------

# lab_write_config HOME ACCESS_KEY: the home's config, credentials and approval policy table. The backend is the flow's
# own model backend: charlie-code on the lab's endpoint, so the trial, the rehearsal and the switch's checks all run the
# same model.
lab_write_config() {
  local home=$1 access_key=$2
  cat > "$home/config.yaml" <<EOF
server:
  host: 127.0.0.1
  port: $PORT
  subprocess_buffer_limit_mb: 1024
  session_memory_max_mb: 1024
  session_swap_max_mb: 0
  session_tasks_max: 256
  min_free_disk_gib: 0
  enable_session_cpu_weight: ${RELEASE_LAB_CPU_WEIGHT:-true}
  enable_session_io_weight: ${RELEASE_LAB_IO_WEIGHT:-false}
  enable_local_work_guard: ${RELEASE_LAB_GUARD:-true}
paths:
  workspace_dirs:
    - $LAB/workspace
  worktree_dir: $LAB/worktrees
backends:
  calls:
    worker: $BACKEND
    improve: $BACKEND
    verify: $BACKEND
    review: $BACKEND
    cold_read: $BACKEND
    probe: $BACKEND
    autoname: $BACKEND
    triage: $BACKEND
    thread: $BACKEND
    new_session: $BACKEND
  options:
    - id: $BACKEND
      label: Release lab
      type: charlie-code
      model: $MODEL
      api_base: $RELEASE_LAB_API_BASE
      credential: $RELEASE_LAB_CREDENTIAL
EOF
  chmod 600 "$home/config.yaml" || fail "the config is user-only: $home/config.yaml"
  {
    printf '%s:\n  api_key: %s\n' "$RELEASE_LAB_CREDENTIAL" "$(cat "$KEY_FILE")"
    printf 'charliebot:\n  access_key: %s\n' "$access_key"
  } > "$home/credentials.yaml" || fail "the credentials write: $home/credentials.yaml"
  chmod 600 "$home/credentials.yaml"
}

# lab_write_gh: the stand-in for gh, which answers release.sh's CI preflight and serves the lab's built artifact as its
# download. The lab's own build stands in for CI, and the JSON line marks that stage lab-local.
lab_write_gh() {
  mkdir -p "$PATH_DIR" || fail "the lab's path directory writes: $PATH_DIR"
  cat > "$PATH_DIR/gh" <<'GH'
#!/usr/bin/env bash
# The release lab's stand-in for gh: release.sh's CI preflight and artifact download answered from the lab's built files,
# so a run measures the flow's own work. Every other invocation goes to the real gh named by RELEASE_LAB_REAL_GH.
set -uo pipefail
artifact=${RELEASE_LAB_ARTIFACT_DIR:-}
run_id=${RELEASE_LAB_RUN_ID:-lab}
real=${RELEASE_LAB_REAL_GH:-}
passthrough() {
  if [ -n "$real" ] && [ -x "$real" ]; then exec "$real" "$@"; fi
  echo 'gh: the stand-in answers only the release flow CI preflight and download' >&2
  exit 1
}
case "${1:-} ${2:-}" in
  'auth status') exit 0 ;;
  'repo view') printf 'release-lab/release-lab\n'; exit 0 ;;
  'run list') printf '%s\n' "$run_id"; exit 0 ;;
  'run view')
    if [ "${3:-}" = "$run_id" ]; then printf 'true\n'; else passthrough "$@"; fi
    ;;
  'run download')
    if [ -z "$artifact" ]; then echo 'gh: RELEASE_LAB_ARTIFACT_DIR is unset' >&2; exit 1; fi
    shift 2
    dir= name=
    while [ $# -gt 0 ]; do
      case $1 in
        --dir) dir=${2:-}; shift 2 ;;
        --name) name=${2:-}; shift 2 ;;
        --repo) shift 2 ;;
        *) shift ;;
      esac
    done
    if [ -z "$dir" ] || [ -z "$name" ]; then echo 'gh: run download needs --name and --dir' >&2; exit 1; fi
    if [ "${name#charliebot-server-}" != "${RELEASE_LAB_ARTIFACT_COMMIT:-}" ]; then
      echo "gh: $name is not this lab build's artifact" >&2
      exit 1
    fi
    mkdir -p "$dir" || exit 1
    cp "$artifact/charliebot" "$artifact/charliebot-server" "$artifact/release.log" "$artifact/sha256sums.txt" "$dir/" \
      || exit 1
    ;;
  *) passthrough "$@" ;;
esac
GH
  chmod 700 "$PATH_DIR/gh"
}

# prepare: the generated seed home, the lab's git origin and flow checkout, the installed tools, the startup lab's live
# home, the key file's presence and the reader file.
prepare() {
  [[ $TO =~ ^[0-9a-f]{40}$ ]] || fail "--prepare needs --to with a full commit sha: $TO"
  [ -n "${RELEASE_LAB_REPO_URL:-}" ] || fail 'RELEASE_LAB_REPO_URL is unset; --prepare clones the lab git origin from it'
  [ -f "$KEY_FILE" ] || fail "the model key file exists: $KEY_FILE"
  [ "$(stat -c %a "$KEY_FILE")" = 600 ] || fail "the model key file is user-readable only (mode 600): $KEY_FILE"
  local f
  for f in "$TOOLS/release/charliebot-server" "$TOOLS/release/charliebot" "$TOOLS/tools/mock-home" \
    "$TOOLS/tools/startup-lab-launcher" "$TOOLS/COMMIT"; do
    [ -e "$f" ] || fail "the build of $TO is installed (run dev/release-lab/build.sh): $f"
  done
  mkdir -p "$LAB" "$RELEASES" "$REL_LOGS" "$SERVER_LOGS" "$RUNS_DIR" "$STARTUP/tools" "$STARTUP/live-home" \
    "$STARTUP/runs" "$STARTUP/trials" "$LAB/workspace" "$LAB/worktrees" \
    || fail "the lab directories write under $LAB"
  chmod 700 "$LAB" "$RELEASES" "$REL_LOGS" "$SERVER_LOGS" "$RUNS_DIR"
  if [ ! -d "$ORIGIN" ]; then
    log "cloning the lab git origin from $RELEASE_LAB_REPO_URL"
    git clone --bare --quiet "$RELEASE_LAB_REPO_URL" "$ORIGIN" \
      || fail "the lab git origin clones from $RELEASE_LAB_REPO_URL"
  fi
  if [ ! -d "$FLOW" ]; then
    log 'cloning the lab flow checkout'
    git clone --quiet "$ORIGIN" "$FLOW" || fail "the lab flow checkout clones from $ORIGIN"
    git -C "$FLOW" remote set-url origin "$ORIGIN"
  fi
  cp "$TOOLS/tools/mock-home" "$TOOLS/tools/startup-lab-launcher" "$STARTUP/tools/" \
    || fail "the lab tools copy from $TOOLS/tools"
  startup_build_link
  if [ ! -d "$SEED" ]; then
    log 'generating the lab seed home'
    STARTUP_LAB_DIR="$STARTUP" "$FLOW/scripts/release/startup-lab.sh" generate \
      || fail 'the lab seed home generates'
  fi
  lab_write_config "$STARTUP/live-home" "$(od -An -N32 -tx1 /dev/urandom | tr -d '[:space:]')"
  lab_write_gh
  if [ ! -s "$READER_FILE" ]; then
    log "choosing the reader file under $SEED"
    find "$SEED" -type f -size +8M -printf '%s %p\n' 2> /dev/null | sort -rn | head -1 | cut -d' ' -f2- \
      > "$READER_FILE" || fail "the reader file path writes: $READER_FILE"
    [ -s "$READER_FILE" ] || fail "the seed home holds a file of 8 MiB or more for the readers: $SEED"
  fi
  printf '{"kind":"prepare","to":"%s","seed":"%s","flow":"%s","reader_file":"%s"}\n' "$TO" "$SEED" "$FLOW" \
    "$(cat "$READER_FILE")"
}

# teardown: delete the lab's copy of the model key and the homes it wrote. The report names the key file's path.
teardown() {
  rm -f "$KEY_FILE"
  rm -rf "$LAB/standin" "$LAB/startup/live-home/credentials.yaml"
  printf '{"kind":"teardown","key_deleted":"%s"}\n' "$KEY_FILE"
}

# ---- the run's own preparation ---------------------------------------------------------------------------------------

require_tools() {
  local tool
  for tool in git tmux systemd-run ss curl jq sha256sum flock dd setsid ps patch; do
    command -v "$tool" > /dev/null || fail "$tool is on PATH"
  done
  command -v charlie-code > /dev/null \
    || fail 'charlie-code is on PATH; the flow runs its model checks with it'
}

# lab_stage_copy SOURCE DEST MARK PATCH: DEST, in the flow checkout, is the flow-ref's stage script SOURCE. A flow-ref that
# predates the lab's overrides lacks MARK, so PATCH, the lab's override delta, applies to the copy. A PATCH that does not
# apply stops the run, so a copy differs from its flow-ref's script by the override alone.
lab_stage_copy() {
  local source=$1 dest=$2 mark=$3 patch=$4
  cp "$FLOW/$source" "$FLOW/$dest" || fail "the flow-ref's $source copies to $dest"
  if ! grep -q -- "$mark" "$FLOW/$dest"; then
    patch --batch --quiet "$FLOW/$dest" "$patch" || fail "the lab override $patch applies to $FLOW_REF's $source"
  fi
}

# reset_flow_checkout: the flow checkout at --flow-ref, clean in its tracked files. A flow ref whose release.sh runs the
# trial and the rehearsal gets the lab's two stage-script copies beside the flow's own, each the flow-ref's stage script
# with the lab's override applied; a flow ref without them gets no copies, and a stale one from an earlier run is
# removed. The copies are untracked, so the switch's clean-clone check still passes. FLOW_MODEL_STAGES says which shape
# this flow has. SWITCH_SCRIPT is the switch the run times: the flow-ref's own file when it carries the lab's mark
# lines, otherwise the lab checkout's copy of it.
reset_flow_checkout() {
  git -C "$FLOW" fetch --quiet origin || fail "the flow checkout fetches $ORIGIN"
  git -C "$FLOW" reset --hard --quiet "$FLOW_REF" || fail "the flow checkout moves to $FLOW_REF"
  FLOW_MODEL_STAGES=0
  if grep -q 'CHARLIEBOT_RELEASE_TRIAL' "$FLOW/scripts/release.sh"; then
    FLOW_MODEL_STAGES=1
    lab_stage_copy scripts/release/trial.sh scripts/release/trial-lab.sh CHARLIEBOT_TRIAL_LIVE_URL \
      "$REPO_TREE/dev/release-lab/overrides/trial.patch"
    lab_stage_copy scripts/release/switch-rehearsal.sh scripts/release/rehearsal-lab.sh CHARLIEBOT_REHEARSAL_PORT_MIN \
      "$REPO_TREE/dev/release-lab/overrides/switch-rehearsal.patch"
  else
    rm -f "$FLOW/scripts/release/trial-lab.sh" "$FLOW/scripts/release/rehearsal-lab.sh"
  fi
  SWITCH_SCRIPT="$FLOW/scripts/switch-server.sh"
  if ! grep -q 'lab_mark gate_start' "$SWITCH_SCRIPT"; then
    # A flow-ref older than the lab's marks still needs its switch timed. The lab's copy must be that release's file plus
    # the mark lines and the host release lock the current switch sources, so nothing else about the switch changes; any
    # other difference fails the run. The switch re-executes itself as switch-server.sh beside its slice-unit install, so
    # the copy keeps both names in a directory of its own, with the lock file the copy sources beside it.
    local unexpected
    unexpected=$(diff "$SWITCH_SCRIPT" "$REPO_TREE/scripts/switch-server.sh" \
      | awk '/^[<>]/ && $0 !~ /lab_mark|LAB_MARK|lock|LOCK/ && NF > 1 && $2 !~ /^[{}]$/') || true
    [ -z "$unexpected" ] \
      || fail "the lab switch copy differs from $FLOW_REF's switch by more than its mark lines and its release lock: $unexpected"
    mkdir -p "$FLOW/scripts/lab-switch/release" \
      && cp "$REPO_TREE/scripts/switch-server.sh" "$FLOW/scripts/lab-switch/switch-server.sh" \
      && cp "$FLOW/scripts/install-slice-units.sh" "$FLOW/scripts/lab-switch/install-slice-units.sh" \
      && cp "$REPO_TREE/scripts/release/lock.sh" "$FLOW/scripts/lab-switch/release/lock.sh" \
      || fail "the lab switch copy installs into $FLOW/scripts/lab-switch"
    SWITCH_SCRIPT="$FLOW/scripts/lab-switch/switch-server.sh"
  fi
}

# check_commits: every commit the run names is in the lab's origin.
check_commits() {
  local commit
  for commit in "$FROM" "$TO" "$FLOW_REF"; do
    [ -n "$commit" ] || continue
    [[ $commit =~ ^[0-9a-f]{40}$ ]] || fail "a commit argument is a full sha: '$commit'"
    git -C "$FLOW" cat-file -e "$commit^{commit}" 2> /dev/null \
      || fail "commit $commit is not in the lab origin; clone or fetch it first"
  done
}

# install_from_release: the stand-in's release directory, built from the lab's build of --from, with current on it, and
# no release of --to left from an earlier run.
install_from_release() {
  local want="$RELEASES/$FROM"
  if [ ! -d "$want" ] || ! (cd "$want" && sha256sum -c sha256sums.txt) > /dev/null 2>&1; then
    rm -rf "$want"
    mkdir -p "$want"
    cp "$FROM_TOOLS/release/charliebot" "$FROM_TOOLS/release/charliebot-server" "$FROM_TOOLS/release/release.log" \
      "$FROM_TOOLS/release/sha256sums.txt" "$want/" || fail "the $FROM release copies into $RELEASES"
  fi
  rm -rf "${RELEASES:?}/$TO"
  ln -sfn "$FROM" "$RELEASES/current"
}

# install_flow_units: the flow checkout's slice units, so the launcher's preflight and the weight-controller check read
# the units the checkout itself carries. The agents' slice starts here, as scripts/setup.sh starts it on a live host.
install_flow_units() {
  "$FLOW/scripts/install-slice-units.sh" "$FLOW" "$FLOW_REF" > "$RUNS_DIR/slice-units.log" 2>&1 \
    || fail "the flow checkout's slice units install into $UNIT_DIR; see $RUNS_DIR/slice-units.log"
  systemctl --user enable --now charliebot-agents.slice > /dev/null 2>&1 \
    || fail 'the agents slice starts in the user manager'
}

# ---- the stand-in production server ----------------------------------------------------------------------------------

stop_live_runs() {
  local pid
  [ -f "$STANDIN/live-pids" ] || return 0
  while read -r pid; do
    [ -n "$pid" ] || continue
    kill -TERM "$pid" 2> /dev/null || true
  done < "$STANDIN/live-pids"
  rm -f "$STANDIN/live-pids"
}

# standin_make: a fresh copy of the generated seed as a served home, the lab config and credentials, and a live process
# for each of the seed's live runs, so the first start re-attaches them as a release restart does. The run records name
# the lab's backend, which is in the home's backends.options, so the switch's preflight reads their re-attachment.
standin_make() {
  local entries=() session run pid attempt
  rm -rf "$STANDIN"
  mkdir -p "$STANDIN/sessions" "$STANDIN/state" || fail "the stand-in home writes: $STANDIN"
  chmod 700 "$STANDIN" "$STANDIN/sessions" "$STANDIN/state"
  shopt -s nullglob
  entries=("$SEED"/*/)
  shopt -u nullglob
  ((${#entries[@]} > 0)) || fail "the seed home holds session directories: $SEED"
  cp -a -- "${entries[@]}" "$STANDIN/sessions/" || fail "the stand-in home's sessions copy from $SEED"
  if [ -f "$SEED/boot_recovery.json" ]; then
    cp -a -- "$SEED/boot_recovery.json" "$STANDIN/state/boot_recovery.json" || fail 'the seed boot marker copies'
  fi
  lab_write_config "$STANDIN" "$(od -An -N32 -tx1 /dev/urandom | tr -d '[:space:]')"
  : > "$STANDIN/live-pids"
  [ -s "$SEED/live-runs.txt" ] || fail "the seed lists its live runs: $SEED/live-runs.txt"
  while read -r session run; do
    [ -n "$session" ] && [ -n "$run" ] || continue
    jq --arg backend "$BACKEND" '.backend = $backend' \
      "$STANDIN/sessions/$session/data/runs/$run/metadata.json" > "$STANDIN/run.json" \
      && mv "$STANDIN/run.json" "$STANDIN/sessions/$session/data/runs/$run/metadata.json" \
      || fail "the live run record of $session/$run names the lab backend"
    # The boot's resume of a live run's turn resolves the session's backend: one the config does not list fails the
    # resume and finishes the run, so the session record names the lab backend too.
    jq --arg backend "$BACKEND" '.backend = $backend' \
      "$STANDIN/sessions/$session/metadata.json" > "$STANDIN/run.json" \
      && mv "$STANDIN/run.json" "$STANDIN/sessions/$session/metadata.json" \
      || fail "the session record of $session names the lab backend"
    # The live process must outlive the whole flow: the old server ends a run whose process ends, and the switch then
    # finds no live run to re-attach. standin_stop ends every one of them.
    setsid sleep 10800 < /dev/null > /dev/null 2>&1 &
    pid=$!
    for ((attempt = 0; attempt < 100; attempt++)); do
      [ "$(cat "/proc/$pid/comm" 2> /dev/null)" = sleep ] && break
      sleep 0.05
    done
    [ "$(cat "/proc/$pid/comm" 2> /dev/null)" = sleep ] || fail "the live process for $session/$run starts"
    printf '%s\n' "$pid" >> "$STANDIN/live-pids"
    "$TOOLS/tools/mock-home" recovery-attach "$STANDIN/sessions" "$session" "$run" "$pid" \
      || fail "the live run record of $session/$run takes its process"
  done < "$SEED/live-runs.txt"
}

listener_pids() { ss -Hltnp "sport = :$PORT" 2> /dev/null | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u; }

# standin_start: the --from release serves the stand-in from the lab's tmux pane; this returns once its server_ready line
# is in a new launcher log. The tmux server starts with a minimal environment, so no variable of the caller's session
# reaches the stand-in's server; the pane keeps HOME, PATH (which carries charlie-code) and the user runtime directory.
standin_start() {
  local end=$((SECONDS + 300)) log_file
  rm -f "$SERVER_LOGS/server-latest.log"
  tmux -L "$SOCK" kill-server 2> /dev/null || true
  env -i HOME="$HOME" PATH="$PATH" TERM=xterm-256color TMPDIR="${TMPDIR:-/tmp}" \
    XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" \
    tmux -L "$SOCK" new-session -d -s lab -c "$FLOW" || fail "the lab tmux server starts on socket $SOCK"
  tmux -L "$SOCK" send-keys -t lab "cd $(printf %q "$FLOW") && env CHARLIEBOT_HOME=$(printf %q "$STANDIN") CHARLIEBOT_RELEASES_DIR=$(printf %q "$RELEASES") CHARLIEBOT_LOG_DIR=$(printf %q "$SERVER_LOGS") ./scripts/start-server.sh" Enter \
    || fail 'the stand-in launcher types into the lab pane'
  while ((SECONDS < end)); do
    log_file=$(readlink "$SERVER_LOGS/server-latest.log" 2> /dev/null || true)
    if [ -n "$log_file" ] && grep -qE '[[:space:]]server_ready[[:space:]]' "$log_file" 2> /dev/null; then
      log "the stand-in serves $PORT from $log_file"
      return 0
    fi
    tmux -L "$SOCK" has-session -t lab 2> /dev/null || {
      log 'the stand-in launcher exited before server_ready'
      return 1
    }
    sleep 0.2
  done
  log 'the stand-in did not reach server_ready within 300s'
  return 1
}

# standin_stop: the pane's launcher gets Ctrl-C; the port must lose its listener, and the tmux server and the live runs
# stop. A server that outlives Ctrl-C is stopped by its own unit and then killed by pid.
standin_stop() {
  local end=$((SECONDS + 90)) pid unit
  tmux -L "$SOCK" send-keys -t lab C-c 2> /dev/null || true
  while ((SECONDS < end)); do
    [ -z "$(listener_pids)" ] && break
    sleep 0.5
  done
  for pid in $(listener_pids); do
    unit=$(sed -n '2s|.*/||p' "/proc/$pid/cgroup" 2> /dev/null || true)
    log "a server on $PORT survived Ctrl-C; stopping pid $pid"
    [ -n "$unit" ] && systemctl --user stop "$unit" > /dev/null 2>&1
    kill -TERM "$pid" 2> /dev/null || true
    sleep 1
    kill -KILL "$pid" 2> /dev/null || true
  done
  tmux -L "$SOCK" kill-server 2> /dev/null || true
  stop_live_runs
}

# ---- readers ---------------------------------------------------------------------------------------------------------

READERS_PID=

# reader_target: the file the readers stream. It is the largest file of the seed home, so the readers need no file of
# their own and read one the boot also reads.
reader_target() { [ -s "$READER_FILE" ] && cat "$READER_FILE"; }

start_readers() {
  local count=$1 seconds=$2 target
  READERS_PID=
  ((count > 0)) || return 0
  target=$(reader_target)
  [ -n "$target" ] && [ -f "$target" ] || fail "the reader file exists; run --prepare: $READER_FILE"
  # The readers script is this shell's own child, so its pid names it until the script is reaped: a setsid wrapper can
  # fork, and the pid it leaves behind may belong to another process by the time the readers stop.
  "$SCRIPT_DIR/readers.sh" "$target" "$count" "$seconds" "$READER_SLEEP_MS" \
    > /dev/null 2>> "${RUN_DIR:-/dev/null}/readers.log" &
  READERS_PID=$!
}

stop_readers() {
  [ -n "${READERS_PID:-}" ] || return 0
  kill -TERM "$READERS_PID" 2> /dev/null || true
  wait "$READERS_PID" 2> /dev/null || true
  READERS_PID=
}

# ---- one flow run ----------------------------------------------------------------------------------------------------

# flow_vars: the environment the flow's scripts read. Every path is the lab's; the live-server probe points at the
# stand-in; a flow with the trial and rehearsal stages runs the lab's copies of them, which differ from the flow's only
# in overrides whose defaults keep production behavior.
flow_vars() {
  export CHARLIEBOT_HOME="$STANDIN"
  export CHARLIEBOT_RELEASES_DIR="$RELEASES"
  export CHARLIEBOT_RELEASE_LOG_DIR="$REL_LOGS"
  export CHARLIEBOT_LOG_DIR="$SERVER_LOGS"
  export CHARLIEBOT_TMUX_SOCKET="$SOCK"
  export CHARLIEBOT_REPO_ROOT="$FLOW"
  export CHARLIEBOT_SYSTEMD_USER_DIR="$UNIT_DIR"
  export CHARLIEBOT_TRIAL_LIVE_URL="http://127.0.0.1:$PORT"
  if [ "$FLOW_MODEL_STAGES" = 1 ]; then
    export CHARLIEBOT_RELEASE_TRIAL="$FLOW/scripts/release/trial-lab.sh"
    export CHARLIEBOT_RELEASE_REHEARSAL="$FLOW/scripts/release/rehearsal-lab.sh"
  else
    unset CHARLIEBOT_RELEASE_TRIAL CHARLIEBOT_RELEASE_REHEARSAL
  fi
  export TRIAL_PORT_MIN=${RELEASE_LAB_TRIAL_PORT_MIN:-18820}
  export TRIAL_PORT_MAX=${RELEASE_LAB_TRIAL_PORT_MAX:-18839}
  export CHARLIEBOT_REHEARSAL_PORT_MIN=${RELEASE_LAB_REHEARSAL_PORT_MIN:-18840}
  export CHARLIEBOT_REHEARSAL_PORT_MAX=${RELEASE_LAB_REHEARSAL_PORT_MAX:-18880}
  export RELEASE_LAB_ARTIFACT_DIR="$TOOLS/release"
  export RELEASE_LAB_ARTIFACT_COMMIT="$TO"
  export RELEASE_LAB_REAL_GH="$REAL_GH"
  export PATH="$PATH_DIR:$PATH"
  unset CHARLIE_CODE_API_KEY AIGW_API_KEY OPENAI_API_KEY ANTHROPIC_API_KEY
}

# stage_seconds FILE SINCE_MS: the wall time from SINCE_MS to FILE's last write, in seconds with three decimals.
stage_seconds() {
  local file=$1 since=$2 end
  [ -n "$since" ] && [ -f "$file" ] || return 0
  end=$(date -r "$file" +%s%3N 2> /dev/null) || return 0
  elapsed_s "$since" "$end"
}

# log_end FILE SINCE_MS: the epoch milliseconds of a stage log's last write, or SINCE_MS when the flow ran no such
# stage. A flow without the trial and rehearsal has no log for them, and the stages after them start where the one
# before them ended.
log_end() {
  local file=$1 since=$2 end
  if [ -f "$file" ] && end=$(date -r "$file" +%s%3N 2> /dev/null); then
    printf '%s' "$end"
  else
    printf '%s' "$since"
  fi
}

# newest_release_run: the run directory release.sh created, under the lab's log root.
newest_release_run() { ls -1dt "$REL_LOGS"/*/ 2> /dev/null | head -1; }

# release_start_ms DIR: the second the flow's own clock starts. release.sh creates the directory after its preflight,
# and the directory name holds that second.
release_start_ms() {
  local stamp
  stamp=$(basename "${1%/}")
  stamp=${stamp##*-}
  [[ $stamp =~ ^[0-9]{8}T[0-9]{6}Z$ ]] || return 1
  date -u -d "${stamp:0:4}-${stamp:4:2}-${stamp:6:2} ${stamp:9:2}:${stamp:11:2}:${stamp:13:2}" +%s%3N
}

# mark_value MARKS NAME: the epoch milliseconds of one lab mark in the switch log.
mark_value() { awk -v name="$2" '$1 == name { print $2; exit }' <<< "$1"; }
mark_value_last() { awk -v name="$2" '$1 == name { value = $2 } END { print value }' <<< "$1"; }

# check_seconds LOG CHECK: the seconds of the check's last logged step. A check logs its elapsed seconds from its own
# start at each step, and a check the switch retried once logs the retry's, so the last figure is the check's time. A
# check the switch never reached logs none.
check_seconds() {
  local log=$1 check=$2
  [ -f "$log" ] || return 0
  sed -n "s/.* switch: $check: .* elapsed=\([0-9][0-9]*\)s\$/\1/p" "$log" | tail -1
}

run_one_flow() {
  local run=$1 start_ms load1 proc_entries rel_start sw_start sw_rc switch_log switch_pid run_dir
  # Every stage field prints, so each starts empty: a run that stops before the switch has no switch stage to time.
  local download_s='' units_s='' trial_s='' rehearsal_s='' install_s='' preflight_s='' gate_s='' stop_s='' start_s=''
  local ready_in_ms read_bytes release_to_live_s release_stamp_ms live_runs reattached verdict switch_exit
  local gate_waits marks gate_start gate_end pre_preflight preflight_after_gate a probe_s delegation_s downtime_s
  local server_log_before readers_started

  start_ms=$(ms_now)
  load1=$(cut -d' ' -f1 /proc/loadavg)
  # The gate's read-table walks cost time in proportion to the /proc entry count, so the line records it at the start.
  proc_entries=$(find /proc -maxdepth 1 -name '[0-9]*' 2> /dev/null | wc -l)
  RUN_DIR="$RUNS_DIR/$(date -u +%Y%m%dT%H%M%SZ)-run$run"
  mkdir -p "$RUN_DIR" || fail "the run directory writes: $RUN_DIR"
  log "run $run: $FROM -> $TO, flow $FLOW_REF, readers $READERS, load $load1"

  reset_flow_checkout
  check_commits
  install_from_release
  install_flow_units
  standin_make
  if ! standin_start; then
    standin_stop
    [ "$KEEP" = 1 ] || rm -rf "$STANDIN"
    LAST_VERDICT=start_failed
    printf '{"kind":"flow","run":%s,"from":"%s","to":"%s","flow_ref":"%s","model_stages":%s,"download_s":null,"units_s":null,"trial_s":null,"rehearsal_s":null,"install_s":null,"preflight_s":null,"gate_s":null,"stop_s":null,"start_s":null,"probe_s":null,"delegation_s":null,"downtime_s":null,"ready_in_ms":null,"read_bytes":null,"release_to_live_s":null,"live_runs":null,"reattached":null,"verdict":"start_failed","switch_exit":null,"load1":%s,"proc_entries":%s,"readers":%s,"gate_wait_samples":null,"download_source":"lab"}\n' \
      "$run" "$FROM" "$TO" "$FLOW_REF" "$(json_bool "$([ "$FLOW_MODEL_STAGES" = 1 ] && echo true || echo false)")" \
      "$load1" "$proc_entries" "$READERS"
    return 1
  fi

  rel_start=$(ms_now)
  ( flow_vars && cd "$FLOW" && ./scripts/release.sh "$TO" ) > "$RUN_DIR/release.out" 2>&1
  local rel_rc=$?
  run_dir=$(newest_release_run)

  download_s=$(stage_seconds "$run_dir/download.log" "$rel_start")
  a=$(log_end "$run_dir/download.log" "$rel_start")
  units_s=$(stage_seconds "$run_dir/units.log" "$a")
  a=$(log_end "$run_dir/units.log" "$a")
  trial_s=$(stage_seconds "$run_dir/trial.log" "$a")
  a=$(log_end "$run_dir/trial.log" "$a")
  rehearsal_s=$(stage_seconds "$run_dir/rehearsal.log" "$a")
  a=$(log_end "$run_dir/rehearsal.log" "$a")
  install_s=$(stage_seconds "$run_dir/install.log" "$a")

  if ((rel_rc != 0)); then
    log "run $run: release.sh exited $rel_rc; see $RUN_DIR/release.out"
    standin_stop
    [ "$KEEP" = 1 ] || rm -rf "$STANDIN"
    LAST_VERDICT=flow_stopped
    printf '{"kind":"flow","run":%s,"from":"%s","to":"%s","flow_ref":"%s","model_stages":%s,"download_s":%s,"units_s":%s,"trial_s":%s,"rehearsal_s":%s,"install_s":%s,"preflight_s":null,"gate_s":null,"stop_s":null,"start_s":null,"probe_s":null,"delegation_s":null,"downtime_s":null,"ready_in_ms":null,"read_bytes":null,"release_to_live_s":null,"live_runs":null,"reattached":null,"verdict":"flow_stopped","switch_exit":null,"load1":%s,"proc_entries":%s,"readers":%s,"gate_wait_samples":null,"download_source":"lab"}\n' \
      "$run" "$FROM" "$TO" "$FLOW_REF" "$(json_bool "$([ "$FLOW_MODEL_STAGES" = 1 ] && echo true || echo false)")" \
      "$(json_num "$download_s")" "$(json_num "$units_s")" "$(json_num "$trial_s")" \
      "$(json_num "$rehearsal_s")" "$(json_num "$install_s")" "$load1" "$proc_entries" "$READERS"
    return 1
  fi

  # A flow whose release ran a rehearsal leaves the old release's units installed. The flow checkout sits at the
  # flow-ref commit, so the launcher's preflight must compare it with the same commit's units, as a live host compares
  # the live release's. A flow without the rehearsal stage re-installs the same units here.
  install_flow_units

  server_log_before=$(readlink "$SERVER_LOGS/server-latest.log" 2> /dev/null || true)
  sw_start=$(ms_now)
  : > "$RUN_DIR/switch.out"
  ( flow_vars && export CHARLIEBOT_SWITCH_LAB_MARKS=1 && "$SWITCH_SCRIPT" rust "$TO" ) \
    > "$RUN_DIR/switch.out" 2>&1 &
  switch_pid=$!
  switch_log=
  READERS_PID=
  readers_started=0
  while kill -0 "$switch_pid" 2> /dev/null; do
    if [ -z "$switch_log" ]; then
      switch_log=$(sed -n 's/^switch log: //p' "$RUN_DIR/switch.out" | head -1)
    fi
    if [ "$readers_started" = 0 ] && [ -n "$switch_log" ] && grep -q 'lab_mark=gate_start' "$switch_log" 2> /dev/null; then
      readers_started=1
      start_readers "$READERS" "$READER_SECONDS"
      log "run $run: $READERS reader(s) start at the gate"
    fi
    # The gate samples the disk only after its two read tables, and those take minutes on a host whose process table is
    # large. The readers therefore run until the first waiting sample is logged and then stop: that sample is theirs, and
    # the next one sees a quiet disk.
    if [ -n "$READERS_PID" ] && grep -q 'waiting for the root disk' "$switch_log" 2> /dev/null; then
      stop_readers
      log "run $run: readers stop at the gate's first waiting sample"
    fi
    sleep 0.2
  done
  wait "$switch_pid"
  sw_rc=$?
  stop_readers
  [ -n "$switch_log" ] || switch_log=$(sed -n 's/^switch log: //p' "$RUN_DIR/switch.out" | head -1)

  standin_stop

  marks=
  [ -n "$switch_log" ] && [ -f "$switch_log" ] \
    && marks=$(sed -n 's/.*lab_mark=\([a-z_]*\) at=\([0-9]*\).*/\1 \2/p' "$switch_log")
  gate_start=$(mark_value "$marks" gate_start)
  gate_end=$(mark_value "$marks" gate_end)
  pre_preflight=$(mark_value "$marks" switch_start)
  preflight_after_gate=$(mark_value "$marks" preflight_end)
  stop_s=$(elapsed_s "$(mark_value "$marks" stop_start)" "$(mark_value "$marks" stop_end)")
  start_s=$(elapsed_s "$(mark_value "$marks" start_start)" "$(mark_value "$marks" start_end)")
  gate_s=$(elapsed_s "$gate_start" "$gate_end")
  if [ -n "$pre_preflight" ] && [ -n "$gate_start" ] && [ -n "$gate_end" ] && [ -n "$preflight_after_gate" ]; then
    preflight_s=$(awk -v a="$pre_preflight" -v b="$gate_start" -v c="$gate_end" -v d="$preflight_after_gate" \
      'BEGIN { printf "%.3f", ((b - a) + (d - c)) / 1000 }')
  fi
  gate_waits=0
  [ -n "$switch_log" ] && [ -f "$switch_log" ] && gate_waits=$(grep -c 'waiting for the root disk' "$switch_log" || true)
  # The checks that need a model run after server_ready, so their times are the release path's own stages no longer.
  probe_s=$(check_seconds "${switch_log:-/nonexistent}" probe)
  delegation_s=$(check_seconds "${switch_log:-/nonexistent}" delegation)
  # Downtime is the port's unserved span: from stopping the old server to the last start the switch made reaching ready.
  # A switch back makes two starts, and the port is served again at the second one.
  downtime_s=$(elapsed_s "$(mark_value "$marks" stop_start)" "$(mark_value_last "$marks" start_end)")

  # The stand-in's server writes a log of its own; only a log the switch created carries the new server's reading.
  ready_in_ms=
  read_bytes=
  local server_log
  server_log=$(readlink "$SERVER_LOGS/server-latest.log" 2> /dev/null || true)
  if [ -n "$server_log" ] && [ "$server_log" != "$server_log_before" ]; then
    ready_in_ms=$(grep -oE '[[:space:]]ready_in_ms=[0-9]+' "$server_log" 2> /dev/null | tail -1 | cut -d= -f2)
    read_bytes=$(grep -oE '[[:space:]]read_bytes=[0-9]+' "$server_log" 2> /dev/null | tail -1 | cut -d= -f2)
  fi
  release_stamp_ms=$(release_start_ms "${run_dir:-}" || true)
  release_to_live_s=$(elapsed_s "${release_stamp_ms:-$rel_start}" "$(mark_value "$marks" start_end)")
  live_runs=
  [ -n "$switch_log" ] && [ -f "$switch_log" ] \
    && live_runs=$(grep -oE 'recorded [0-9]+ live run' "$switch_log" | head -1 | grep -oE '[0-9]+')
  reattached=
  switch_exit=$sw_rc
  if [ -n "$switch_log" ] && [ -f "$switch_log" ]; then
    if grep -qE 'readback: run [0-9a-zA-Z_-]+ (was not adopted|died during the switch)' "$switch_log"; then
      reattached=false
    elif ((sw_rc == 0 || sw_rc == 2)); then
      # The forward switch and the switch back both read back every recorded run before any model check ran.
      reattached=true
    fi
  fi
  case ${sw_rc:-} in
    0) verdict=live ;;
    2) verdict=switched_back ;;
    3 | 4 | 5 | 6) verdict=kept_reported ;;
    *) verdict=switch_failed ;;
  esac

  if [ "$KEEP" = 1 ]; then
    log "run $run: keeping $RUN_DIR, $SERVER_LOGS and $STANDIN"
  else
    rm -rf "$STANDIN"
  fi
  printf '{"kind":"flow","run":%s,"from":"%s","to":"%s","flow_ref":"%s","model_stages":%s,"download_s":%s,"units_s":%s,"trial_s":%s,"rehearsal_s":%s,"install_s":%s,"preflight_s":%s,"gate_s":%s,"stop_s":%s,"start_s":%s,"probe_s":%s,"delegation_s":%s,"downtime_s":%s,"ready_in_ms":%s,"read_bytes":%s,"release_to_live_s":%s,"live_runs":%s,"reattached":%s,"verdict":"%s","switch_exit":%s,"load1":%s,"proc_entries":%s,"readers":%s,"gate_wait_samples":%s,"download_source":"lab"}\n' \
    "$run" "$FROM" "$TO" "$FLOW_REF" "$(json_bool "$([ "$FLOW_MODEL_STAGES" = 1 ] && echo true || echo false)")" \
    "$(json_num "$download_s")" "$(json_num "$units_s")" "$(json_num "$trial_s")" \
    "$(json_num "$rehearsal_s")" "$(json_num "$install_s")" "$(json_num "$preflight_s")" "$(json_num "$gate_s")" \
    "$(json_num "$stop_s")" "$(json_num "$start_s")" "$(json_num "$probe_s")" "$(json_num "$delegation_s")" \
    "$(json_num "$downtime_s")" "$(json_num "$ready_in_ms")" "$(json_num "$read_bytes")" \
    "$(json_num "$release_to_live_s")" "$(json_num "$live_runs")" "$(json_bool "$reattached")" "$verdict" \
    "$(json_num "$switch_exit")" "$load1" "$proc_entries" "$READERS" "$gate_waits"
  LAST_VERDICT=$verdict
  return 0
}

# ---- the start matrix ------------------------------------------------------------------------------------------------

# startup_build_link: the build directory scripts/release/startup-lab.sh reads for the build it is asked to start, named
# `to`: the two binaries and the commit they were built from, as that script's own `install` writes them.
startup_build_link() {
  local bin="$STARTUP/bin-to"
  rm -rf "$bin"
  mkdir -p "$bin" || fail "the startup lab's build directory writes: $bin"
  ln -sfn "$TOOLS/release/charliebot-server" "$bin/charliebot-server" \
    && ln -sfn "$TOOLS/release/charliebot" "$bin/charliebot" \
    && ln -sfn "$TOOLS/COMMIT" "$bin/COMMIT" \
    || fail "the startup lab's build links write: $bin"
}

# startup_lab_layout: the entries scripts/release/startup-lab.sh reads, pointed at this lab's seed, tools and build.
startup_lab_layout() {
  startup_build_link
  [ -d "$SEED" ] || fail "the lab seed home exists; run --prepare: $SEED"
  [ -f "$STARTUP/live-home/config.yaml" ] || fail "the lab live home exists; run --prepare: $STARTUP/live-home"
}

# run_one_start: one cold start of --to on the seed home with K readers. The readers start when the trial creates its
# server log and stop at server_ready, so every start has the same schedule around the boot. The startup lab runs from
# the flow checkout, whose git history holds the commit its trial extracts.
run_one_start() {
  local readers=$1 run=$2 pid rc ready read_bytes void=false started=0
  local trial_tree="$STARTUP/trials/trial-${TO:0:12}"
  local out proc_entries
  load1=$(cut -d' ' -f1 /proc/loadavg)
  proc_entries=$(find /proc -maxdepth 1 -name '[0-9]*' 2> /dev/null | wc -l)
  RUN_DIR="$RUNS_DIR/$(date -u +%Y%m%dT%H%M%SZ)-start-r$readers-$run"
  mkdir -p "$RUN_DIR" || fail "the run directory writes: $RUN_DIR"
  rm -rf "$trial_tree"
  out="$RUN_DIR/startup-lab.out"
  log "start $run with $readers reader(s): $TO cold on the seed home, load $load1"

  ( STARTUP_LAB_DIR="$STARTUP" LAB_START=release "$FLOW/scripts/release/startup-lab.sh" time to cold "r$readers-$run" ) \
    > "$out" 2>&1 &
  pid=$!
  READERS_PID=
  while kill -0 "$pid" 2> /dev/null; do
    if [ "$started" = 0 ] && [ -s "$trial_tree/server-1.log" ]; then
      started=1
      start_readers "$readers" 600
    fi
    if [ -n "$READERS_PID" ] && grep -qE '[[:space:]]server_ready[[:space:]]' "$trial_tree/server-1.log" 2> /dev/null; then
      stop_readers
    fi
    sleep 0.05
  done
  wait "$pid"
  rc=$?
  stop_readers
  ready=$(sed -n 's/.*ready_in_ms=\([0-9]*\).*/\1/p' "$out" | tail -1)
  read_bytes=$(sed -n 's/.*disk_read_bytes=\([0-9]*\).*/\1/p' "$out" | tail -1)
  local void=false
  if [ "$rc" -ne 0 ] || [ -z "$ready" ]; then void=true; fi
  printf '{"kind":"start","to":"%s","cache":"cold","readers":%s,"run":%s,"ready_in_ms":%s,"read_bytes":%s,"load1":%s,"proc_entries":%s,"exit":%s,"void":%s}\n' \
    "$TO" "$readers" "$run" "$(json_num "$ready")" "$(json_num "$read_bytes")" "$load1" "$proc_entries" "$rc" "$void"
  [ "$void" = false ]
}

run_starts() {
  local counts=() k n attempt
  IFS=, read -r -a counts <<< "$READERS"
  for k in "${counts[@]}"; do
    [[ $k =~ ^[0-9]+$ ]] || fail "--readers in --mode starts is a comma-separated list of reader counts: $READERS"
    for ((n = 1; n <= RUNS; n++)); do
      for ((attempt = 1; attempt <= 3; attempt++)); do
        run_one_start "$k" "$n" && break
        log "start $n with $k reader(s) is void on attempt $attempt; starting it again"
      done
      [ "$attempt" -le 3 ] || BAD_RUNS=$((BAD_RUNS + 1))
    done
  done
}

# ---- cleanup ---------------------------------------------------------------------------------------------------------

# kill_scope_leftovers: every process in this process's cgroup that is not this shell or one of its ancestors.
kill_scope_leftovers() {
  local self_cgroup cgroup_dir protect p pid leftovers=0
  self_cgroup=$(sed -n 's/^0:://p' /proc/self/cgroup)
  # Only the lab's own scope is swept. A shell that set RELEASE_LAB_IN_SCOPE by hand shares its cgroup with other work.
  case $self_cgroup in
    */release-lab-*.scope) ;;
    *)
      log "not in a release-lab scope; leaving the processes of $self_cgroup alone"
      return 0
      ;;
  esac
  cgroup_dir="/sys/fs/cgroup$self_cgroup"
  [ -d "$cgroup_dir" ] || return 0
  protect=" $$ "
  p=$$
  while [ "$p" -gt 1 ]; do
    p=$(ps -o ppid= -p "$p" 2> /dev/null | tr -d ' ')
    [ -n "$p" ] || break
    protect="$protect$p "
  done
  for pid in $(cat "$cgroup_dir/cgroup.procs" 2> /dev/null); do
    case $protect in *" $pid "*) continue ;; esac
    leftovers=$((leftovers + 1))
    kill -TERM "$pid" 2> /dev/null || true
  done
  if ((leftovers > 0)); then
    sleep 1
    for pid in $(cat "$cgroup_dir/cgroup.procs" 2> /dev/null); do
      case $protect in *" $pid "*) continue ;; esac
      kill -KILL "$pid" 2> /dev/null || true
    done
    log "cleanup stopped $leftovers process(es) left in the lab scope"
  fi
}

# cleanup: nothing the lab started outlives it. The stand-in server, its tmux server and the live runs are stopped by
# name; every process left in the lab scope's cgroup is a leak, and each is stopped here.
cleanup() {
  local rc=$? pid unit
  trap - EXIT INT TERM
  stop_readers
  standin_stop
  for pid in $(listener_pids); do
    unit=$(sed -n '2s|.*/||p' "/proc/$pid/cgroup" 2> /dev/null || true)
    [ -n "$unit" ] && systemctl --user stop "$unit" > /dev/null 2>&1
    kill -KILL "$pid" 2> /dev/null || true
  done
  kill_scope_leftovers
  printf '{"kind":"lab_end","exit":%s,"port_listener":%s}\n' "$rc" "$(listener_pids | wc -l)"
  exit "$rc"
}

# ---- main ------------------------------------------------------------------------------------------------------------

require_env
FROM=${FROM,,}
TO=${TO,,}
FLOW_REF=${FLOW_REF:-$TO}
FLOW_REF=${FLOW_REF,,}
lab_paths
enter_scope
case $ACTION in
  teardown)
    teardown
    exit 0
    ;;
  prepare)
    prepare
    exit 0
    ;;
esac

[[ $MODE == flow || $MODE == starts ]] || fail "--mode is flow or starts: $MODE"
[[ $RUNS =~ ^[0-9]+$ ]] && ((RUNS > 0)) || fail "--runs is a positive number: $RUNS"
require_tools
[ -f "$KEY_FILE" ] || fail "the model key file exists: $KEY_FILE"
[ -d "$SEED" ] || fail "the lab seed home exists; run --prepare: $SEED"
[ -d "$LAB/bin/$TO/release" ] || fail "the build of $TO is installed (run dev/release-lab/build.sh): $LAB/bin/$TO"
if [ "$MODE" = flow ]; then
  [ -d "$LAB/bin/$FROM/release" ] || fail "the build of $FROM is installed: $LAB/bin/$FROM"
fi
[ -z "$(listener_pids)" ] || fail "the lab port $PORT is free before the run"
mkdir -p "$RUNS_DIR"
BAD_RUNS=0
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [ "$MODE" = starts ]; then
  reset_flow_checkout
  check_commits
  startup_lab_layout
  run_starts
  ((BAD_RUNS == 0)) || exit 1
  exit 0
fi

MAX_ATTEMPTS=${MAX_ATTEMPTS:-$((RUNS + 2))}
completed=0
attempt=0
STOPPED=0
LAST_VERDICT=
while ((attempt < MAX_ATTEMPTS && completed < RUNS)); do
  attempt=$((attempt + 1))
  if run_one_flow "$((completed + 1))"; then
    completed=$((completed + 1))
    [ "$LAST_VERDICT" = live ] || BAD_RUNS=$((BAD_RUNS + 1))
  else
    STOPPED=$((STOPPED + 1))
  fi
done
log "$completed of $RUNS run(s) completed, $STOPPED attempt(s) stopped before the switch, after $attempt attempt(s)"
[ "$completed" -ge "$RUNS" ] && ((BAD_RUNS == 0)) || exit 1
exit 0

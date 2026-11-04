#!/usr/bin/env bash
# The startup lab: timed server starts on a task-tree mock home through trial.sh --serve, and the recovery check that
# compares what two builds' starts write and log.
#
#   startup-lab.sh install <build> <commit> <release dir>
#       copy charliebot-server and charliebot from a release build of <commit> into bin-<build>/, and the commit into
#       bin-<build>/COMMIT
#   startup-lab.sh install-tools <release dir>
#       copy mock-home and startup-lab-launcher from a release build into tools/; every later subcommand runs these
#   startup-lab.sh generate
#       write the lab home, home/ (8,100 task-tree sessions, seed 1)
#   startup-lab.sh reset
#       return the lab home to its generated state: boot marker, file times and worktrees
#   startup-lab.sh time <build> <hot|cold> [label]
#       reset, then one start of bin-<build>/charliebot-server on a copy of the lab home, with a warmed or an evicted
#       page cache; prints the summary
#   startup-lab.sh summary <server log>
#       the start's segments, walk counts and phase line, with checks that the start is valid
#   startup-lab.sh fidelity <server log>
#       whether a base start reproduces the production start of the reference below
#   startup-lab.sh verify-seed
#       reset the lab home, copy it to check-home/ and add the recovery check's sessions
#   startup-lab.sh verify-run <build> <label>
#       one start of bin-<build> on check-home/, with live runs; digests what the server wrote and logged
#   startup-lab.sh verify-compare <run dir> <run dir>
#       diff two verify runs' recovery summaries, file digests and log items
#
# STARTUP_LAB_DIR names the lab directory, an absolute path. It holds:
#   bin-<build>/      a server, its CLI and COMMIT, the full sha they were built from
#   tools/            mock-home and startup-lab-launcher, from one build
#   live-home/        the config.yaml and credentials.yaml that trial.sh reads as the live home
#   home/, check-home/, runs/<UTC>-<build>-<cache>[-label]/, trials/, start/<build>/
# trial.sh runs from this script's checkout, which holds every timed commit and origin/main. The host needs
# systemd-run with a user manager and charlie-code on PATH, as trial.sh does.
#
# The start: trial.sh --serve --seed-home copies the lab home into a fresh trial home inside a systemd scope with
# MemoryMax 32G, and starts start/<build>/charliebot-server, the launcher. The launcher syncs, warms or evicts the trial
# home's pages, and execs the server (rust/crates/app/mock-home/src/bin/startup_lab_launcher.rs). The serve command
# saves the ready server's /proc counters to the run's proc.txt.
#
# The fidelity reference is the production start of 11-01 12:04 PT, full=593, state_open=296, file_after_marker=100,
# worktree_exists=144, sequence_ref=34, run_unended=19, and the one of 11-03 09:38 PT, node_pass_file_reads=247 and
# 909,949,484 bytes of logged whole-log reads (chat_events_file_read) before the phase line. The warm start of release
# 5a41936047 (2026-11-04 00:05:45Z) makes 7 whole-log reads of 215,163,491 bytes in recover_pending_reports and 3 of
# 76,614,206 bytes in run_facts, and its fact scans take longer than its skip checks. A base start passes when each
# count or byte total lies within half to twice its reference, and the fact scans take longer than the skip checks.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
readonly SCRIPT_DIR
readonly SELF="$SCRIPT_DIR/startup-lab.sh"
readonly TRIAL="$SCRIPT_DIR/trial.sh"
readonly SESSIONS=8100
readonly SEED=1
# The scope caps the trial's memory, page cache included. trial.sh copies the 6 GiB seed inside it, so the cap must
# hold the copy and the server or the cgroup reclaims the copy and a hot start reads from disk.
readonly SCOPE_MEMORY_MAX=32G
# A hot start whose server read more than this from disk before server_ready did not find its files cached.
readonly HOT_READ_BYTES_MAX=200000000
# The verify serve command waits this long for the open tasks' live runs to end, and then for the log to stay
# unchanged this many seconds.
readonly RUN_END_WAIT_SECONDS=180
readonly SETTLE_SECONDS=8

fail() {
  printf 'startup-lab.sh: %s\n' "$1" >&2
  exit 2
}

LAB=${STARTUP_LAB_DIR:-}
[[ $LAB == /* && -d $LAB ]] || fail "STARTUP_LAB_DIR names the lab directory as an absolute path: '${LAB}'"
LAB=$(cd "$LAB" && pwd -P)
readonly LAB
readonly HOME_DIR="$LAB/home"
readonly CHECK_SEED="$LAB/check-home"
readonly LIVE_RUNS="$LAB/check-home.live-runs"
readonly TOOLS="$LAB/tools"
readonly MOCK_HOME="$TOOLS/mock-home"

require_tools() {
  [[ -x $MOCK_HOME && -x $TOOLS/startup-lab-launcher ]] || fail "the lab tools are missing; run install-tools: $TOOLS"
}

build_commit() {
  local build=$1
  [[ $build =~ ^[A-Za-z0-9._-]+$ ]] || fail "a build name is letters, digits, dots, dashes and underscores: $build"
  [[ -x $LAB/bin-$build/charliebot-server && -x $LAB/bin-$build/charliebot && -f $LAB/bin-$build/COMMIT ]] \
    || fail "the build is missing; run install: $LAB/bin-$build"
  cat "$LAB/bin-$build/COMMIT"
}

cmd_install() {
  local build=$1 commit=$2 release=$3
  [[ $build =~ ^[A-Za-z0-9._-]+$ ]] || fail "a build name is letters, digits, dots, dashes and underscores: $build"
  [[ $commit =~ ^[0-9a-f]{40}$ ]] || fail "the commit is a full lowercase sha: $commit"
  git -C "$SCRIPT_DIR" cat-file -e "$commit^{commit}" || fail "this checkout lacks $commit, which trial.sh extracts"
  mkdir -p "$LAB/bin-$build"
  cp "$release/charliebot-server" "$release/charliebot" "$LAB/bin-$build/"
  printf '%s\n' "$commit" >"$LAB/bin-$build/COMMIT"
  printf 'installed %s at %s\n' "$LAB/bin-$build" "$commit"
}

cmd_install_tools() {
  local release=$1
  mkdir -p "$TOOLS"
  cp "$release/mock-home" "$release/startup-lab-launcher" "$TOOLS/"
  printf 'installed %s\n' "$TOOLS"
}

cmd_generate() {
  require_tools
  [[ ! -e $HOME_DIR ]] || fail "the lab home exists; remove $HOME_DIR to generate it again"
  "$MOCK_HOME" generate "$HOME_DIR" --shape task-tree --sessions "$SESSIONS" --seed "$SEED"
}

cmd_reset() {
  require_tools
  "$MOCK_HOME" reset "$HOME_DIR"
}

# The value of key=<digits> on the first log line holding event, or nothing when the line or the key is absent.
log_field() {
  local log=$1 event=$2 key=$3 line
  line=$(grep -m1 -E "[[:space:]]$event[[:space:]]" "$log") || return 0
  if [[ $line =~ (^|[[:space:]])$key=([0-9]+) ]]; then
    printf '%s' "${BASH_REMATCH[2]}"
  fi
}

# The bytes the server read from disk before server_ready. Its read counter includes the launcher's page-cache pass,
# which ran in the same process before the exec, so the launcher's own count is subtracted.
server_disk_reads() {
  local log=$1 total launcher='' line
  total=$(log_field "$log" server_ready read_bytes)
  line=$(grep -m1 '^startup-lab:' "$log") || fail "$log has no launcher line"
  [[ $line =~ wrapper_read_bytes=([0-9]+) ]] && launcher=${BASH_REMATCH[1]}
  [[ -n $total && -n $launcher ]] || fail "$log lacks server_ready read_bytes or the launcher's read count"
  printf '%s' "$(( total - launcher ))"
}

check() {
  local name=$1 value=$2 low=$3 high=$4
  if (( value >= low && value <= high )); then
    printf 'PASS %s=%s in [%s, %s]\n' "$name" "$value" "$low" "$high"
  else
    printf 'FAIL %s=%s outside [%s, %s]\n' "$name" "$value" "$low" "$high"
    return 1
  fi
}

# Prints one start's numbers, and returns 1 when the start is not valid: the segments do not sum to ready_in_ms, the
# server could not bind, or it launched an agent.
cmd_summary() {
  local log=$1 failed=0
  [[ -f $log ]] || fail "no server log at $log"
  local ready walk done_line
  ready=$(log_field "$log" server_ready ready_in_ms)
  walk=$(log_field "$log" task_tree_recovery_done elapsed_ms)
  [[ -n $ready && -n $walk ]] || fail "$log lacks server_ready or task_tree_recovery_done"
  done_line=$(grep -m1 -E '[[:space:]]task_tree_recovery_done[[:space:]]' "$log")
  printf 'log %s\n' "$log"
  printf '%s\n' "${done_line#*task_tree_recovery_done}" | sed 's/^ */walk /'
  printf 'ready_in_ms=%s walk_ms=%s disk_read_bytes=%s\n' "$ready" "$walk" "$(server_disk_reads "$log")"
  local segments segment_sum
  segments=$(grep -E '[[:space:]]startup_segment[[:space:]]' "$log") || fail "$log has no startup_segment line"
  printf 'segments:\n'
  sed -E 's/.*segment=([^ ]+) elapsed_ms=([0-9]+) at_ms=([0-9]+).*/  \1 \2 \3/' <<<"$segments"
  segment_sum=$(sed -E 's/.*elapsed_ms=([0-9]+).*/\1/' <<<"$segments" | awk '{ sum += $1 } END { print sum }')
  if (( segment_sum == ready )); then
    printf 'PASS segment sum %s = ready_in_ms\n' "$segment_sum"
  else
    printf 'FAIL segment sum %s != ready_in_ms %s\n' "$segment_sum" "$ready"
    failed=1
  fi
  grep -m1 -E '[[:space:]]task_tree_recovery_phases[[:space:]]' "$log" | sed 's/.*task_tree_recovery_phases */phases /'
  printf 'triggers_recovered=%s node_failures=%s\n' \
    "$(grep -cE '[[:space:]]trigger_recovered[[:space:]]' "$log" || true)" \
    "$(grep -cE '[[:space:]]task_recovery_node_failed[[:space:]]' "$log" || true)"
  if grep -qE '[[:space:]]bind_failed[[:space:]]' "$log"; then
    printf 'FAIL void: the server could not bind its port; another server answered the readiness poll\n'
    failed=1
  fi
  local launches
  launches=$(grep -cE '[[:space:]](master_cc_starting|master_cc_spawned|worker_spawned|task_run_launch_failed|task_run_launch_refused)[[:space:]]' "$log" || true)
  if (( launches == 0 )); then
    printf 'PASS no agent launch\n'
  else
    printf 'FAIL %s agent launch lines\n' "$launches"
    failed=1
  fi
  return "$failed"
}

# The value of one step's entry in a phase field, such as node_pass_step_file_reads='recover_pending_reports=7 ...', or
# nothing when the phase line or the step is absent.
step_value() {
  local log=$1 field=$2 step=$3 line pattern='' pair
  local -a list
  line=$(grep -m1 -E '[[:space:]]task_tree_recovery_phases[[:space:]]' "$log") || return 0
  pattern="$field='([^']*)'"
  [[ $line =~ $pattern ]] || return 0
  read -r -a list <<<"${BASH_REMATCH[1]}"
  for pair in "${list[@]}"; do
    [[ $pair == "$step="* ]] && printf '%s' "${pair#*=}"
  done
  return 0
}

cmd_fidelity() {
  local log=$1 failed=0
  [[ -f $log ]] || fail "no server log at $log"
  local done_line phases_at full reads logged
  done_line=$(grep -m1 -E '[[:space:]]task_tree_recovery_done[[:space:]]' "$log") || fail "$log has no walk"
  full=$(log_field "$log" task_tree_recovery_done full)
  reads=$(log_field "$log" task_tree_recovery_phases node_pass_file_reads)
  [[ -n $full && -n $reads ]] || fail "$log lacks the walk's full count or node_pass_file_reads"
  phases_at=$(grep -m1 -nE '[[:space:]]task_tree_recovery_phases[[:space:]]' "$log" | cut -d: -f1)
  logged=$(head -n "$phases_at" "$log" | awk '/[[:space:]]chat_events_file_read[[:space:]]/ {
    if (match($0, /[[:space:]]bytes=[0-9]+/)) sum += substr($0, RSTART + 7, RLENGTH - 7) } END { print sum + 0 }')
  check full "$full" 297 1186 || failed=1
  local reason reference value
  for reason in state_open:296 file_after_marker:100 worktree_exists:144 sequence_ref:34 run_unended:19; do
    reference=${reason#*:}
    reason=${reason%%:*}
    value=$(grep -oE "\"$reason\": [0-9]+" <<<"$done_line" | grep -oE '[0-9]+$' || printf '0')
    check "$reason" "$value" "$(( (reference + 1) / 2 ))" "$(( reference * 2 ))" || failed=1
  done
  check node_pass_file_reads "$reads" 124 494 || failed=1
  check logged_whole_log_read_bytes "$logged" 454974742 1819898968 || failed=1
  # The warm production start's recover_pending_reports and run_facts: 7 reads of 215,163,491 bytes and 3 reads of
  # 76,614,206 bytes. The bounds are half to twice each reference.
  check recover_pending_reports_reads "$(step_value "$log" node_pass_step_file_reads recover_pending_reports)" 4 14 || failed=1
  check recover_pending_reports_bytes "$(step_value "$log" node_pass_step_file_read_bytes recover_pending_reports)" \
    107581745 430326982 || failed=1
  check run_facts_reads "$(step_value "$log" node_pass_step_file_reads run_facts)" 2 6 || failed=1
  check run_facts_bytes "$(step_value "$log" node_pass_step_file_read_bytes run_facts)" 38307103 153228412 || failed=1
  # The fact scans take longer than the skip checks, as in the warm production start (1,003 ms and 559 ms).
  local fact_scans skip_checks
  fact_scans=$(log_field "$log" task_tree_recovery_phases fact_scans_ms)
  skip_checks=$(log_field "$log" task_tree_recovery_phases skip_checks_ms)
  [[ -n $fact_scans && -n $skip_checks ]] || fail "$log lacks fact_scans_ms or skip_checks_ms"
  if (( fact_scans > skip_checks )); then
    printf 'PASS fact_scans_ms=%s > skip_checks_ms=%s\n' "$fact_scans" "$skip_checks"
  else
    printf 'FAIL fact_scans_ms=%s <= skip_checks_ms=%s\n' "$fact_scans" "$skip_checks"
    failed=1
  fi
  return "$failed"
}

# The ready server's counters: CPU time and run-queue wait summed over its threads (schedstat, ns), I/O and memory.
# The run-queue wait is the time its threads were runnable but waited for a CPU, so it measures host contention.
cmd_serve_snapshot() {
  local run=$1 commit=$2 pid=${CHARLIEBOT_SERVE_PID:?trial.sh exports CHARLIEBOT_SERVE_PID}
  # trial.sh's readiness poll accepts any server on its port, so wait for this server's own server_ready line.
  local log="$LAB/trials/trial-${commit:0:12}/server-1.log"
  until grep -qE '[[:space:]]server_ready[[:space:]]' "$log"; do
    kill -0 "$pid" 2>/dev/null || fail "the trial server $pid exited before server_ready"
    sleep 0.2
  done
  {
    printf 'loadavg %s\n' "$(cat /proc/loadavg)"
    awk '{ cpu += $1; wait += $2; slices += $3 } END { printf "threads_cpu_ns %d\nthreads_runqueue_wait_ns %d\nthreads_timeslices %d\n", cpu, wait, slices }' \
      /proc/"$pid"/task/*/schedstat
    sed 's/^/io_/; s/: / /' /proc/"$pid"/io
    grep -E '^(VmHWM|VmRSS|Threads|voluntary_ctxt_switches|nonvoluntary_ctxt_switches):' /proc/"$pid"/status | tr -d ':'
  } >"$run/proc.txt"
}

# Prepares one trial start of <build> and prints its run directory: the launcher links, the reset, the seed read into
# the page cache outside the scope, so the copy's reads hit the cache and only the copy is charged to the scope.
prepare_start() {
  local build=$1 seed=$2 run=$3
  require_tools
  [[ -f $LAB/live-home/config.yaml ]] || fail "the lab live home has no config.yaml: $LAB/live-home"
  command -v charlie-code >/dev/null || fail 'charlie-code resolves on PATH, as trial.sh requires'
  command -v systemd-run >/dev/null || fail 'systemd-run starts the memory-capped scope'
  local free_kb
  free_kb=$(df -Pk "$LAB" | awk 'NR == 2 { print $4 }')
  (( free_kb > 20 * 1024 * 1024 )) || fail "less than 20 GiB free under $LAB for the trial copy"
  mkdir -p "$LAB/start/$build" "$LAB/trials" "$run"
  ln -sfn "$TOOLS/startup-lab-launcher" "$LAB/start/$build/charliebot-server"
  ln -sfn "$LAB/bin-$build/charliebot" "$LAB/start/$build/charliebot"
  cmd_reset
  find "$seed" -type f -exec cat {} + >/dev/null
}

# Runs trial.sh --serve on <seed> with the launcher of <build> and the serve command after the --.
serve() {
  local build=$1 cache=$2 seed=$3 run=$4 commit=$5 unit=$6
  shift 6
  export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
  STARTUP_LAB_SERVER="$LAB/bin-$build/charliebot-server" STARTUP_LAB_CACHE="$cache" \
    CHARLIEBOT_HOME="$LAB/live-home" CHARLIEBOT_RELEASE_RUN_DIR="$run" \
    systemd-run --user --scope --quiet -p MemoryMax="$SCOPE_MEMORY_MAX" --unit="$unit" \
    "$TRIAL" --serve --tree-root "$LAB/trials" --seed-home "$seed" --in-caller-scope \
    "$commit" "$LAB/start/$build/charliebot-server" -- "$@" >"$run/trial.out" 2>&1
}

cmd_time() {
  local build=$1 cache=$2 label=${3:-} commit
  commit=$(build_commit "$build")
  [[ $cache == hot || $cache == cold ]] || fail "the cache is hot or cold: $cache"
  local stamp run
  stamp=$(date -u +%Y%m%dT%H%M%SZ)
  run="$LAB/runs/$stamp-$build-$cache${label:+-$label}"
  prepare_start "$build" "$HOME_DIR" "$run"
  printf 'start %s: %s at %s, %s cache, load %s, evidence %s\n' "$stamp" "$build" "$commit" "$cache" \
    "$(cut -d' ' -f1 /proc/loadavg)" "$run"
  local status=0
  serve "$build" "$cache" "$HOME_DIR" "$run" "$commit" "startup-lab-$stamp" "$SELF" serve-snapshot "$run" "$commit" \
    || status=$?
  tail -n 5 "$run/trial.out"
  (( status == 0 )) || fail "trial.sh exited $status; see $run/trial.out"
  local log verdict=0 read_bytes
  log=$(find "$run" -name server-1.log | head -1)
  cmd_summary "$log" | tee "$run/summary.txt" || verdict=1
  read_bytes=$(server_disk_reads "$log")
  if [[ $cache == hot ]] && (( read_bytes > HOT_READ_BYTES_MAX )); then
    printf 'FAIL not hot: the server read %s bytes from disk before server_ready\n' "$read_bytes" | tee -a "$run/summary.txt"
    verdict=1
  fi
  tr '\n' ' ' <"$run/proc.txt" | tee -a "$run/summary.txt"
  printf '\n' | tee -a "$run/summary.txt"
  return "$verdict"
}

cmd_verify_seed() {
  require_tools
  [[ ! -e $CHECK_SEED ]] || fail "the check seed exists; remove $CHECK_SEED to build it again"
  cmd_reset
  mkdir "$CHECK_SEED"
  cp -a "$HOME_DIR/." "$CHECK_SEED/"
  rm -rf "$CHECK_SEED/.worktrees"
  "$MOCK_HOME" recovery-seed "$CHECK_SEED" "$LIVE_RUNS"
}

# Starts one process per live run and writes its identity into the check seed's run record, so the walk finds the run
# alive. setsid gives each sleep a session of its own, so a signal to the run's process group reaches only the sleep.
start_live_runs() {
  local run=$1 session_id run_id label pid
  : >"$run/live-pids.txt"
  while read -r session_id run_id label; do
    setsid sleep 1800 </dev/null >/dev/null 2>&1 &
    pid=$!
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      [[ $(cat "/proc/$pid/comm" 2>/dev/null) == sleep ]] && break
      sleep 0.05
    done
    [[ $(cat "/proc/$pid/comm" 2>/dev/null) == sleep ]] || fail "the live process for $label is not the started sleep"
    "$MOCK_HOME" recovery-attach "$CHECK_SEED" "$session_id" "$run_id" "$pid"
    printf '%s %s %s %s\n' "$pid" "$session_id" "$run_id" "$label" >>"$run/live-pids.txt"
  done <"$LIVE_RUNS"
}

stop_live_runs() {
  local run=$1 pid rest
  while read -r pid rest; do
    if [[ $(cat "/proc/$pid/comm" 2>/dev/null) == sleep ]]; then
      kill -TERM "$pid"
      printf 'stopped live run process %s (%s)\n' "$pid" "$rest"
    fi
  done <"$run/live-pids.txt"
}

cmd_verify_run() {
  local build=$1 label=$2 commit
  commit=$(build_commit "$build")
  [[ -d $CHECK_SEED && -s $LIVE_RUNS ]] || fail 'the check seed is missing; run verify-seed'
  local stamp run
  stamp=$(date -u +%Y%m%dT%H%M%SZ)
  run="$LAB/runs/$stamp-verify-$build-$label"
  # The reset restores the lab home's worktrees, which the check seed's run records name.
  prepare_start "$build" "$CHECK_SEED" "$run"
  start_live_runs "$run"
  sleep 1
  touch "$run/stamp"
  printf 'verify %s: %s at %s, evidence %s\n' "$stamp" "$build" "$commit" "$run"
  local status=0
  serve "$build" hot "$CHECK_SEED" "$run" "$commit" "startup-verify-$stamp" "$SELF" serve-digest "$run" "$commit" \
    || status=$?
  stop_live_runs "$run"
  tail -n 8 "$run/trial.out"
  (( status == 0 )) || fail "trial.sh exited $status; see $run/trial.out"
  local log
  log=$(find "$run" -name server-1.log | head -1)
  "$MOCK_HOME" recovery-items "$log" "$CHECK_SEED" --lab "$LAB" >"$run/items.txt"
  # A verify start resumes dead runs, so its summary may fail the no-launch check that a timed start must pass.
  cmd_summary "$log" >"$run/summary.txt" || printf 'the summary flags this start; see %s\n' "$run/summary.txt"
  grep -E '^walk|^triggers|agent launch' "$run/summary.txt"
  printf 'digest %s lines, items %s lines\n' "$(wc -l <"$run/files.txt")" "$(wc -l <"$run/items.txt")"
}

# Waits until the file stops growing for SETTLE_SECONDS.
wait_quiet() {
  local file=$1 size last=-1 quiet=0
  for _ in $(seq 1 120); do
    size=$(stat -c %s "$file")
    if (( size == last )); then
      quiet=$(( quiet + 1 ))
      (( quiet >= SETTLE_SECONDS )) && return 0
    else
      quiet=0
    fi
    last=$size
    sleep 1
  done
  fail "$file kept growing for 120 s"
}

# The verify serve command: after server_ready it ends the live runs, waits until the open tasks' runs have ended and
# the log is quiet, and digests what the server wrote.
cmd_serve_digest() {
  local run=$1 commit=$2
  local tree="$LAB/trials/trial-${commit:0:12}"
  local log="$tree/server-1.log" home="$tree/home/.charliebot"
  [[ -f $log && -d $home/sessions ]] || fail "no trial log or home under $tree"
  until grep -qE '[[:space:]]server_ready[[:space:]]' "$log"; do
    kill -0 "$CHARLIEBOT_SERVE_PID" 2>/dev/null || fail 'the trial server exited before server_ready'
    sleep 0.2
  done
  grep -m1 -E '[[:space:]]task_tree_recovery_done[[:space:]]' "$log" >"$run/recovery-done.txt"
  sleep 3
  stop_live_runs "$run"
  local waited=0 open
  while :; do
    open=$("$MOCK_HOME" recovery-unended "$home/sessions" "$LIVE_RUNS")
    (( open == 0 )) && break
    (( waited < RUN_END_WAIT_SECONDS )) || fail "$open live runs did not end in $RUN_END_WAIT_SECONDS s"
    sleep 1
    waited=$(( waited + 1 ))
  done
  printf 'live runs ended after %s s\n' "$waited"
  wait_quiet "$log"
  "$MOCK_HOME" recovery-files "$CHECK_SEED" "$home" "$run/stamp" --lab "$LAB" >"$run/files.txt"
}

cmd_verify_compare() {
  local a=$1 b=$2 failed=0 run
  printf '== recovery done\n'
  for run in "$a" "$b"; do
    sed -E 's/.*task_tree_recovery_done *//; s/elapsed_ms=[0-9]+ //' "$run/recovery-done.txt"
  done
  printf '== files (%s vs %s lines)\n' "$(wc -l <"$a/files.txt")" "$(wc -l <"$b/files.txt")"
  diff "$a/files.txt" "$b/files.txt" || failed=1
  printf '== items (%s vs %s lines)\n' "$(wc -l <"$a/items.txt")" "$(wc -l <"$b/items.txt")"
  diff "$a/items.txt" "$b/items.txt" || failed=1
  return "$failed"
}

usage='install <build> <commit> <release dir> | install-tools <release dir> | generate | reset
  | time <build> <hot|cold> [label] | summary <server log> | fidelity <server log>
  | verify-seed | verify-run <build> <label> | verify-compare <run dir> <run dir>'

case ${1:-}:$# in
  install:4) cmd_install "$2" "$3" "$4" ;;
  install-tools:2) cmd_install_tools "$2" ;;
  generate:1) cmd_generate ;;
  reset:1) cmd_reset ;;
  time:3 | time:4) cmd_time "$2" "$3" "${4:-}" ;;
  summary:2) cmd_summary "$2" ;;
  fidelity:2) cmd_fidelity "$2" ;;
  verify-seed:1) cmd_verify_seed ;;
  verify-run:3) cmd_verify_run "$2" "$3" ;;
  verify-compare:3) cmd_verify_compare "$2" "$3" ;;
  serve-snapshot:3) cmd_serve_snapshot "$2" "$3" ;;
  serve-digest:3) cmd_serve_digest "$2" "$3" ;;
  *) fail "usage: startup-lab.sh $usage" ;;
esac

#!/usr/bin/env bash
# The poor-network bench's entry command. It runs on the build host inside a synced tree, through
# dev/remote-run/run.sh, and drives the six scenarios of README.md against a trial server on the mock home.
#
#   dev/remote-run/run.sh dev/poor-network/run.sh --side both --out-dir <dir> --before-clone <clone>
#   dev/remote-run/run.sh dev/poor-network/run.sh --side after --out-dir <dir>
#
# --side before  measures origin/main (--before-rev, default origin/main, in --before-clone, a git clone that holds it);
# --side after   measures this tree;
# --side both    runs before, then after, back to back in one invocation. If the before side's calibration does not
#                reproduce the switch-during-refresh hold, it stops after before, prints its numbers, and exits 3.
# --detach       starts the run as a transient systemd user unit and returns at once; the log is <out-dir>/pn-<id>.log.
# --lock FILE    holds an advisory lock on FILE around each side's trial (not the builds), waiting up to 2 h for it, so
#                bench runs that take the same lock never measure side by side. One hold covers one side attempt; a
#                side also ends once its JSON is 120 s old even if its bench hangs, and a retry re-queues.
#
# Each side runs as the command of `scripts/release/trial.sh --serve`, so the trial starts, checks and stops the server
# the bench measures, and proves that no process of the trial remains. The bench's own children (Chrome, the proxy, the
# TLS front) end with the bench. The server's charlie-code backend is the scripted stub in dev/poor-network/stub.
#
# Exit: 0 both sides measured; 1 a measurement or build failed; 2 a precondition failed, nothing started; 3 the baseline
# calibration did not reproduce, so the after side did not run; 75 a side's wait for the lock ran out.
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly BENCH_DIR="$SCRIPT_DIR"
readonly LOCK_WAIT_SECONDS=7200
# flock's exit code when the wait runs out: one no trial or bench exit code uses.
readonly LOCK_TIMEOUT_EXIT=75
# How long a side's written JSON may sit beside a bench that does not exit, before the runner ends the trial tree.
readonly SIDE_EXIT_GRACE_SECONDS=120

side=both
out_dir=
before_clone=
before_rev=origin/main
repetitions=5
scenarios=background,held,reset,stream
assets_dir=
detach=0
lock_file=

usage() {
  sed -n '2,21p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

die() {
  local code=$1
  shift
  printf 'poor-network: %s\n' "$*" >&2
  exit "$code"
}

while (($#)); do
  case $1 in
    --side) side=${2:?--side needs a value}; shift 2 ;;
    --out-dir) out_dir=${2:?--out-dir needs a value}; shift 2 ;;
    --before-clone) before_clone=${2:?--before-clone needs a value}; shift 2 ;;
    --before-rev) before_rev=${2:?--before-rev needs a value}; shift 2 ;;
    --repetitions) repetitions=${2:?--repetitions needs a value}; shift 2 ;;
    --scenarios) scenarios=${2:?--scenarios needs a value}; shift 2 ;;
    --assets-dir) assets_dir=${2:?--assets-dir needs a value}; shift 2 ;;
    --detach) detach=1; shift ;;
    --lock) lock_file=${2:?--lock needs a value}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die 2 "unknown argument: $1" ;;
  esac
done

case $side in
  before|after|both) ;;
  *) die 2 "--side must be before, after or both, got $side" ;;
esac
[[ -n $out_dir ]] || die 2 "--out-dir is required: the run's files go there, outside the tree"
[[ $repetitions =~ ^[1-9][0-9]*$ ]] || die 2 "--repetitions must be a positive integer, got $repetitions"
if [[ $side != after ]]; then
  [[ -n $before_clone ]] || die 2 "--before-clone is required for --side $side"
fi

for tool in node cargo git openssl systemd-run flock; do
  command -v "$tool" >/dev/null || die 2 "missing tool on this host: $tool"
done

tree=$(git -C "$BENCH_DIR" rev-parse --show-toplevel) || die 2 "$BENCH_DIR is not in a git tree"
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)
run_id=$(date -u +%Y%m%dT%H%M%SZ)

if ((detach)); then
  log="$out_dir/pn-$run_id.log"
  forwarded=(--side "$side" --out-dir "$out_dir" --before-rev "$before_rev" --repetitions "$repetitions" --scenarios "$scenarios")
  [[ -n $before_clone ]] && forwarded+=(--before-clone "$before_clone")
  [[ -n $assets_dir ]] && forwarded+=(--assets-dir "$assets_dir")
  [[ -n $lock_file ]] && forwarded+=(--lock "$lock_file")
  systemd-run --user --unit="pn-$run_id" --collect --working-directory="$tree" \
    --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
    ${POOR_NETWORK_CHROME:+--setenv=POOR_NETWORK_CHROME="$POOR_NETWORK_CHROME"} \
    bash -c "exec bash '$BENCH_DIR/run.sh' ${forwarded[*]@Q} > '$log' 2>&1" >/dev/null
  printf 'started transient unit pn-%s; log: %s\n' "$run_id" "$log"
  exit 0
fi

# The Chrome the bench drives. The trial replaces HOME for its own tree, so the discovery happens here.
chrome=${POOR_NETWORK_CHROME:-}
if [[ -z $chrome ]]; then
  chrome=$(ls -1 "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux*/chrome 2>/dev/null | sort -V | tail -1 || true)
fi
[[ -n $chrome && -x $chrome ]] || die 2 "no Chrome binary: set POOR_NETWORK_CHROME to one"
export POOR_NETWORK_CHROME=$chrome

run_dir="$out_dir/$run_id"
mkdir -p "$run_dir"
readonly RUN_DIR=$run_dir
export POOR_NETWORK_ASSETS=${assets_dir:-$out_dir/assets}
mkdir -p "$POOR_NETWORK_ASSETS"

# The after side is this tree. Its commit is the synced checkout's, which the remote runner makes a git commit.
after_commit=$(git -C "$tree" rev-parse HEAD)
after_label="tree $(git -C "$tree" rev-parse 'HEAD^{tree}')"

# The stub backend goes first on the server's PATH, so the charlie-code backend runs the scripted reply.
export PATH="$BENCH_DIR/stub:$PATH"

echo "poor-network: run $run_id side=$side chrome=$chrome"
echo "poor-network: after tree $tree at $after_label"

(cd "$tree/rust" && cargo build --release --locked -p charliebot-server -p charliebot-cli -p charliebot-mock-home)
after_bin="$tree/rust/target/release/charliebot-server"
mock_bin="$tree/rust/target/release/mock-home"
[[ -x $after_bin && -x $mock_bin ]] || die 1 "the after build did not produce charliebot-server and mock-home"

before_commit=
before_bin=
if [[ $side != after ]]; then
  before_commit=$(git -C "$before_clone" rev-parse --verify --quiet "$before_rev^{commit}") \
    || die 2 "$before_rev does not name a commit in $before_clone; fetch it there first"
  before_src="$run_dir/before-src"
  mkdir -p "$before_src"
  git -C "$before_clone" archive "$before_commit" | tar -x -C "$before_src"
  echo "poor-network: before commit $before_commit from $before_clone"
  (cd "$before_src/rust" && CARGO_TARGET_DIR="$out_dir/target-before" \
    cargo build --release --locked -p charliebot-server -p charliebot-cli)
  before_bin="$out_dir/target-before/release/charliebot-server"
  [[ -x $before_bin ]] || die 1 "the before build did not produce charliebot-server"
fi

# The mock home holds the view's rows that the bench's own sessions do not: VIEW.rows less BENCH_SESSIONS, each one a
# session of the mock's active corpus, with a task spec sized to the live view's rows (the view shape).
mock_version=$("$mock_bin" version)
mock_sessions=$(node -e 'import(process.argv[1]).then((profile) => process.stdout.write(String(profile.VIEW.rows - profile.BENCH_SESSIONS)))' "file://$BENCH_DIR/profile.mjs")
mock_home="$HOME/improve/mock-home/$mock_version-view-$mock_sessions"
if [[ ! -d $mock_home ]]; then
  echo "poor-network: generating the view home of $mock_sessions sessions at $mock_home"
  "$mock_bin" generate "$mock_home" --sessions "$mock_sessions" --shape view
fi

node "$BENCH_DIR/reply.mjs" "$tree" "$run_dir/replies"

# A bench-owned home for the trial: its config names a stand-in endpoint the stub never calls, and holds no credential,
# so the trial copies no live key and reaches no live model. The trial reads only this option's api_base; it writes its
# own option, charlie-code-release-trial, which the bench's sessions use.
live_home="$run_dir/live-home"
mkdir -p "$live_home"
cat >"$live_home/config.yaml" <<'YAML'
backends:
  options:
    - id: bench-endpoint
      label: Bench endpoint
      type: charlie-code
      model: bench/stub
      api_base: http://127.0.0.1:9/v1
YAML

# Runs one side as the command of a serve trial. $1 side, $2 trial script, $3 commit, $4 server binary.
run_side() {
  local name=$1 trial_script=$2 commit=$3 server_bin=$4 trial_root trial_status
  trial_root="/tmp/pn-$run_id-$name"
  mkdir -p "$trial_root"
  local label=$commit
  [[ $name == after ]] && label=$after_label
  export CHARLIEBOT_HOME=$live_home
  export CHARLIEBOT_RELEASE_RUN_DIR="$run_dir/evidence-$name"
  export POOR_NETWORK_REPLY_DIR="$run_dir/replies"
  # flock -o keeps the lock in flock's own process, so no process of the trial holds it past the trial's end.
  local lock=()
  if [[ -n $lock_file ]]; then
    lock=(flock -o -w "$LOCK_WAIT_SECONDS" -E "$LOCK_TIMEOUT_EXIT" "$lock_file")
    echo "poor-network: $name side waits for $lock_file"
  fi
  set +e
  "${lock[@]}" systemd-run --user --scope -p MemoryMax=16G -p MemorySwapMax=0 --unit="pn-$run_id-$name" \
    "$trial_script" --serve --in-caller-scope --tree-root "$trial_root" --seed-home "$mock_home" \
    "$commit" "$server_bin" -- \
    node "$BENCH_DIR/bench.mjs" --side "$name" --out "$run_dir/$name.json" \
      --mock-home "$mock_home" --mock-home-bin "$mock_bin" --reply-dir "$run_dir/replies" \
      --assets "$POOR_NETWORK_ASSETS" --commit "$label" \
      --repetitions "$repetitions" --scenarios "$scenarios" &
  trial_pid=$!
  # The side JSON is the bench's last artifact, and a bench that has written it can still sit alive and hold the trial,
  # and so the lock (this line's known defect). Once the JSON is SIDE_EXIT_GRACE_SECONDS old and the trial still runs,
  # the runner ends the trial tree itself: the lock is released at that point, and the summary is reprinted from the
  # JSON afterwards, so the kill loses nothing but the hang.
  local json_seen_at="" ended_tree=0
  while :; do
    kill -0 "$trial_pid" 2>/dev/null || break
    if [[ -f "$run_dir/$name.json" ]]; then
      [[ -n $json_seen_at ]] || json_seen_at=$(date +%s)
      if (( $(date +%s) - json_seen_at >= SIDE_EXIT_GRACE_SECONDS )) \
        && systemctl --user is-active --quiet "pn-$run_id-$name.scope"; then
        echo "poor-network: the $name side's JSON is ${SIDE_EXIT_GRACE_SECONDS}s old and its bench is still running; ending the trial tree"
        systemctl --user stop "pn-$run_id-$name.scope"
        ended_tree=1
      fi
    fi
    sleep 5
  done
  wait "$trial_pid"
  trial_status=$?
  # A tree the runner ended on purpose leaves the side's own verdict in its JSON; the kill is not a failure.
  if ((ended_tree)); then
    trial_status=0
  fi
  set -e
  # The timeout keeps flock's own exit code, so a caller that retries can tell it from a failed measurement and take
  # its place at the back of the queue again. One lock hold covers one side attempt; run.sh never retries inside it.
  ((trial_status != LOCK_TIMEOUT_EXIT)) || die "$LOCK_TIMEOUT_EXIT" "the $name side did not get $lock_file within $LOCK_WAIT_SECONDS s"
  echo "poor-network: $name side exit $trial_status"
  [[ -f "$run_dir/$name.json" ]] || die 1 "the $name side wrote no JSON"
  return "$trial_status"
}

if [[ $side == before || $side == both ]]; then
  (cd "$before_clone" && run_side before "$before_clone/scripts/release/trial.sh" "$before_commit" "$before_bin") \
    || die 1 "the before side failed; see $run_dir/before.json"
  calibrated=$(node -e 'const s = require(process.argv[1]); process.stdout.write(s.calibration ? String(s.calibration.reproduced) : "none")' "$run_dir/before.json")
  if [[ $side == both && $calibrated != true ]]; then
    node "$BENCH_DIR/report.mjs" "$run_dir/before.json"
    die 3 "the baseline did not reproduce the switch-during-refresh hold; the proxy does not model the link. The after side did not run."
  fi
fi

if [[ $side == after || $side == both ]]; then
  (cd "$tree" && run_side after "$tree/scripts/release/trial.sh" "$after_commit" "$after_bin") \
    || die 1 "the after side failed; see $run_dir/after.json"
fi

echo "poor-network: JSON in $run_dir"
if [[ $side == both ]]; then
  node "$BENCH_DIR/report.mjs" "$run_dir/before.json" "$run_dir/after.json"
else
  node "$BENCH_DIR/report.mjs" "$run_dir/$side.json"
fi

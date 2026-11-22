#!/usr/bin/env bash
# The poor-network bench's entry command. It runs on the build host inside a synced tree, through
# dev/remote-run/run.sh, and drives the scenarios of README.md against a trial server on the mock home.
#
#   dev/remote-run/run.sh dev/poor-network/run.sh --side both --out-dir <dir> --before-rev <rev> --before-clone <clone>
#   dev/remote-run/run.sh dev/poor-network/run.sh --side both --out-dir <dir> --before-clone <clone> \
#     --change-patch <file>
#   dev/remote-run/run.sh dev/poor-network/run.sh --side after --out-dir <dir>
#
# --side before  measures --before-rev (default origin/main, a commit of --before-clone, which holds it);
# --side after   measures this tree;
# --side both    measures the before side, then the after side, back to back in one invocation.
#                With --before-rev, the before side is that commit, measured now. Without it, the before side is the
#                stored reference commit R (before-store.mjs): each row R holds is reused when its key matches and the
#                A/A result marks it load-safe, and the rest are measured now. The after side is R with --change-patch
#                applied, a patch of the change (git diff <merge-base> HEAD --binary); an empty file means no change. A
#                patch that does not apply to R exits 2 and names --before-rev <merge-base>, which measures the change
#                on its own base.
#                The open scenario runs both sides at one CPU rate: the after side runs its open cells at the rate the
#                before side's reference cell derived, or at the stored rows' rate, so the sides' numbers compare.
#                If the before side measures the calibration reference (CALIBRATION_REFERENCE in profile.mjs) and does
#                not reproduce the switch-during-refresh hold, it stops after before, prints its numbers, and exits 3.
# --scenarios auto   the rows the change's files reach (scenario-map.mjs), with the default four always included. The
#                files come from --change-patch in the stored mode, or from --change-files FILE (one path per line)
#                with --before-rev.
# --detach       starts the run as a transient systemd user unit and returns at once; the log is <out-dir>/pn-<id>.log.
# --seed-only    sizes the sessions on each side the run names (bench --mode seed), writes each side's sizes to its
#                JSON and stops: no scenario, no store update and no report. The sizing check takes minutes, so a
#                sizing change is checked on both sides before a bench lock is taken for the measurement.
# --scenario-repetitions NAME=N,...  gives the named scenarios their own repetition count; the rest take --repetitions.
# --foryou-shots  after the after side's measurement, writes the For-you screenshots (switch, retry and offline reopen at
#                the three CSS sizes) into <run dir>/foryou-shots; needs the foryou scenario.
# --seed-home DIR  the view home that the run copies as its seed (default: the shared home
#                  ~/improve/mock-home/<version>-view-<sessions>). A run with a pinned home names it here, so a
#                  rebuild of the shared home cannot change the run's seed; each side still checks the copy's
#                  content hash, which the run records in seed.sha256.
#
# The bench holds the host's bench lock one step at a time (bench.mjs, step-gate.mjs): it takes the lock for a
# preparation stage, a probe or a measurement, gives it up after a step when another process is queued for it, and
# takes it again behind that process. --lock-wait SECONDS is how long each take waits (default 7200; an improve round
# passes 900). A side that cannot take the lock in time exits 75.
# --open-conditions LIST  the open scenario's cells: any of
#                         link,held,reset,flaps,offline,release,release-offline,replied-offline (default
#                         link,held,reset,flaps,offline,release). The reference cell always runs, since the CPU rate
#                         is derived from it. Stored-copy rows are measured only on the after side because main has no
#                         offline-copy client.
# --open-cpu-rate RATE  every side runs the open scenario's cells at RATE instead of a rate its reference derives, so
#                sides of separate invocations (two benches on one product) compare at one CPU speed. Each side's JSON
#                still records the rate its own reference cell derives.
#
# Each side runs as the command of `scripts/release/trial.sh --serve`, so the trial starts, checks and stops the server
# the bench measures, and proves that no process of the trial remains. The bench's own children (Chrome, the proxy, the
# TLS front) end with the bench. The server's charlie-code backend is the scripted stub in dev/poor-network/stub. The
# trial's tree is unique to this invocation and side, so two invocations on one commit never share a tree.
#
# Each side's trial tree goes under TMPDIR when the caller sets it, and under /tmp otherwise, so a caller inside a
# writable sandbox keeps the trees in its scratch.
#
# Exit: 0 both sides measured; 1 a measurement or build failed; 2 a precondition failed, nothing started, or the change
# does not apply to R; 3 the baseline calibration did not reproduce, so the after side did not run; 4 a scenario that
# passed on the before side fails on the after side; 5 a segment's CPU probe failed the rate gate twice; 75 a side's
# wait for the lock ran out.
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly BENCH_DIR="$SCRIPT_DIR"
source "$SCRIPT_DIR/lock.sh"
# How long a side's written JSON may sit beside a bench that does not exit, before the runner ends the trial tree.
readonly SIDE_EXIT_GRACE_SECONDS=120
# The trial's server ports. trial.sh's default range (18650-18669) is the improve perf trials' on host3, and two trials
# that pick the same free port race at startup (fy6: "port 18651 is held by another process"). 18600-18799 is claimed by
# the release lab, so the bench takes 18800-18819, which no repo script names.
readonly BENCH_PORT_MIN=18800
readonly BENCH_PORT_MAX=18819
readonly RATE_GATE_EXIT=5

side=both
out_dir=
before_clone=
before_rev=
change_patch=
change_files=
repetitions=5
scenarios=background,held,reset,stream
open_conditions=link,held,reset,flaps,offline,release
assets_dir=
detach=0
seed_only=0
foryou_shots=0
scenario_repetitions=
lock_wait=$BENCH_LOCK_WAIT_SECONDS
open_rate_args=()
seed_home=

usage() {
  # The header block runs from line 2 to the `set -euo pipefail` that ends it, so an added line never drops text.
  sed -n '2,/^set -euo pipefail$/{/^set -euo pipefail$/d;p}' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

die() {
  local code=$1
  shift
  printf 'poor-network: %s\n' "$*" >&2
  exit "$code"
}

# The content hash of a seed home: the sha256 of the sha256sum listing of its files, in path order.
seed_content_hash() {
  (cd "$1" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1)
}

while (($#)); do
  case $1 in
    --side) side=${2:?--side needs a value}; shift 2 ;;
    --out-dir) out_dir=${2:?--out-dir needs a value}; shift 2 ;;
    --before-clone) before_clone=${2:?--before-clone needs a value}; shift 2 ;;
    --before-rev) before_rev=${2:?--before-rev needs a value}; shift 2 ;;
    --change-patch) change_patch=${2:?--change-patch needs a value}; shift 2 ;;
    --change-files) change_files=${2:?--change-files needs a value}; shift 2 ;;
    --repetitions) repetitions=${2:?--repetitions needs a value}; shift 2 ;;
    --scenarios) scenarios=${2:?--scenarios needs a value}; shift 2 ;;
    --open-conditions) open_conditions=${2:?--open-conditions needs a value}; shift 2 ;;
    --open-cpu-rate) open_rate_args=(--open-cpu-rate "${2:?--open-cpu-rate needs a value}"); shift 2 ;;
    --assets-dir) assets_dir=${2:?--assets-dir needs a value}; shift 2 ;;
    --lock-wait) lock_wait=${2:?--lock-wait needs a value}; shift 2 ;;
    --seed-home) seed_home=${2:?--seed-home needs a value}; shift 2 ;;
    --detach) detach=1; shift ;;
    --seed-only) seed_only=1; shift ;;
    --scenario-repetitions) scenario_repetitions=${2:?--scenario-repetitions needs a value}; shift 2 ;;
    --foryou-shots) foryou_shots=1; shift ;;
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
[[ $lock_wait =~ ^[1-9][0-9]*$ ]] || die 2 "--lock-wait must be a positive number of seconds, got $lock_wait"
mode_args=()
if ((seed_only)); then
  mode_args=(--mode seed)
fi
if [[ $side != after ]]; then
  [[ -n $before_clone ]] || die 2 "--before-clone is required for --side $side"
fi
# The stored mode: both sides without --before-rev. Its before side is the stored reference R and its after side is R
# with the change's patch applied.
mode=explicit
if [[ $side == both && -z $before_rev ]]; then
  mode=stored
  [[ -n $change_patch ]] || die 2 "--side both without --before-rev measures the stored reference R and needs" \
    "--change-patch FILE: the change as git diff <merge-base> HEAD --binary (an empty file for none)"
  [[ -f $change_patch ]] || die 2 "--change-patch $change_patch is not a file"
fi
if [[ $side == before && -z $before_rev ]]; then
  before_rev=origin/main
fi

tree=$(git -C "$BENCH_DIR" rev-parse --show-toplevel) || die 2 "$BENCH_DIR is not in a git tree"
bench_trial_script="$tree/scripts/release/trial.sh"
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)
cd "$out_dir"
run_id=$(date -u +%Y%m%dT%H%M%SZ)

if ((detach)); then
  log="$out_dir/pn-$run_id.log"
  detached_env=()
  [[ -z ${CARGO_HOME:-} ]] || detached_env+=(--setenv="CARGO_HOME=$CARGO_HOME")
  [[ -z ${RUSTUP_HOME:-} ]] || detached_env+=(--setenv="RUSTUP_HOME=$RUSTUP_HOME")
  # The remote runner's TMPDIR belongs to the SSH command. Carry a caller's scratch path into the detached run only
  # when it is inside HOME, where the task keeps its durable files.
  if [[ -n ${TMPDIR:-} && $TMPDIR == "$HOME/"* ]]; then
    mkdir -p "$TMPDIR"
    detached_env+=(--setenv="TMPDIR=$TMPDIR")
  fi
  forwarded=(--side "$side" --out-dir "$out_dir" --repetitions "$repetitions"
    --scenarios "$scenarios" --open-conditions "$open_conditions" --lock-wait "$lock_wait"
    ${open_rate_args+"${open_rate_args[@]}"})
  [[ -n $before_rev ]] && forwarded+=(--before-rev "$before_rev")
  [[ -n $before_clone ]] && forwarded+=(--before-clone "$before_clone")
  [[ -n $change_patch ]] && forwarded+=(--change-patch "$change_patch")
  [[ -n $change_files ]] && forwarded+=(--change-files "$change_files")
  [[ -n $assets_dir ]] && forwarded+=(--assets-dir "$assets_dir")
  [[ -n $seed_home ]] && forwarded+=(--seed-home "$seed_home")
  if ((seed_only)); then
    forwarded+=(--seed-only)
  fi
  [[ -n $scenario_repetitions ]] && forwarded+=(--scenario-repetitions "$scenario_repetitions")
  ((foryou_shots)) && forwarded+=(--foryou-shots)
  systemd-run --user --unit="pn-$run_id" --collect --working-directory="$tree" \
    --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
    "${detached_env[@]}" \
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
chrome_version=$("$chrome" --version 2>/dev/null | head -1)
[[ -n $chrome_version ]] || die 2 "$chrome --version printed nothing"

run_dir="$out_dir/$run_id"
mkdir -p "$run_dir"
readonly RUN_DIR=$run_dir
export POOR_NETWORK_ASSETS=${assets_dir:-$out_dir/assets}
mkdir -p "$POOR_NETWORK_ASSETS"

# The bench code is this tree's. The after side is this tree in the explicit mode, and R with the change in the stored
# one.
bench_tree=$(git -C "$tree" rev-parse 'HEAD:dev/poor-network')
after_commit=$(git -C "$tree" rev-parse HEAD)
after_src=$tree
after_label="tree $(git -C "$tree" rev-parse 'HEAD^{tree}')"

# The stub backend goes first on the server's PATH, so the charlie-code backend runs the scripted reply.
export PATH="$BENCH_DIR/stub:$PATH"

echo "poor-network: run $run_id side=$side mode=$mode chrome=$chrome"
echo "poor-network: after tree $tree at $after_label"

# The store's paths, and the stored reference R of this run when it has one.
store_paths=$(node "$BENCH_DIR/before-store.mjs" paths)
store_root=$(node -p "JSON.parse(process.argv[1]).root" "$store_paths")
load_safe_file=$(node -p "JSON.parse(process.argv[1]).loadSafe" "$store_paths")

# The changed files: from the patch in the stored mode, or from the named list. `auto` reads them below.
changed_files=()
if [[ $mode == stored ]]; then
  if [[ -s $change_patch ]]; then
    while IFS=$'\t' read -r _added _removed path_name; do
      changed_files+=("$path_name")
    done < <(git apply --numstat "$change_patch")
  fi
elif [[ -n $change_files ]]; then
  [[ -f $change_files ]] || die 2 "--change-files $change_files is not a file"
  mapfile -t changed_files <"$change_files"
fi
if [[ $scenarios == auto ]]; then
  [[ ${#changed_files[@]} -gt 0 || $mode == stored ]] \
    || die 2 "--scenarios auto needs the changed files: --change-files FILE with --before-rev"
  selection=$(node "$BENCH_DIR/scenario-map.mjs" ${changed_files[@]+"${changed_files[@]}"})
  scenarios=$(node -p "JSON.parse(process.argv[1]).scenarios.join(',')" "$selection")
  open_list=$(node -p "JSON.parse(process.argv[1]).openConditions.join(',')" "$selection")
  [[ -z $open_list ]] || open_conditions=$open_list
  echo "poor-network: --scenarios auto selected $scenarios; open conditions ${open_list:-none};" \
    "from ${#changed_files[@]} changed file(s)"
fi

# The after tree's build. In the stored mode, R is the reference: its clone takes the change's patch.
if [[ $mode == stored ]]; then
  stored_rev=$(node "$BENCH_DIR/before-store.mjs" pick "$store_root" || true)
  if [[ -n $stored_rev ]]; then
    before_rev_resolved=$stored_rev
  else
    before_rev_resolved=$(git -C "$before_clone" rev-parse --verify --quiet 'origin/main^{commit}') \
      || die 2 "origin/main does not name a commit in $before_clone; fetch it there first"
  fi
fi

# Prepares the after tree: in the stored mode, a clone of the before clone at R, with the change applied. The rebase
# that the stored mode asks for is a patch on R: a change that does not apply names the base to measure instead.
prepare_after_src() {
  local rev=$1
  local after_repo="$run_dir/after-repo"
  git clone -q --shared --no-checkout "$before_clone" "$after_repo"
  git -C "$after_repo" checkout -q --detach "$rev"
  if [[ -s $change_patch ]]; then
    git -C "$after_repo" apply --whitespace=nowarn "$change_patch" \
      || die 2 "the change in $change_patch does not apply to $rev; to measure the change on its own base," \
        "run with --before-rev <merge-base of the change>"
  fi
  after_src=$after_repo
  after_commit=$(git -C "$after_repo" rev-parse HEAD)
  if [[ -s $change_patch ]]; then
    after_label="$rev + change $(sha256sum "$change_patch" | cut -c1-12)"
  else
    after_label="$rev"
  fi
}

# The stored plan of the run's rows against R: each row's decision, the stored files it reuses and the open rate they
# ran at. $1 is the stale rule: report keeps a stale row's decision; measure measures it once R has moved to a newer
# one.
plan_stored_rows() {
  node "$BENCH_DIR/before-store.mjs" plan --rev "$before_rev_resolved" --chrome "$chrome_version" \
    --seed "$seed_hash" --tree "$bench_tree" --scenarios "$scenarios" --open-conditions "$open_csv" \
    --root "$store_root" --loadsafe "$load_safe_file" --stale "$1"
}

# The mock home holds the view's rows that the bench's own sessions and the default cron tasks' sessions do not:
# VIEW.rows less BENCH_SESSIONS less DEFAULT_CRON_ROWS, each one a session of the mock's active corpus, with a task spec
# sized to the live view's rows (the view shape). The mock binary is the tree's own.
(cd "$tree/rust" && cargo build --release --locked -p charliebot-mock-home)
mock_bin="$tree/rust/target/release/mock-home"
[[ -x $mock_bin ]] || die 1 "the build did not produce mock-home"
mock_version=$("$mock_bin" version)
mock_sessions=$(node -e '
  import(process.argv[1]).then((profile) => {
    process.stdout.write(String(profile.VIEW.rows - profile.BENCH_SESSIONS - profile.DEFAULT_CRON_ROWS));
  });
' "file://$BENCH_DIR/profile.mjs")
# A run that names the search scenario takes the search home instead: the view home's sessions plus the search view's
# match sets (archived sessions named for each query). The default homes keep their own names and bytes.
home_shape=view
case ",$scenarios," in
  *,search,*) home_shape=search ;;
esac
if [[ -n $seed_home ]]; then
  [[ $home_shape == view ]] || die 2 "--seed-home holds the view home; the search scenario needs the search home"
  mock_home=$seed_home
else
  mock_home="$HOME/improve/mock-home/$mock_version-$home_shape-$mock_sessions"
  if [[ ! -d $mock_home ]]; then
    echo "poor-network: generating the $home_shape home of $mock_sessions sessions at $mock_home"
    "$mock_bin" generate "$mock_home" --sessions "$mock_sessions" --shape "$home_shape"
  fi
fi
# Both sides read a copy of the home that this run owns: the shared home is also the improve trials' seed, and a change
# to it would move the newest-row targets between the sides. The copy's content hash is recorded, and each side checks
# it before the side starts.
seed_dir="$run_dir/seed"
mkdir -p "$seed_dir"
seed_copy="$seed_dir/$(basename "$mock_home")"
cp -a "$mock_home" "$seed_copy"
seed_hash=$(seed_content_hash "$seed_copy")
printf '%s  %s\n' "$seed_hash" "$seed_copy" > "$run_dir/seed.sha256"
echo "poor-network: seed $mock_home copied to $seed_copy (content sha256 $seed_hash)"
mock_home=$seed_copy

node "$BENCH_DIR/reply.mjs" "$run_dir/replies"

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

# Builds the server of a tree at a commit: $1 the source directory, $2 its target directory name under out_dir.
build_server() {
  local source_dir=$1 target=$2
  (cd "$source_dir/rust" && CARGO_TARGET_DIR="$out_dir/$target" \
    cargo build --release --locked -p charliebot-server -p charliebot-cli)
  [[ -x $out_dir/$target/release/charliebot-server ]] \
    || die 1 "the build of $source_dir did not produce charliebot-server"
  printf '%s\n' "$out_dir/$target/release/charliebot-server"
}

# Runs one side as the command of a serve trial. $1 side, $2 trial script, $3 commit, $4 server binary, $5 the rows
# the side measures (its scenarios and open conditions, as the bench reads them).
run_side() {
  local name=$1 trial_script=$2 commit=$3 server_bin=$4 trial_root trial_status
  local side_scenarios=$5 side_conditions=${6:-$open_conditions}
  # Each invocation and side owns its trial tree: the tree outlives a yield of the lock, so a second invocation on the
  # same commit must not find it.
  # The root is unique to this side and invocation, and short enough for trial.sh's socket-path preflight (side-root.sh).
  trial_root=$("$BENCH_DIR/side-root.sh")
  local label=$commit
  [[ $name == after ]] && label=$after_label
  export CHARLIEBOT_HOME=$live_home
  export CHARLIEBOT_RELEASE_RUN_DIR="$run_dir/evidence-$name"
  export POOR_NETWORK_REPLY_DIR="$run_dir/replies"
  # Chrome's process-singleton socket lives under TMPDIR, and the trial points TMPDIR into its tree. The caller's own
  # TMPDIR is short and writable, so Chrome gets it back beside the trial's long one.
  export POOR_NETWORK_CHROME_TMPDIR=${TMPDIR:-/tmp}
  export TRIAL_PORT_MIN=$BENCH_PORT_MIN TRIAL_PORT_MAX=$BENCH_PORT_MAX
  # The seed copy must still hold the hash recorded at the run's start before this side reads it.
  [[ $(seed_content_hash "$mock_home") == "$seed_hash" ]] \
    || die 1 "the seed copy $mock_home changed since the run recorded $seed_hash; the $name side did not start"
  # The after side of one invocation checks its newest-row targets against the before side's JSON.
  local expect_args=() reps_args=() shots_args=()
  [[ -z $scenario_repetitions ]] || reps_args=(--scenario-repetitions "$scenario_repetitions")
  if [[ $name == after ]] && ((foryou_shots)); then
    shots_args=(--foryou-shots-dir "$run_dir/foryou-shots")
  fi
  if [[ $name == after && $mode == explicit && $side == both && $seed_only == 0 \
    && ",$side_scenarios," == *",open,"* ]]; then
    expect_args=(--expect-targets "$run_dir/before.json")
  fi
  # The trial starts and stops the server; the bench takes the lock one step at a time inside it (bench.mjs).
  set +e
  systemd-run --user --scope -p MemoryMax=16G -p MemorySwapMax=0 \
    --unit="pn-$run_id-$name" "$trial_script" --serve --in-caller-scope \
    --tree-root "$trial_root" --seed-home "$mock_home" \
    "$commit" "$server_bin" -- \
    node "$BENCH_DIR/bench.mjs" --side "$name" --out "$run_dir/$name.json" \
      --mock-home "$mock_home" --mock-home-bin "$mock_bin" --reply-dir "$run_dir/replies" \
      --assets "$POOR_NETWORK_ASSETS" --commit "$label" \
      --repetitions "$repetitions" --scenarios "$side_scenarios" --open-conditions "$side_conditions" \
      --lock-wait "$lock_wait" \
      ${open_rate_args+"${open_rate_args[@]}"} ${expect_args+"${expect_args[@]}"} \
      ${mode_args+"${mode_args[@]}"} ${reps_args+"${reps_args[@]}"} ${shots_args+"${shots_args[@]}"} &
  trial_pid=$!
  # The side JSON is the bench's last artifact, and a bench that has written it can still sit alive and hold the trial.
  # Once the JSON is SIDE_EXIT_GRACE_SECONDS old and the trial still runs, the runner ends the trial tree itself, so the
  # kill loses nothing but the hang.
  local json_seen_at="" ended_tree=0
  while :; do
    kill -0 "$trial_pid" 2>/dev/null || break
    if [[ -f "$run_dir/$name.json" ]]; then
      [[ -n $json_seen_at ]] || json_seen_at=$(date +%s)
      if (( $(date +%s) - json_seen_at >= SIDE_EXIT_GRACE_SECONDS )) \
        && systemctl --user is-active --quiet "pn-$run_id-$name.scope"; then
        echo "poor-network: the $name side's JSON is ${SIDE_EXIT_GRACE_SECONDS}s old and its bench is still running;" \
          "ending the trial tree"
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
  if ((trial_status == BENCH_LOCK_TIMEOUT_EXIT)); then
    die "$BENCH_LOCK_TIMEOUT_EXIT" "the $name side did not get the bench lock within $lock_wait s"
  fi
  echo "poor-network: $name side exit $trial_status"
  [[ -f "$run_dir/$name.json" ]] || die 1 "the $name side wrote no JSON"
  if ((trial_status == RATE_GATE_EXIT)); then
    die "$RATE_GATE_EXIT" "the $name side's CPU probe failed the rate gate twice; see $run_dir/$name.json"
  fi
  # trial.sh removes its tree and temp root; the empty root from mktemp goes with them.
  rmdir "$trial_root"
  return "$trial_status"
}

# name_trial_commit COMMIT SOURCE makes COMMIT the bench tree's origin/main, fetching it from SOURCE when the tree
# lacks it. Both sides run the bench tree's trial.sh, and the trial reads its commit and its origin/main merge base from
# the repo its script runs in. The synced tree holds only the upload's own commit, so the other commit is fetched into it.
name_trial_commit() {
  local commit=$1 source=$2
  git -C "$tree" cat-file -e "$commit^{commit}" 2>/dev/null \
    || git -C "$tree" fetch -q "$source" "$commit" \
    || die 2 "the trial commit $commit cannot be fetched into $tree from $source"
  git -C "$tree" update-ref refs/remotes/origin/main "$commit"
}

# The before side: the commit `before_rev_resolved` (stored mode) or `before_rev` (explicit mode), measured for the rows
# named below. Its server is built from that commit; both sides use the bench tree's trial script.
measure_before() {
  local rev=$1 side_scenarios=$2 side_conditions=$3
  local before_commit before_bin before_src
  before_commit=$(git -C "$before_clone" rev-parse --verify --quiet "$rev^{commit}") \
    || die 2 "$rev does not name a commit in $before_clone; fetch it there first"
  before_src="$run_dir/before-src"
  mkdir -p "$before_src"
  git -C "$before_clone" archive "$before_commit" | tar -x -C "$before_src"
  echo "poor-network: before commit $before_commit from $before_clone"
  name_trial_commit "$before_commit" "$before_clone"
  before_bin=$(build_server "$before_src" target-before)
  (cd "$before_src" && run_side before "$bench_trial_script" "$before_commit" "$before_bin" \
    "$side_scenarios" "$side_conditions") \
    || die "$?" "the before side failed; see $run_dir/before.json"
}

# The bench's rows: the scenarios and open conditions the run selected. Stored mode splits them into the rows it
# measures on the before side and the rows it reuses.
stored_files=()
before_scenarios=
before_conditions=
stored_mode_before=0
if [[ $mode == stored ]]; then
  stored_mode_before=1
  open_csv=
  if [[ ",$scenarios," == *,open,* ]]; then
    open_csv=$open_conditions
  fi
  plan=$(plan_stored_rows report)
  stale=$(node -p "Object.values(JSON.parse(process.argv[1]).decisions).includes('stale')" "$plan")
  if [[ $stale == true ]]; then
    # A stored row that failed its target on R: the run takes the newest origin/main as its reference and measures
    # every row there, so R's failing row is replaced by the newest main's measurement.
    before_rev_resolved=$(git -C "$before_clone" rev-parse --verify --quiet 'origin/main^{commit}')
    echo "poor-network: a stored row failed its target on the stored R;" \
      "this run takes origin/main $before_rev_resolved as R"
    plan=$(plan_stored_rows measure)
  fi
  echo "poor-network: stored plan for R $before_rev_resolved: $plan"
  while IFS= read -r stored_file; do
    [[ -z $stored_file ]] || stored_files+=("$stored_file")
  done < <(node -p "JSON.parse(process.argv[1]).reuse.join('\\n')" "$plan")
  stored_open_rate=$(node -p "JSON.parse(process.argv[1]).openRate ?? ''" "$plan")
  measured_rows=$(node -p "Object.entries(JSON.parse(process.argv[1]).decisions)
    .filter(([, d]) => d === 'measure').map(([r]) => r).join(',')" "$plan")
  for row in ${measured_rows//,/ }; do
    case $row in
      open:reference) ;;
      open:*) before_conditions+="${before_conditions:+,}${row#open:}" ;;
      *) before_scenarios+="${before_scenarios:+,}$row" ;;
    esac
  done
  # The search row is measured on every run, so both sides measure it whatever the store holds.
  if [[ ",$scenarios," == *,search,* ]]; then
    before_scenarios+="${before_scenarios:+,}search"
  fi
  # The open group is measured whole (before-store.mjs planRows), so the reference cell runs with the conditions.
  if [[ -n $before_conditions ]]; then
    before_scenarios+="${before_scenarios:+,}open"
  fi
  if [[ -n $stored_open_rate && ${#open_rate_args[@]} -eq 0 ]]; then
    open_rate_args=(--open-cpu-rate "$stored_open_rate")
    echo "poor-network: the open cells run at the stored rows' CPU rate $stored_open_rate"
  fi
fi

if [[ $mode == stored ]]; then
  prepare_after_src "$before_rev_resolved"
fi
after_bin=$(build_server "$after_src" target-after)


# Shares the open CPU rate that the before side derived: the after side runs its open cells at that rate, so the two
# sides' numbers compare at one CPU speed. The before side has measured (a failure would have stopped above), so its
# JSON names the rate whenever the open scenario was selected.
share_open_rate() {
  local open_cpu_rate
  open_cpu_rate=$(node -e '
    const side = require(process.argv[1]);
    const rate = side.open?.cpu?.rate;
    if (typeof rate !== "number" || !(rate >= 1)) {
      throw new Error(`before.json open.cpu.rate is ${JSON.stringify(rate)}`);
    }
    process.stdout.write(String(rate));
  ' "$run_dir/before.json") || die 1 "could not read the open CPU rate to share from $run_dir/before.json"
  open_rate_args=(--open-cpu-rate "$open_cpu_rate")
  echo "poor-network: the after side runs the open scenario at the before side's CPU rate $open_cpu_rate"
}

if [[ $side == before || $side == both ]]; then
  if ((seed_only)); then
    # The sizing check measures and stores no row, so its before side sits at R whatever the store holds.
    measure_before "${before_rev_resolved:-$before_rev}" "$scenarios" "$open_conditions"
  elif [[ $mode == explicit || $side == before ]]; then
    measure_before "$before_rev" "$scenarios" "$open_conditions"
  elif [[ -n $before_scenarios || -n $before_conditions ]]; then
    measure_before "$before_rev_resolved" "$before_scenarios" "$before_conditions"
  fi
  if [[ $side == both && ",$scenarios," == *",open,"* && ${#open_rate_args[@]} -eq 0 ]] && ((!seed_only)); then
    share_open_rate
  fi
fi

if [[ $stored_mode_before == 1 ]]; then
  # The before side the report reads: the rows measured now, with the stored rows merged in, each row's source named.
  measured_arg=()
  [[ -f $run_dir/before.json ]] && measured_arg=(--measured "$run_dir/before.json")
  stored_arg=()
  ((${#stored_files[@]} == 0)) || stored_arg=(--stored "$(IFS=,; echo "${stored_files[*]}")")
  if [[ $side == both ]]; then
    template_arg=(--template "$run_dir/after.json")
  fi
fi

if [[ $side == after || $side == both ]]; then
  name_trial_commit "$after_commit" "$after_src"
  (cd "$after_src" && run_side after "$bench_trial_script" "$after_commit" "$after_bin" \
    "$scenarios" "$open_conditions") \
    || die "$?" "the after side failed; see $run_dir/after.json"
fi

if ((seed_only)); then
  echo "poor-network: seed-only sizes in $run_dir"
  exit 0
fi

if [[ $stored_mode_before == 1 ]]; then
  node "$BENCH_DIR/before-store.mjs" assemble --out "$run_dir/before-assembled.json" \
    ${measured_arg+"${measured_arg[@]}"} ${template_arg+"${template_arg[@]}"} ${stored_arg+"${stored_arg[@]}"}
  # The rows measured now are stored when they are load-safe; the stored ones are already there.
  if [[ -n ${measured_rows:-} && -f $run_dir/before.json ]]; then
    node "$BENCH_DIR/before-store.mjs" save --rev "$before_rev_resolved" --chrome "$chrome_version" \
      --seed "$seed_hash" --tree "$bench_tree" --rows "$measured_rows" \
      --side "$run_dir/before.json" --root "$store_root" --loadsafe "$load_safe_file" >/dev/null
  fi
  mv "$run_dir/before-assembled.json" "$run_dir/before.json"
fi

echo "poor-network: JSON in $run_dir"
if [[ $side == both ]]; then
  node "$BENCH_DIR/report.mjs" "$run_dir/before.json" "$run_dir/after.json"
else
  node "$BENCH_DIR/report.mjs" "$run_dir/$side.json"
fi

#!/usr/bin/env bash
# The remote-wake bench: two CharlieBot builds serve side by side against one job list in a container, and the run
# reports each wake's lag against the job's own end time.
#
#   dev/remote-wake/run.sh --before <rev> [--after <rev>] --out-dir <dir> [options]
#
# Run it on the build host through dev/remote-run/run.sh; see README.md. The bench builds both sides from a source
# clone, starts one container that acts as the remote host, runs both builds' serve trials segment by segment, and
# writes the report into <dir>/run-<UTC>/.
#
# The two builds come from commits: --before names a revision of --source-repo (default origin/main), and --after
# names one too (default the current tree's HEAD). Each side builds from its own extracted tree with the bench host's
# cargo. A revision the source clone lacks is fetched from --fetch-from.
#
# Options:
#   --before <rev>            the "before" commit (default origin/main)
#   --after <rev>             the "after" commit (default the current tree's HEAD)
#   --source-repo <dir>       the clone that holds both commits (default the current tree)
#   --fetch-from <url>        a git URL to fetch a commit the source clone lacks
#   --out-dir <dir>           where the run's directory goes; required
#   --waits <n>               waits per build (default 100)
#   --seed <n>                the workload seed (default 20261108)
#   --stagger-seconds <n>     spread of the job starts (default 3600)
#   --restarts <n>            server restarts (default 3); 0 runs one segment
#   --faults <list>           cut,hold or both (default cut,hold); empty for none
#   --cut-seconds <n>         length of the connection cut (default 60)
#   --hold-seconds <n>        length of the new-connection hold (default 300)
#   --rtt-ms <n>              netem round-trip delay (default 309; 708 is the other measured value)
#   --length-cap <n>          longest sampled job length (default 7200; a smoke run uses 300)
#   --mock-sessions <n>       sessions in each build's generated mock home (default 4)
#   --image <name>            container image (default built from container/)
#   --gate smoke|full         the report's gate (default full)
#   --detach                  start the run as a transient systemd user unit and return at once
#
# Exit: 0 the gate passed; 1 a build, trial or measurement failed; 2 a precondition failed and nothing started;
# 3 the gate failed; 75 the bench lock was not free within the wait.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly SCRIPT_DIR
source "$SCRIPT_DIR/lock.sh"

usage() {
  sed -n '2,/^set -euo pipefail$/{/^set -euo pipefail$/d;s/^# \{0,1\}//;p}' "${BASH_SOURCE[0]}"
}

die() {
  local code=$1
  shift
  printf 'remote-wake: %s\n' "$*" >&2
  exit "$code"
}

before_rev=origin/main
after_rev=
source_repo=
fetch_from=
out_dir=
waits=100
seed=20261108
stagger_seconds=3600
restarts=3
faults=cut,hold
cut_seconds=60
hold_seconds=300
rtt_ms=309
length_cap=7200
mock_sessions=4
image=
gate=full
detach=0
detached=0

while (($#)); do
  case $1 in
    --before) before_rev=${2:?--before needs a value}; shift 2 ;;
    --after) after_rev=${2:?--after needs a value}; shift 2 ;;
    --source-repo) source_repo=${2:?--source-repo needs a value}; shift 2 ;;
    --fetch-from) fetch_from=${2:?--fetch-from needs a value}; shift 2 ;;
    --out-dir) out_dir=${2:?--out-dir needs a value}; shift 2 ;;
    --waits) waits=${2:?--waits needs a value}; shift 2 ;;
    --seed) seed=${2:?--seed needs a value}; shift 2 ;;
    --stagger-seconds) stagger_seconds=${2:?--stagger-seconds needs a value}; shift 2 ;;
    --restarts) restarts=${2:?--restarts needs a value}; shift 2 ;;
    --faults) faults=${2:?--faults needs a value}; shift 2 ;;
    --cut-seconds) cut_seconds=${2:?--cut-seconds needs a value}; shift 2 ;;
    --hold-seconds) hold_seconds=${2:?--hold-seconds needs a value}; shift 2 ;;
    --rtt-ms) rtt_ms=${2:?--rtt-ms needs a value}; shift 2 ;;
    --length-cap) length_cap=${2:?--length-cap needs a value}; shift 2 ;;
    --mock-sessions) mock_sessions=${2:?--mock-sessions needs a value}; shift 2 ;;
    --image) image=${2:?--image needs a value}; shift 2 ;;
    --gate) gate=${2:?--gate needs a value}; shift 2 ;;
    --detach) detach=1; shift ;;
    --detached) detached=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die 2 "unknown argument: $1" ;;
  esac
done

[[ -n $out_dir ]] || die 2 "--out-dir is required: the run's files go there, outside the tree"
[[ $gate == smoke || $gate == full ]] || die 2 "--gate must be smoke or full, got $gate"
[[ $waits =~ ^[1-9][0-9]*$ ]] || die 2 "--waits must be a positive integer"
[[ $restarts =~ ^[0-9]+$ ]] || die 2 "--restarts must be a non-negative integer"
[[ $rtt_ms =~ ^[1-9][0-9]*$ ]] || die 2 "--rtt-ms must be a positive integer"
[[ $length_cap =~ ^[1-9][0-9]*$ ]] || die 2 "--length-cap must be a positive integer"

# Detach: run this script again as a transient systemd user unit, whose memory cap is what the serve trials keep their
# servers under. The unit outlives the ssh that started it, so a run longer than one tool call completes on its own.
if ((detach)) && ((!detached)); then
  mkdir -p "$out_dir"
  out_dir=$(cd "$out_dir" && pwd -P)
  id=$(date -u +%Y%m%dT%H%M%SZ)-$$
  log="$out_dir/remote-wake-$id.log"
  forwarded=(
    --before "$before_rev" --out-dir "$out_dir" --waits "$waits" --seed "$seed"
    --stagger-seconds "$stagger_seconds" --restarts "$restarts" --faults "$faults"
    --cut-seconds "$cut_seconds" --hold-seconds "$hold_seconds" --rtt-ms "$rtt_ms"
    --length-cap "$length_cap" --mock-sessions "$mock_sessions" --gate "$gate" --detached
  )
  if [[ -n $after_rev ]]; then forwarded+=(--after "$after_rev"); fi
  if [[ -n $source_repo ]]; then forwarded+=(--source-repo "$source_repo"); fi
  if [[ -n $fetch_from ]]; then forwarded+=(--fetch-from "$fetch_from"); fi
  if [[ -n $image ]]; then forwarded+=(--image "$image"); fi
  systemd-run --user --unit="rw-$id" --collect -p MemoryMax=32G --working-directory="$PWD" \
    --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
    bash -c "exec bash '$SCRIPT_DIR/run.sh' ${forwarded[*]@Q} >>'$log' 2>&1" >/dev/null
  printf 'started unit rw-%s\nlog: %s\nout-dir: %s\ndone marker: %s/.done\n' "$id" "$log" "$out_dir" "$out_dir"
  exit 0
fi

for tool in git node cargo docker flock systemd-run ssh ssh-keygen; do
  command -v "$tool" >/dev/null || die 2 "missing tool on this host: $tool"
done

tree=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null) || die 2 "$SCRIPT_DIR is not in a git tree"
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd -P)
run_id=$(date -u +%Y%m%dT%H%M%SZ)-$$
run_dir="$out_dir/run-$run_id"
mkdir -p "$run_dir"
rm -f "$out_dir/.done"
printf '%s\n' "$run_dir" >"$out_dir/.last-run"
echo "remote-wake: run $run_id -> $run_dir"

container="rw-$run_id"
container_created=0
# The orchestrator names each trial tree `$tree_prefix-<b><segment>`, and cleanup removes exactly that set, so the
# prefix reaches the orchestrator through meta.json instead of being rebuilt from the run id on one side only.
tree_prefix="/tmp/rw-$run_id"

cleanup() {
  local code=$?
  trap - EXIT
  if [[ -n ${ctl_base:-} && $ctl_base == /tmp/rwc.* ]]; then
    rm -rf -- "$ctl_base"
  fi
  for tree_root in "$tree_prefix"-?*; do
    if [[ -e $tree_root ]]; then
      rm -rf -- "$tree_root"
    fi
  done
  if compgen -G "$tree_prefix-?*" >/dev/null; then
    printf 'remote-wake: trial trees remain after cleanup under %s\n' "$tree_prefix" >&2
    code=1
  fi
  if ((container_created)); then
    docker rm -f "$container" >/dev/null 2>&1 || true
    if docker ps -a --format '{{.Names}}' | grep -qx "$container"; then
      printf 'remote-wake: the container %s remains after cleanup\n' "$container" >&2
      code=1
    fi
  fi
  if [[ ! -f $out_dir/.done ]]; then
    printf 'fail\n' >"$out_dir/.done"
  fi
  exit "$code"
}
trap cleanup EXIT

[[ -d $source_repo ]] || source_repo=$tree
source_repo=$(cd "$source_repo" && pwd -P)
git -C "$source_repo" rev-parse --git-dir >/dev/null 2>&1 || die 2 "--source-repo is a git clone: $source_repo"
git -C "$tree" rev-parse --git-dir >/dev/null 2>&1 || die 2 "$tree is not a git tree"

# One scratch clone per side, checked out at its commit. trial.sh extracts its own commit from the clone, so the side's
# own scripts/release/trial.sh runs the side.
prepare_side() {
  local side=$1 rev=$2 source=$3 src commit
  src="$run_dir/src-$side"
  git clone -q --shared --no-checkout "$(git -C "$source" rev-parse --path-format=absolute --git-common-dir)" "$src" \
    || die 1 "cannot clone $source for the $side side"
  if commit=$(git -C "$src" rev-parse --verify --quiet "$rev^{commit}"); then
    :
  else
    [[ -n $fetch_from ]] || die 2 "$rev does not name a commit in $source; pass --fetch-from"
    git -C "$src" fetch -q "$fetch_from" "$rev" || die 1 "fetching $rev from $fetch_from failed"
    commit=$(git -C "$src" rev-parse --verify --quiet "$rev^{commit}") \
      || commit=$(git -C "$src" rev-parse --verify 'FETCH_HEAD^{commit}') \
      || die 1 "$fetch_from did not give the commit for $rev"
  fi
  git -C "$src" update-ref refs/remotes/origin/main "$commit"
  git -C "$src" checkout -q --detach "$commit"
  printf '%s' "$commit"
}

before_commit=$(prepare_side before "$before_rev" "$source_repo")
if [[ -n $after_rev ]]; then
  after_commit=$(prepare_side after "$after_rev" "$source_repo")
else
  after_commit=$(prepare_side after HEAD "$tree")
fi
echo "remote-wake: before $before_rev=$before_commit"
echo "remote-wake: after ${after_rev:-current tree}=$after_commit"

sccache=
if command -v sccache >/dev/null 2>&1; then
  sccache=$(command -v sccache)
  : "${SCCACHE_DIR:=$HOME/.cache/sccache}"
  export SCCACHE_DIR
fi
build_side() {
  local side=$1
  local -a build_env=(CARGO_TARGET_DIR="$run_dir/target-$side" CARGO_BUILD_JOBS=4)
  if [[ -n $sccache ]]; then
    build_env+=("RUSTC_WRAPPER=$sccache")
  fi
  echo "remote-wake: building the $side side"
  (
    cd "$run_dir/src-$side/rust"
    env "${build_env[@]}" cargo build --release --locked -p charliebot-server -p charliebot-cli -p charliebot-mock-home
  ) || return $?
}
build_side before || die 1 "the before build failed"
if [[ $before_commit == $after_commit ]]; then
  echo "remote-wake: both sides name $before_commit, so the after side runs the before build"
else
  build_side after || die 1 "the after build failed"
fi
after_target="$run_dir/target-after"
if [[ $before_commit == "$after_commit" ]]; then
  after_target="$run_dir/target-before"
fi
for side in before after; do
  target="$run_dir/target-$side"
  if [[ $side == after ]]; then
    target=$after_target
  fi
  for binary in charliebot-server charliebot mock-home; do
    [[ -x "$target/release/$binary" ]] || die 1 "the $side build produced no $binary"
  done
done

# The container is the remote host. A published loopback port keeps it off the network; the bench reaches it only
# through the trial servers' ssh and through docker exec.
image=${image:-remote-wake-bench:$run_id}
docker build -q -t "$image" "$SCRIPT_DIR/container" || die 1 "the container image build failed"
docker run -d --init --name "$container" --cap-add=NET_ADMIN -p 127.0.0.1::22 "$image" >/dev/null
container_created=1
for _ in $(seq 1 150); do
  docker exec "$container" sh -c 'ss -ltn | grep -q ":22 "' >/dev/null 2>&1 && break
  sleep 0.2
done
docker exec "$container" sh -c 'ss -ltn | grep -q ":22 "' >/dev/null 2>&1 \
  || die 1 "the container's sshd did not start"
docker exec "$container" tc qdisc add dev eth0 root netem delay "${rtt_ms}ms" \
  || die 1 "netem delay ${rtt_ms}ms was refused in the container"
port=$(docker port "$container" 22/tcp | head -1)
port=${port##*:}
[[ $port =~ ^[0-9]+$ ]] || die 1 "the container published no ssh port"
echo "remote-wake: container $container on 127.0.0.1:$port with ${rtt_ms} ms netem delay"

# One user and one key per build: the sshd's own log names the user of every authenticated connection, so the report
# attributes the container's connections to a build.
# The server's control sockets live under /tmp: a Unix socket path is limited to 108 bytes, and the trial home's path is
# longer than that. Every segment of a build shares one directory, so a master outlives the segment restart.
ctl_base=$(mktemp -d /tmp/rwc.XXXXXX)
ssh_users=()
for side in before after; do
  if [[ $side == before ]]; then user=rwa; else user=rwb; fi
  ssh_users+=("$user")
  ssh-keygen -q -t ed25519 -N '' -f "$run_dir/key-$side" >/dev/null
  docker exec -i -u 0 "$container" sh -c "
    useradd -m -s /bin/sh $user &&
    usermod -p '*' $user &&
    mkdir -p /home/$user/.ssh &&
    cat > /home/$user/.ssh/authorized_keys &&
    chown -R $user:$user /home/$user/.ssh &&
    chmod 700 /home/$user/.ssh &&
    chmod 600 /home/$user/.ssh/authorized_keys &&
    mkdir -p /run/bench/$user && chown $user:$user /run/bench/$user" <"$run_dir/key-$side.pub" \
    || die 1 "the container refused the $side user"
  mkdir -p "$run_dir/ssh" "$ctl_base/$side"
  cat >"$run_dir/ssh/$side.conf" <<EOF
Host rw-remote
  HostName 127.0.0.1
  Port $port
  User $user
  IdentityFile $run_dir/key-$side
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  BatchMode yes
EOF
  chmod 600 "$run_dir/ssh/$side.conf"
done

# The server's ssh calls read the operator's own ~/.ssh/config, because OpenSSH expands ~ from the password entry, not
# from $HOME. A wrapper on PATH supplies this run's config and a short control socket directory instead.
mkdir -p "$run_dir/sshbin"
ssh_binary=$(command -v ssh)
cat >"$run_dir/sshbin/ssh" <<EOF
#!/bin/sh
exec "$ssh_binary" -F "\$REMOTE_WAKE_SSH_CONFIG" -o ControlPath="\$REMOTE_WAKE_SSH_CONTROL_DIR/%C" "\$@"
EOF
chmod +x "$run_dir/sshbin/ssh"

# The trial reads its backend entry from this bench-owned live home, so the run needs no real credential and reaches no
# model. The scripted backend makes a fired wake start a turn.
mkdir -p "$run_dir/live-home"
cat >"$run_dir/live-home/config.yaml" <<'YAML'
backends:
  options:
    - id: bench-endpoint
      label: Bench endpoint
      type: charlie-code
      model: bench/stub
      api_base: http://127.0.0.1:9/v1
YAML
node "$SCRIPT_DIR/../poor-network/reply.mjs" "$tree" "$run_dir/replies" >/dev/null

mock_bin="$after_target/release/mock-home"
"$mock_bin" generate "$run_dir/homes/before-mock" --sessions "$mock_sessions" --seed 1 >/dev/null \
  || die 1 "mock-home could not generate the before home"
cp -a "$run_dir/homes/before-mock" "$run_dir/homes/after-mock" || die 1 "the after mock home could not be copied"

node "$SCRIPT_DIR/plan.mjs" --waits "$waits" --seed "$seed" --stagger-seconds "$stagger_seconds" \
  --restarts "$restarts" --faults "$faults" --cut-seconds "$cut_seconds" --hold-seconds "$hold_seconds" \
  --length-cap "$length_cap" \
  >"$run_dir/plan.json" || die 1 "the workload plan could not be generated"

node - "$run_dir/meta.json" "$run_id" "$before_commit" "$after_commit" "$rtt_ms" "$container" "$gate" \
  "$run_dir" "${ssh_users[0]}" "${ssh_users[1]}" "$after_target" "$ctl_base" "$tree_prefix" \
  <<'NODE' || die 1 "meta.json could not be written"
const [file, runId, beforeCommit, afterCommit, rttMs, container, gate, runDir, beforeUser, afterUser, afterTarget, ctlBase, treePrefix] = process.argv.slice(2);
const meta = {
  run_id: runId,
  tree_prefix: treePrefix,
  created_at: new Date().toISOString(),
  commits: { before: beforeCommit, after: afterCommit },
  binaries: {
    before: `${runDir}/target-before/release/charliebot-server`,
    after: `${afterTarget}/release/charliebot-server`,
  },
  trials: {
    before: `${runDir}/src-before/scripts/release/trial.sh`,
    after: `${runDir}/src-after/scripts/release/trial.sh`,
  },
  ssh_users: { before: beforeUser, after: afterUser },
  ctl_base: ctlBase,
  rtt_ms: Number(rttMs),
  container,
  gate,
  run_dir: runDir,
};
require('node:fs').writeFileSync(file, `${JSON.stringify(meta, null, 2)}\n`);
NODE

# The bench lock covers the container and the measured runs, so no other bench measures beside this one.
echo "remote-wake: waiting for the bench lock"
remote_wake_lock || die "$REMOTE_WAKE_LOCK_TIMEOUT_EXIT" "the bench lock was not free within $REMOTE_WAKE_LOCK_WAIT_SECONDS s"

# Both serve trials share this environment: the scripted backend, the ssh wrapper, and the reply pieces.
export PATH="$run_dir/sshbin:$SCRIPT_DIR/../poor-network/stub:$PATH"
export POOR_NETWORK_REPLY_DIR="$run_dir/replies"
export CHARLIEBOT_HOME="$run_dir/live-home"

orchestrate() {
  node "$SCRIPT_DIR/orchestrate.mjs" --run-dir "$run_dir" --container "$container" --gate "$gate"
}
set +e
if ((detached)); then
  orchestrate
else
  # The serve trials keep their servers in this scope's cgroup, whose memory cap they rely on.
  systemd-run --user --scope -p MemoryMax=32G --unit="rw-run-$run_id" bash -c \
    "cd '$SCRIPT_DIR' && exec node '$SCRIPT_DIR/orchestrate.mjs' --run-dir '$run_dir' --container '$container' --gate '$gate'"
fi
status=$?
set -e

if [[ -f $run_dir/done ]]; then
  cp "$run_dir/done" "$out_dir/.done"
else
  printf 'fail\n' >"$out_dir/.done"
fi
if ((status == 0)); then
  echo "remote-wake: gate $gate passed; results in $run_dir"
else
  echo "remote-wake: run failed with status $status; results in $run_dir" >&2
fi
exit "$status"

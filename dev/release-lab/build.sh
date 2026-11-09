#!/usr/bin/env bash
# Build one commit's release binaries on the build host and install them into the release lab.
#
#   dev/release-lab/build.sh <worktree at the commit> <lab bin dir on the build host>
#
# The worktree's HEAD names the commit, and its files are what the build compiles. The build goes through
# dev/remote-run/cargo-slot.sh, so it runs on the build host with a build slot; the install goes through
# dev/remote-run/run.sh, whose synced tree still holds rust/target. The install writes:
#   <lab bin dir>/release/{charliebot-server,charliebot,release.log,sha256sums.txt}   the CI artifact's files
#   <lab bin dir>/tools/{mock-home,startup-lab-launcher}                             the lab's own tools
#   <lab bin dir>/COMMIT                                                             the commit they were built from
# rust/target is removed afterwards, so one commit's build does not hold the build host's disk.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd -P)
REPO=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)

fail() {
  printf 'build.sh: %s\n' "$1" >&2
  exit 2
}

[ $# -eq 2 ] || fail 'usage: build.sh <worktree at the commit> <lab bin dir on the build host>'
WORKTREE=$(cd -- "$1" 2> /dev/null && pwd -P) || fail "the worktree is a directory: $1"
LAB_BIN=$2
[[ $LAB_BIN == /* ]] || fail "the lab bin dir is an absolute path on the build host: $LAB_BIN"
[[ $(git -C "$WORKTREE" rev-parse --show-toplevel 2> /dev/null) == "$WORKTREE" ]] \
  || fail "$WORKTREE is a git worktree root"
[[ -x $WORKTREE/dev/remote-run/cargo-slot.sh ]] || fail "$WORKTREE carries dev/remote-run"
COMMIT=$(git -C "$WORKTREE" rev-parse HEAD)
[[ $COMMIT =~ ^[0-9a-f]{40}$ ]] || fail "HEAD names a full commit sha: $COMMIT"

( cd "$WORKTREE/rust" && "$WORKTREE/dev/remote-run/cargo-slot.sh" build --release --locked \
  -p charliebot-server -p charliebot-cli -p charliebot-mock-home )

# The install runs on the build host, in the synced tree the build left rust/target in. It writes the lab's copy, checks
# the artifact's own checksums and removes the build output.
read -r -d '' INSTALL <<'INSTALL_SCRIPT' || true
set -eu
commit=$1
lab_bin=$2
src=rust/target/release
install -d "$lab_bin/release" "$lab_bin/tools"
cp "$src/charliebot-server" "$src/charliebot" "$lab_bin/release/"
cp "$src/mock-home" "$src/startup-lab-launcher" "$lab_bin/tools/"
chmod 755 "$lab_bin/release/charliebot-server" "$lab_bin/release/charliebot"
chmod 755 "$lab_bin/tools/mock-home" "$lab_bin/tools/startup-lab-launcher"
printf 'commit=%s\nstage=build verdict=pass log=-\n' "$commit" > "$lab_bin/release/release.log"
( cd "$lab_bin/release" && sha256sum charliebot charliebot-server release.log > sha256sums.txt )
( cd "$lab_bin/release" && sha256sum -c sha256sums.txt )
printf '%s\n' "$commit" > "$lab_bin/COMMIT"
rm -rf rust/target
INSTALL_SCRIPT

cd "$WORKTREE"
"$WORKTREE/dev/remote-run/run.sh" bash -c "$INSTALL" build-install "$COMMIT" "$LAB_BIN"
printf 'build.sh: installed %s into %s on the build host\n' "$COMMIT" "$LAB_BIN"

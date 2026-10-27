#!/usr/bin/env bash
# Install the server binary that CI built for one commit on main.
#
#   scripts/release.sh <full main commit>
#
# Exit codes:
#   2  the preflight refused and wrote nothing: the argument is not a full 40-character sha on origin/main, the
#      release directory already exists, gh is not authenticated for this repository, or CI has no successful push
#      run of both Rust jobs for the commit.
#   1  a stage failed; the stage name and its log path are printed, and nothing is installed.
#   0  the release is installed; the switch command is printed.
#
# The stages run in order:
#   download   gh downloads the charliebot-server-<commit> artifact, and sha256sum -c passes on it;
#   trial      scripts/release/trial.sh runs the trial on the downloaded binary;
#   rehearsal  scripts/release/switch-rehearsal.sh switches an isolated checkout from the release that current
#              selects to this one and back;
#   install    the release directory is built under a temporary name and renamed to <commit>.
#
# The release directory holds charliebot-server, release.log with each stage's verdict, and sha256sums.txt over every
# file. It has no PORT_BASELINE, and the current link is not changed.
#
# Environment: CHARLIEBOT_RELEASES_DIR (default $HOME/charliebot-releases); CHARLIEBOT_RELEASE_LOG_DIR (default
# $HOME/charliebot-release-logs; stage logs and evidence live here, outside the repository and the release directory,
# and are kept after a failure); CHARLIEBOT_RELEASE_TRIAL and CHARLIEBOT_RELEASE_REHEARSAL replace the trial and
# rehearsal stage commands.
set -uo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
readonly RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
readonly LOG_ROOT="${CHARLIEBOT_RELEASE_LOG_DIR:-$HOME/charliebot-release-logs}"
readonly TRIAL_CMD="${CHARLIEBOT_RELEASE_TRIAL:-$REPO_ROOT/scripts/release/trial.sh}"
readonly REHEARSAL_CMD="${CHARLIEBOT_RELEASE_REHEARSAL:-$REPO_ROOT/scripts/release/switch-rehearsal.sh}"
readonly ARTIFACT_NAME_PREFIX="charliebot-server-"

usage() {
  echo "usage: release.sh <full main commit>" >&2
  exit 2
}

refuse() {
  echo "release: preflight refused: $*" >&2
  exit 2
}

[ $# -eq 1 ] || usage
COMMIT=$1
[[ $COMMIT =~ ^[0-9a-f]{40}$ ]] || refuse "$COMMIT is not a full 40-character commit sha"

command -v gh > /dev/null 2>&1 || refuse "gh is not on PATH"
gh auth status > /dev/null 2>&1 || refuse "gh is not authenticated"
REPO_SLUG="$(gh repo view --json nameWithOwner --jq .nameWithOwner 2> /dev/null)" \
  || refuse "gh is not authenticated for this repository"

git -C "$REPO_ROOT" rev-parse --verify --quiet origin/main > /dev/null \
  || refuse "origin/main is not in $REPO_ROOT; fetch origin first"
git -C "$REPO_ROOT" merge-base --is-ancestor "$COMMIT" origin/main 2> /dev/null \
  || refuse "$COMMIT is not on origin/main"
[ ! -e "$RELEASES_DIR/$COMMIT" ] || refuse "release $RELEASES_DIR/$COMMIT already exists"

# ci_run_id: the newest push run of the CI workflow for COMMIT whose release-build and rust-test jobs succeeded.
ci_run_id() {
  local id jobs
  while IFS= read -r id; do
    [ -n "$id" ] || continue
    jobs="$(gh run view "$id" --repo "$REPO_SLUG" --json jobs 2> /dev/null)" || continue
    if python3 -I -c '
import json, sys
jobs = json.load(sys.stdin).get("jobs", [])
names = {job.get("name") for job in jobs if job.get("conclusion") == "success"}
sys.exit(0 if {"release-build", "rust-test"} <= names else 1)
' <<<"$jobs"; then
      printf '%s' "$id"
      return 0
    fi
  done < <(gh run list --repo "$REPO_SLUG" --workflow CI --commit "$COMMIT" --limit 20 \
      --json databaseId,event,headSha,conclusion 2> /dev/null | python3 -I -c '
import json, sys
for run in json.load(sys.stdin):
    if run.get("event") == "push" and run.get("headSha") == sys.argv[1] and run.get("conclusion") == "success":
        print(run["databaseId"])
' "$COMMIT")
  return 1
}

RUN_ID="$(ci_run_id)" || refuse "CI has no successful push run of release-build and rust-test for $COMMIT"

RUN_DIR="$LOG_ROOT/${COMMIT}-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p -m 700 "$RUN_DIR" || refuse "cannot create the log directory $RUN_DIR"
readonly RUN_DIR
readonly ARTIFACT_DIR="$RUN_DIR/artifact"
export CHARLIEBOT_RELEASE_RUN_DIR="$RUN_DIR"

STAGE_LINES=()

# run_stage NAME COMMAND...: runs one stage, keeping its output as that stage's log. A failed stage prints its name
# and log path and exits 1 with nothing installed.
run_stage() {
  local name=$1
  shift
  local log="$RUN_DIR/$name.log"
  if "$@" >>"$log" 2>&1; then
    STAGE_LINES+=("stage=$name verdict=pass log=$log")
    return 0
  fi
  STAGE_LINES+=("stage=$name verdict=fail log=$log")
  echo "release: stage $name failed: $log" >&2
  exit 1
}

stage_download() {
  mkdir -p "$ARTIFACT_DIR" || return 1
  gh run download "$RUN_ID" --repo "$REPO_SLUG" --name "${ARTIFACT_NAME_PREFIX}${COMMIT}" --dir "$ARTIFACT_DIR" \
    || return 1
  (cd "$ARTIFACT_DIR" && sha256sum -c sha256sums.txt)
}

stage_trial() {
  "$TRIAL_CMD" "$COMMIT" "$ARTIFACT_DIR/charliebot-server"
}

stage_rehearsal() {
  "$REHEARSAL_CMD" "$COMMIT" "$ARTIFACT_DIR"
}

# stage_install: build the release directory under a temporary name, then rename it to the commit.
stage_install() {
  local tmp="$RELEASES_DIR/.$COMMIT.$$" line
  rm -rf "$tmp"
  mkdir -m 700 "$tmp" || return 1
  if ! cp "$ARTIFACT_DIR/charliebot-server" "$tmp/charliebot-server" || ! chmod 755 "$tmp/charliebot-server"; then
    rm -rf "$tmp"
    return 1
  fi
  {
    printf 'commit=%s\n' "$COMMIT"
    for line in "${STAGE_LINES[@]}"; do
      printf '%s\n' "$line"
    done
    printf 'stage=install verdict=pass log=%s\n' "$RUN_DIR/install.log"
  } > "$tmp/release.log" || {
    rm -rf "$tmp"
    return 1
  }
  if ! (cd "$tmp" && sha256sum charliebot-server release.log > sha256sums.txt); then
    rm -rf "$tmp"
    return 1
  fi
  if ! mv -T "$tmp" "$RELEASES_DIR/$COMMIT"; then
    rm -rf "$tmp"
    return 1
  fi
}

run_stage download stage_download
run_stage trial stage_trial
run_stage rehearsal stage_rehearsal
run_stage install stage_install

echo "release: installed $RELEASES_DIR/$COMMIT"
echo "release: switch with: scripts/switch-server.sh rust $COMMIT"
exit 0

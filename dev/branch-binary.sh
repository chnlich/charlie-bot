#!/usr/bin/env bash
# Builds the server and CLI binaries of a task branch on CI and downloads them, so a live demo before
# landing costs one CI build and no local compile.
#
# Usage: dev/branch-binary.sh <branch> [dest-dir]
#
# The script pushes the branch to origin (no PR), dispatches the CI workflow on that ref, waits for the
# release-build job, downloads its artifact into dest-dir (default:
# $HOME/.cache/charliebot-branch-binaries/<branch>; a later call for the same branch replaces the earlier
# download), verifies the checksums and prints the local absolute paths of the CLI and server binaries.
#
# The dispatched ref's workflow file must itself carry the workflow_dispatch trigger: a branch whose ci.yml
# predates this change fails at dispatch with HTTP 422, which the script propagates. The trigger does not need
# to be on the default branch first.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

usage() {
  echo "usage: dev/branch-binary.sh <branch> [dest-dir]" >&2
  exit 2
}

(( $# >= 1 && $# <= 2 )) || usage
branch=$1
dest=${2:-"$HOME/.cache/charliebot-branch-binaries/$branch"}

WORKFLOW=ci.yml
JOB=release-build
# The job's own timeout-minutes caps the build; the wait gives the queue the same room.
JOB_WAIT_SECONDS=2700
# A dispatched run registers within a couple of minutes or something is wrong.
RUN_LOOKUP_SECONDS=120

# Push. git's own error text (unknown ref, rejected update) is the failure message.
git push origin "$branch:$branch"

# The run list lags the dispatch, so the lookup demands a run id that did not exist before it: without
# that guard a rerun for the same branch resolves to the previous call's run and downloads a stale binary.
latest_before=$(gh run list --workflow "$WORKFLOW" --branch "$branch" --event workflow_dispatch \
  --limit 1 --json databaseId --jq '.[0].databaseId // empty')

# Dispatch. gh prints its own error and exits non-zero; no retry.
gh workflow run "$WORKFLOW" --ref "$branch"

# Find the dispatched run.
run_id=''
while (( SECONDS < RUN_LOOKUP_SECONDS )); do
  run_id=$(gh run list --workflow "$WORKFLOW" --branch "$branch" --event workflow_dispatch \
    --limit 1 --json databaseId --jq '.[0].databaseId // empty')
  [[ -n "$run_id" && "$run_id" != "$latest_before" ]] && break
  sleep 10
done
if [[ -z "$run_id" ]]; then
  echo "error: no dispatched run of $WORKFLOW on $branch registered within ${RUN_LOOKUP_SECONDS}s" >&2
  exit 1
fi
run_url=$(gh run view "$run_id" --json url --jq .url)

# Wait for the job that produces the artifact. Polling is the wait, not a retry: the first non-success
# conclusion ends the script.
job_status=''
job_conclusion=''
while (( SECONDS < JOB_WAIT_SECONDS )); do
  line=$(gh run view "$run_id" --json jobs \
    --jq ".jobs[] | select(.name == \"$JOB\") | .status + \" \" + (.conclusion // \"\")")
  job_status=${line%% *}
  job_conclusion=${line#* }
  [[ "$job_status" == completed ]] && break
  sleep 15
done
if [[ "$job_status" != completed ]]; then
  echo "error: $JOB of run $run_id still not completed after ${JOB_WAIT_SECONDS}s: $run_url" >&2
  exit 1
fi
if [[ "$job_conclusion" != success ]]; then
  echo "error: $JOB of run $run_id concluded $job_conclusion: $run_url" >&2
  gh run view "$run_id" --json jobs \
    --jq '.jobs[] | select(.name == "release-build") | .steps[] | select(.conclusion != "success" and .conclusion != "skipped") | "  failed step: " + .name' >&2 || true
  exit 1
fi

# Replace the earlier download for this branch with the fresh artifact.
mkdir -p "$dest"
dest=$(cd "$dest" && pwd -P)
find "$dest" -mindepth 1 -maxdepth 1 -exec rm -rf {} +

sha=$(gh run view "$run_id" --json headSha --jq .headSha)
# gh prints its own error when the artifact is missing or the download fails.
gh run download "$run_id" --name "charliebot-server-$sha" --dir "$dest"

(cd "$dest" && sha256sum -c sha256sums.txt)

echo "run: $run_url"
echo "charliebot CLI: $dest/charliebot"
echo "server binary:  $dest/charliebot-server"

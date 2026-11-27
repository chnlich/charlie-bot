#!/usr/bin/env bash
# The one way to merge a pull request outside a round. Run it on the round host:
#
#   merge-outside-round.sh --evidence-dir <dir> [--review-record <file>] <PR>
#
# The runner keeps each kit revision's tools under ~/improve/runner/bin/<revision>/: the charliebot that a round
# calls, the gh shim, the leak scripts and the kit (kit/). The evidence's skills come from the runner checkout's git
# content (kit-skills.sh), so no skills directory of a revision is kept there. The link ~/improve/runner/bin/charliebot
# names the live revision, the revision of the latest launch. The round profile home is ~/improve/home, which the runner
# refreshes at every launch with the leak patterns file that github-post.sh reads.
#
# Before any call to GitHub, the script checks each item that the merge depends on. A missing item stops it with a
# refusal that names the item: exit 2, nothing merged. Then it runs the live revision's github-post.sh pr-merge with
# CHARLIEBOT_HOME set to the round profile home and the live revision's tool directory first on PATH. That call's exit
# code is the script's own.
#
# Exit codes: 2 a refusal (nothing sent); otherwise pr-merge's exit code (4: sent, but GitHub does not show it merged).
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/kit-skills.sh"

usage() {
  printf 'usage: merge-outside-round.sh --evidence-dir <dir> [--review-record <file>] <PR>\n' >&2
  exit 2
}

# refuse <reason>: name the missing item and send nothing.
refuse() {
  printf 'merge-outside-round: %s; nothing was merged.\n' "$*" >&2
  exit 2
}

evidence_dir='' review_record='' pr=''
while [[ $# -gt 0 ]]; do
  case $1 in
    --evidence-dir) [[ $# -ge 2 ]] || usage; evidence_dir=$2; shift 2 ;;
    --review-record) [[ $# -ge 2 ]] || usage; review_record=$2; shift 2 ;;
    -*) usage ;;
    *) [[ -z $pr ]] || usage; pr=$1; shift ;;
  esac
done
[[ -n $evidence_dir && -n $pr ]] || usage

[[ -z ${IMPROVE_LAUNCH_DIR-} ]] \
  || refuse "IMPROVE_LAUNCH_DIR names a round, and this entry point merges outside a round"

# The round profile home and the runner root, as remote-runner.sh sets them.
round_home="$HOME/improve/home"
runner_root="$HOME/improve/runner"

patterns="$round_home/skills_leak_patterns.local.txt"
[[ -r $patterns ]] || refuse "the round profile home lacks its leak patterns file: $patterns"

live_link="$runner_root/bin/charliebot"
[[ -L $live_link ]] || refuse "the live charliebot link is missing: $live_link"
live_target=$(readlink -- "$live_link")
live=${live_target#charliebot-}
[[ $live_target == "charliebot-$live" && $live =~ ^[0-9a-f]{40}$ ]] \
  || refuse "the live charliebot link names no kit revision: $live_link -> $live_target"
live_dir="$runner_root/bin/$live"
live_binary="$live_dir/charliebot"
[[ -x $live_binary ]] || refuse "the live revision $live has no charliebot: $live_binary"
[[ $(readlink -f -- "$live_binary") == "$(readlink -f -- "$live_link")" ]] \
  || refuse "the live revision's charliebot does not resolve to the live link: $live_binary"
"$live_binary" screenshots sizes > /dev/null || refuse "the live charliebot does not run: $live_binary"

live_script="$live_dir/kit/dev/improve/github-post.sh"
[[ -x $live_script ]] || refuse "the live revision $live has no kit script: $live_script"

for fact in inflight-files.txt round-events.ndjson kit-revision; do
  [[ -r $evidence_dir/$fact ]] || refuse "the evidence directory lacks $evidence_dir/$fact"
done
evidence_revision=$(<"$evidence_dir/kit-revision")
[[ $evidence_revision =~ ^[0-9a-f]{40}$ ]] \
  || refuse "$evidence_dir/kit-revision holds no full kit revision"
kit_skills_dir "$evidence_revision" > /dev/null \
  || refuse "the runner checkout holds no skills tree for kit revision $evidence_revision"

args=(pr-merge --evidence-dir "$evidence_dir")
[[ -z $review_record ]] || args+=(--review-record "$review_record")
export CHARLIEBOT_HOME=$round_home
export PATH="$live_dir:$PATH"
exec "$live_script" "${args[@]}" "$pr"

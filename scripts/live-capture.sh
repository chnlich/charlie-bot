#!/usr/bin/env bash
# The live capture stage: capture the live page of each `View:` trailer that a commit in OLD..NEW carries, at every
# window size, then have the `judge` backend check each commit's screenshots. The release switch runs this stage once
# its new release serves, and a rerun runs it against the server that runs now. The stage stops, starts and moves
# nothing, so the server keeps serving while it runs.
#
#   scripts/live-capture.sh [--release DIR] [--name NAME] <old commit> <new commit>
#
# --release names the release directory whose `charliebot` answers the window sizes, the view checks and the judge
# (default: the release `current` names). --name names the directory under CHARLIEBOT_LOG_DIR/live-screenshots that
# holds the captures (default: rerun_<time>). The port and the access key come from the home's config, read through
# `charliebot switch-helper home-helper config`; the captures reach that server on 127.0.0.1.
#
# Each commit fails on its own. A View line the live server does not resolve, a capture that fails (live-shot.mjs
# names the failed page, such as the access-key login page), or a verdict other than `shows` fails that commit with a
# reason. A commit whose captures all pass is judged even when another commit failed. A failed commit gets no verdict.
# The stage prints one line per failed commit with its reason, and exits 7 when any commit failed. It exits 0 when
# every commit with a View line passed, and 2 on a usage error. It exits 1 when it cannot start, such as a missing
# release or config. A capture that fails leaves no PNG; a failed commit's passing sizes stay on disk as captures only.
#
# Each commit's captures and judge files stay in STAGE_DIR/<short sha>/: the PNG of each view at each size, `task.md`,
# `answer.txt`, `judge.stderr` and `events.ndjson`.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${CHARLIEBOT_REPO_ROOT:-$SCRIPT_DIR/..}"
REPO_ROOT="$(cd "$REPO_ROOT" && pwd)"
HOME_DIR="${CHARLIEBOT_HOME:-$HOME/.charliebot}"
RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
LOG_DIR="${CHARLIEBOT_LOG_DIR:-/tmp/charliebot-logs}"

usage() {
  echo "usage: live-capture.sh [--release DIR] [--name NAME] <old commit> <new commit>" >&2
  exit 2
}
log() { echo "$(date '+%Y-%m-%d %H:%M:%S.%3N') live-capture: $*"; }
die() {
  log "cannot start: $*"
  exit 1
}

# The failures, one `<short sha><TAB><reason>` each, in the order they happened, and the short shas that failed.
FAILURES=()
FAILED_SHORTS=""

# fail_commit SHORT REASON: the commit fails with REASON. Its other captures and verdicts stay in place.
fail_commit() {
  FAILURES+=("$1"$'\t'"$2")
  FAILED_SHORTS+=" $1"
}

# commit_failed SHORT: whether a step has failed for the commit.
commit_failed() {
  [[ " $FAILED_SHORTS " == *" $1 "* ]]
}

# commit_view_lines SHA: each non-empty `View:` trailer value of commit SHA, one per line.
commit_view_lines() {
  local message views view
  message="$(git -C "$REPO_ROOT" log -1 --format=%B "$1")" || return 1
  views="$(printf '%s\n' "$message" | sed -n 's/^View:[[:space:]]*//p' | sed 's/[[:space:]]*$//')" || return 1
  while IFS= read -r view; do
    [ -n "$view" ] || continue
    printf '%s\n' "$view"
  done <<< "$views"
}

# commits_with_views OLD NEW: the full sha of each commit in OLD..NEW that carries a View trailer, oldest first.
commits_with_views() {
  local shas sha
  shas="$(git -C "$REPO_ROOT" rev-list --reverse "$1..$2")" || return 1
  for sha in $shas; do
    if [ -n "$(commit_view_lines "$sha")" ]; then
      printf '%s\n' "$sha"
    fi
  done
}

# view_slug VIEW: the view path as a file name, with up to 80 safe characters plus an 8-hex-char SHA-256 suffix of the
# full view string so two views sharing their first 80 characters never overwrite each other's PNGs.
view_slug() {
  local prefix hash
  prefix="$(printf '%s' "$1" | sed 's/[^A-Za-z0-9._-]/_/g' | cut -c1-80)"
  hash="$(printf '%s' "$1" | sha256sum | cut -c1-8)"
  printf '%s-%s' "$prefix" "$hash"
}

# capture_commit SHORT VIEWS: the PNG of each view at each window size, from the server on 127.0.0.1. A view the live
# server does not resolve is refused before any picture of it is taken. A capture that fails leaves no PNG.
capture_commit() {
  local short=$1 views=$2 view resolved size width height dir path err
  dir="$STAGE_DIR/$short"
  mkdir -p "$dir" || die "cannot create $dir"
  rm -f "$dir/task.md" "$dir/answer.txt" "$dir/judge.stderr" "$dir/events.ndjson"
  while IFS= read -r view; do
    [ -n "$view" ] || continue
    if ! resolved="$(CHARLIEBOT_HOME="$HOME_DIR" "$BIN" screenshots resolve "$view" 2>&1)"; then
      resolved="${resolved//$'\n'/ }"
      log "live screenshot: commit $short: $resolved"
      fail_commit "$short" "$resolved"
      continue
    fi
    for size in $SIZES; do
      width=${size%x*}
      height=${size#*x}
      path="$dir/$(view_slug "$view")-${width}x${height}.png"
      rm -f "$path"
      if err="$(CHARLIEBOT_ACCESS_KEY="$KEY" node "$SCRIPT_DIR/live-shot.mjs" \
        "http://127.0.0.1:$PORT" "$view" "$width" "$height" "$path" 2>&1 > /dev/null)"; then
        log "live screenshot: $path commit $short view $view size $width×$height"
      else
        err="${err//$'\n'/ }"
        log "live screenshot: commit $short view $view size $width×$height: ${err:-exit status non-zero}"
        fail_commit "$short" "view $view size $width×$height: ${err:-exit status non-zero}"
      fi
    done
  done <<< "$views"
}

# write_live_view_task PROMPT SHA SHORT DIR VIEWS SIZES: write `task.md` inside DIR for the `judge` call, carrying the
# prompt, commit message, first-parent diff stat and full diff, and each View with its PNG filenames.
write_live_view_task() {
  local prompt=$1 sha=$2 short=$3 dir=$4 views=$5 sizes=$6
  local message diff_stat diff_full view size
  message="$(git -C "$REPO_ROOT" log -1 --format=%B "$sha")" || return 1
  if git -C "$REPO_ROOT" rev-parse --verify -q "${sha}~1" > /dev/null 2>&1; then
    diff_stat="$(git -C "$REPO_ROOT" diff --stat "${sha}~1..${sha}")" || return 1
    diff_full="$(git -C "$REPO_ROOT" diff "${sha}~1..${sha}")" || return 1
  else
    diff_stat="$(git -C "$REPO_ROOT" diff-tree --root --stat --no-commit-id "$sha")" || return 1
    diff_full="$(git -C "$REPO_ROOT" diff-tree --root -p --no-commit-id "$sha")" || return 1
  fi
  {
    printf '%s\n\n' "$prompt"
    printf '## Commit %s\n\n' "$short"
    printf '### Message\n```\n%s\n```\n\n' "$message"
    printf '### Changed paths\n```\n%s\n```\n\n' "$diff_stat"
    printf '### Diff of the commit\n```diff\n%s\n```\n\n' "$diff_full"
    printf '### View lines and screenshots\n'
    while IFS= read -r view; do
      [ -n "$view" ] || continue
      printf -- '- View: %s\n' "$view"
      for size in $sizes; do
        printf '  - %s\n' "$(view_slug "$view")-${size}.png"
      done
    done <<< "$views"
  } > "$dir/task.md" || return 1
}

# check_live_view_verdicts SHORT VIEWS ANSWER_FILE: parse one `VERDICT <view> <shows|not-visible|broken> <reason>` line
# per View (allowing leading text before `VERDICT ` on the same line) and log `live view check: commit <short> view ...`
# for every View. Each View that lacks a verdict, duplicates one, carries an unrecognized word, or reports `not-visible`
# or `broken` fails the commit. It returns 1 when any View failed.
check_live_view_verdicts() {
  local short=$1 views=$2 answer_file=$3
  local parsed_lines=() line payload p_view p_verdict p_reason
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    case "$line" in
      *VERDICT\ *)
        payload="${line#*VERDICT }"
        read -r p_view p_verdict p_reason <<< "$payload"
        parsed_lines+=("$p_view"$'\t'"$p_verdict"$'\t'"$p_reason")
        ;;
    esac
  done < "$answer_file"

  local failed=0 view entry match_count verdict reason
  while IFS= read -r view; do
    [ -n "$view" ] || continue
    match_count=0
    verdict=""
    reason=""
    if [ "${#parsed_lines[@]}" -gt 0 ]; then
      for entry in "${parsed_lines[@]}"; do
        IFS=$'\t' read -r p_view p_verdict p_reason <<< "$entry"
        if [ "$p_view" = "$view" ]; then
          match_count=$((match_count + 1))
          verdict="$p_verdict"
          reason="$p_reason"
        fi
      done
    fi
    if [ "$match_count" -eq 0 ]; then
      log "live view check: commit $short view $view missing verdict"
      fail_commit "$short" "view $view missing verdict"
      failed=1
      continue
    fi
    if [ "$match_count" -gt 1 ]; then
      log "live view check: commit $short view $view multiple verdicts ($match_count)"
      fail_commit "$short" "view $view multiple verdicts ($match_count)"
      failed=1
      continue
    fi
    case "$verdict" in
      shows)
        log "live view check: commit $short view $view shows: ${reason:-no reason given}"
        ;;
      not-visible|broken)
        log "live view check: commit $short view $view $verdict: ${reason:-no reason given}"
        fail_commit "$short" "view $view $verdict: ${reason:-no reason given}"
        failed=1
        ;;
      *)
        log "live view check: commit $short view $view unrecognized verdict '${verdict:-none}':" \
          "${reason:-no reason given}"
        fail_commit "$short" "view $view unrecognized verdict '${verdict:-none}'"
        failed=1
        ;;
    esac
  done <<< "$views"
  [ "$failed" -eq 0 ]
}

# judge_commit SHA SHORT VIEWS: the `judge` backend checks the commit's screenshots, with the caller's run token unset
# so the home's access key authenticates the run. A judge failure or a verdict that fails the commit is recorded.
judge_commit() {
  local sha=$1 short=$2 views=$3 dir err
  dir="$STAGE_DIR/$short"
  if ! write_live_view_task "$PROMPT" "$sha" "$short" "$dir" "$views" "$SIZES"; then
    log "live view check: commit $short: failed to write task file in $dir"
    fail_commit "$short" "cannot write its task file in $dir"
    return
  fi
  if ! (cd "$dir" && unset CHARLIEBOT_RUN_TOKEN \
    && CHARLIEBOT_HOME="$HOME_DIR" "$BIN" backend run --call judge \
    --task-file "$dir/task.md" > "$dir/answer.txt" 2> "$dir/judge.stderr"); then
    err="$(cat "$dir/judge.stderr" "$dir/answer.txt" 2> /dev/null | tr '\n' ' ' \
      | sed 's/[[:space:]]\+/ /g; s/[[:space:]]*$//')"
    log "live view check: commit $short: judge failed: ${err:-exit status non-zero}"
    fail_commit "$short" "judge failed: ${err:-exit status non-zero}"
    return
  fi
  check_live_view_verdicts "$short" "$views" "$dir/answer.txt"
}

# The capture stage's flow. A sourced copy (the switch tests source this file for its functions) stops here.
if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  return 0
fi

RELEASE_DIR=""
NAME=""
while [ $# -gt 0 ]; do
  case "$1" in
    --release) [ $# -ge 2 ] || usage; RELEASE_DIR=$2; shift 2 ;;
    --name) [ $# -ge 2 ] || usage; NAME=$2; shift 2 ;;
    *) break ;;
  esac
done
[ $# -eq 2 ] || usage
[ -z "$NAME" ] || [[ "$NAME" =~ ^[A-Za-z0-9._-]+$ ]] || usage

if [ -z "$RELEASE_DIR" ]; then
  [ -L "$RELEASES_DIR/current" ] || die "no release named current in $RELEASES_DIR"
  RELEASE_DIR="$(readlink -f "$RELEASES_DIR/current")"
fi
BIN="$RELEASE_DIR/charliebot"
[ -x "$BIN" ] || die "no charliebot binary in $RELEASE_DIR"

OLD_SHA="$(git -C "$REPO_ROOT" rev-parse --verify -q "$1^{commit}")" || die "$1 is not a commit in $REPO_ROOT"
NEW_SHA="$(git -C "$REPO_ROOT" rev-parse --verify -q "$2^{commit}")" || die "$2 is not a commit in $REPO_ROOT"

CONFIG="$("$BIN" switch-helper home-helper config "$HOME_DIR")" \
  || die "cannot read $HOME_DIR/config.yaml and credentials.yaml"
PORT="$(sed -n 1p <<< "$CONFIG")"
KEY="$(sed -n 2p <<< "$CONFIG")"
[[ "$PORT" =~ ^[0-9]+$ ]] || die "no port in $HOME_DIR/config.yaml"
[ -n "$KEY" ] || die "no charliebot.access_key in $HOME_DIR/credentials.yaml"
SIZES="$("$BIN" screenshots sizes)" || die "$BIN screenshots sizes failed"
[ -n "$NAME" ] || NAME="rerun_$(date +%Y%m%d_%H%M%S)"
STAGE_DIR="$LOG_DIR/live-screenshots/$NAME"

COMMIT_LIST="$(commits_with_views "$OLD_SHA" "$NEW_SHA")" || die "cannot list the commits of $OLD_SHA..$NEW_SHA"
COMMITS=()
while IFS= read -r sha; do
  [ -n "$sha" ] || continue
  COMMITS+=("$sha")
done <<< "$COMMIT_LIST"
if [ "${#COMMITS[@]}" -eq 0 ]; then
  log "no View trailer in $OLD_SHA..$NEW_SHA; nothing to capture"
  exit 0
fi
PROMPT="$(git -C "$REPO_ROOT" show "$NEW_SHA:prompts/live_view_check.md")" \
  || die "cannot read $NEW_SHA:prompts/live_view_check.md"

for sha in "${COMMITS[@]}"; do
  capture_commit "$(git -C "$REPO_ROOT" rev-parse --short "$sha")" "$(commit_view_lines "$sha")"
done
for sha in "${COMMITS[@]}"; do
  short="$(git -C "$REPO_ROOT" rev-parse --short "$sha")"
  if commit_failed "$short"; then
    log "live view check: commit $short not judged: a capture failed"
    continue
  fi
  judge_commit "$sha" "$short" "$(commit_view_lines "$sha")"
done

if [ "${#FAILURES[@]}" -eq 0 ]; then
  log "live capture stage: every View commit passed (${#COMMITS[@]} commit(s)); captures in $STAGE_DIR"
  exit 0
fi
for entry in "${FAILURES[@]}"; do
  IFS=$'\t' read -r short reason <<< "$entry"
  log "live capture stage: commit $short failed: $reason"
done
failed_count="$(printf '%s\n' $FAILED_SHORTS | sort -u | wc -l)"
log "live capture stage: $failed_count of ${#COMMITS[@]} View commit(s) failed; the others keep their" \
  "captures in $STAGE_DIR"
exit 7

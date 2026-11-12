#!/usr/bin/env bash
# Install the server and CLI binaries that CI built for one commit on main.
#
#   scripts/release.sh <full main commit>
#
# The host release lock: one lock file per host, outside the repository, named once by scripts/release/lock.sh. This
# script takes it before its first stage and holds it to its exit, so two releases of one host run one at a time. A
# caller that finds the lock held exits 2 at once and prints the holder's record — script, commit, pid and start
# time — read from beside the lock; a holder that died left the lock free, because the lock is `flock` on an open
# descriptor. The switch this release hands over to takes the lock in its turn.
#
# Exit codes:
#   2  the preflight refused and wrote nothing: the argument is not a full 40-character sha on origin/main, the
#      release directory already exists, the user manager does not answer, gh is not authenticated for this repository,
#      CI has no successful push run of both Rust jobs for the commit, or another release holds the host release lock.
#   1  a stage failed; the stage name and its log path are printed. A download, units or install failure installs
#      nothing. A prune failure leaves the new release installed, and the prune log names the directory it failed on.
#   0  the release is installed; the switch command is printed.
#
# The stages run in order:
#   download   gh downloads the charliebot-server-<commit> artifact into a temporary directory outside the log
#              directory, sha256sum -c passes on both binaries, and the log directory keeps a copy of the artifact's
#              sha256sums.txt;
#   units      scripts/install-slice-units.sh installs the commit's slice units and reloads the user manager, so the
#              server the switch starts runs against the units the release ships. A release that stops after this
#              stage leaves the new units installed while the live server keeps running; the restore step is in that
#              script's header;
#   install    the release directory is built under a temporary name and renamed to <commit>;
#   prune      keeps the KEPT_VERSIONS most recently installed version directories, the one just installed among
#              them, and the one `current` names; deletes the other version directories and prints each deletion to
#              the prune log. A deleted version re-releases with `scripts/release.sh <commit>` while CI keeps its
#              artifact for 30 days.
#
# The release checks live on the switch, not here: scripts/switch-server.sh runs every check that needs no model
# before it stops the old server, and reads the new server back, probes a turn and checks a delegation once it is
# ready. A release therefore ends when the binaries are installed, and a bad release is proven on the production home
# after the switch, which switches back when the failure is the server's.
#
# The release directory holds charliebot-server and charliebot from the same CI build, release.log with each stage's
# verdict, and sha256sums.txt over both binaries and the log. It does not change the current link.
#
# The log directory holds each stage's text log and a copy of the artifact's sha256sums.txt, and holds no binary: the
# binaries stay in the temporary directory, which every exit of the script removes, a failed stage and an interrupt
# included, and can be re-downloaded from CI for 30 days.
#
# Environment: CHARLIEBOT_RELEASES_DIR (default $HOME/charliebot-releases); CHARLIEBOT_RELEASE_LOG_DIR (default
# $HOME/charliebot-release-logs; stage logs and evidence live here, outside the repository and the release directory,
# and are kept after a failure).
set -uo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
readonly RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
readonly LOG_ROOT="${CHARLIEBOT_RELEASE_LOG_DIR:-$HOME/charliebot-release-logs}"
readonly SLICE_INSTALL="$REPO_ROOT/scripts/install-slice-units.sh"
readonly SYSTEMD_USER="$REPO_ROOT/scripts/systemd-user.sh"
readonly ARTIFACT_NAME_PREFIX="charliebot-server-"
# The version directories the prune keeps, the one just installed included. The `current` target is kept beyond them.
readonly KEPT_VERSIONS=10

# The host release lock, defined once in scripts/release/lock.sh.
. "$SCRIPT_DIR/release/lock.sh"

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

# The lock covers every stage from here to the exit, and the preflight too, so a second release stops before it
# races this one for a trial port.
release_lock_take release.sh "$COMMIT" || exit 2
# cleanup: the one EXIT handler. It removes the lock's holder record on every exit, and the artifact's temporary
# directory once the download stage has made it: success, a refusal, a failed stage and an interrupt.
cleanup() {
  local status=$?
  trap - EXIT INT TERM
  release_lock_release
  if [ -n "${ARTIFACT_DIR:-}" ]; then
    rm -rf -- "$ARTIFACT_DIR"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Check the user manager before downloading or installing anything. The wrapper probes the manager itself.
"$SYSTEMD_USER" --check || refuse "the user manager did not answer"

command -v gh > /dev/null 2>&1 || refuse "gh is not on PATH"
gh auth status > /dev/null 2>&1 || refuse "gh is not authenticated"
REPO_SLUG="$(cd "$REPO_ROOT" && gh repo view --json nameWithOwner --jq .nameWithOwner 2> /dev/null)" \
  || refuse "gh is not authenticated for this repository"

git -C "$REPO_ROOT" rev-parse --verify --quiet origin/main > /dev/null \
  || refuse "origin/main is not in $REPO_ROOT; fetch origin first"
git -C "$REPO_ROOT" merge-base --is-ancestor "$COMMIT" origin/main 2> /dev/null \
  || refuse "$COMMIT is not on origin/main"
[ ! -e "$RELEASES_DIR/$COMMIT" ] && [ ! -L "$RELEASES_DIR/$COMMIT" ] \
  || refuse "release $RELEASES_DIR/$COMMIT already exists"

# ci_run_id: the newest push run of the CI workflow for COMMIT whose release-build and rust-test jobs succeeded. gh
# filters its own JSON; the preflight runs before any release binary is downloaded, and gh is already required.
ci_run_id() {
  local id
  while IFS= read -r id; do
    [ -n "$id" ] || continue
    gh run view "$id" --repo "$REPO_SLUG" --json jobs \
      --jq '([.jobs[] | select(.conclusion == "success") | .name] | (index("release-build") != null) and (index("rust-test") != null))' \
      2> /dev/null | grep -qx true || continue
    printf '%s' "$id"
    return 0
  done < <(gh run list --repo "$REPO_SLUG" --workflow CI --commit "$COMMIT" --limit 20 \
      --json databaseId,event,headSha,conclusion \
      --jq '.[] | select(.event == "push" and .headSha == "'"$COMMIT"'" and .conclusion == "success") | .databaseId' \
      2> /dev/null)
  return 1
}

RUN_ID="$(ci_run_id)" || refuse "CI has no successful push run of release-build and rust-test for $COMMIT"

RUN_DIR="$LOG_ROOT/${COMMIT}-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p -m 700 "$RUN_DIR" || refuse "cannot create the log directory $RUN_DIR"
readonly RUN_DIR
export CHARLIEBOT_RELEASE_RUN_DIR="$RUN_DIR"

# The downloaded artifact lives outside the log directory, and every exit of the script removes it: success, a failed
# stage, and an interrupt.
ARTIFACT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/charliebot-release-artifact-${COMMIT}.XXXXXXXXXX")" \
  || refuse "cannot create the artifact temporary directory"
readonly ARTIFACT_DIR

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
  gh run download "$RUN_ID" --repo "$REPO_SLUG" --name "${ARTIFACT_NAME_PREFIX}${COMMIT}" --dir "$ARTIFACT_DIR" \
    || return 1
  (cd "$ARTIFACT_DIR" && sha256sum -c sha256sums.txt) || return 1
  # The log directory keeps the checksums and no binary: the binaries re-download from CI for 30 days.
  cp "$ARTIFACT_DIR/sha256sums.txt" "$RUN_DIR/sha256sums.txt"
}

stage_units() {
  "$SLICE_INSTALL" "$REPO_ROOT" "$COMMIT"
}

# stage_install: build the release directory under a temporary name, then rename it to the commit.
stage_install() {
  local tmp="$RELEASES_DIR/.$COMMIT.$$" line
  rm -rf "$tmp"
  mkdir -m 700 "$tmp" || return 1
  if ! cp "$ARTIFACT_DIR/charliebot-server" "$ARTIFACT_DIR/charliebot" "$tmp/" \
    || ! chmod 755 "$tmp/charliebot-server" "$tmp/charliebot"; then
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
  if ! (cd "$tmp" && sha256sum charliebot charliebot-server release.log > sha256sums.txt); then
    rm -rf "$tmp"
    return 1
  fi
  if ! mv -T "$tmp" "$RELEASES_DIR/$COMMIT"; then
    rm -rf "$tmp"
    return 1
  fi
}

# version_listing: one "<install time> <commit>" line per version directory, newest install first. A version
# directory is a real directory named by a full commit sha. Its install time is its mtime: stage_install writes the
# directory's last entry before it renames the directory into place, and nothing writes into a release directory
# after that.
version_listing() {
  local dir name time
  for dir in "$RELEASES_DIR"/*; do
    name=${dir##*/}
    [[ $name =~ ^[0-9a-f]{40}$ ]] || continue
    [ -d "$dir" ] && [ ! -L "$dir" ] || continue
    time="$(stat -c '%.9Y' "$dir")" || return 1
    printf '%s %s\n' "$time" "$name"
  done | sort -rn
}

# stage_prune: deletes each version directory beyond the KEPT_VERSIONS newest installs, unless `current` names it. The
# release just installed is kept outright, so its place in the install-time order does not decide whether it stays.
stage_prune() {
  local live="" listing name rank=0
  if [ -L "$RELEASES_DIR/current" ]; then
    live="$(readlink "$RELEASES_DIR/current")" || return 1
    live=${live##*/}
  fi
  listing="$(version_listing)" || return 1
  while IFS=' ' read -r _ name; do
    [ "$name" = "$COMMIT" ] && continue
    rank=$((rank + 1))
    [ "$rank" -lt "$KEPT_VERSIONS" ] && continue
    [ "$name" = "$live" ] && continue
    rm -rf -- "$RELEASES_DIR/$name" || return 1
    echo "release: deleted $RELEASES_DIR/$name"
  done <<< "$listing"
}

run_stage download stage_download
run_stage units stage_units
run_stage install stage_install
run_stage prune stage_prune

echo "release: installed $RELEASES_DIR/$COMMIT"
echo "release: switch with: scripts/switch-server.sh rust $COMMIT"
exit 0

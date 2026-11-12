#!/usr/bin/env bash
# Install one commit's user units into the user manager's unit directory, then reload the user manager.
#
#   scripts/install-slice-units.sh <repo> <commit>
#
# The units are the commit's scripts/systemd/*.slice and *.service, read from the commit object, so the checkout that
# runs this script can sit at another release. The unit directory is CHARLIEBOT_SYSTEMD_USER_DIR, default
# $HOME/.config/systemd/user. This script is the one place that writes user units into that directory:
# scripts/setup.sh, scripts/switch-server.sh and scripts/release.sh call it. The switch calls the copy beside its own
# script, so the call still finds this script after the checkout moves to a commit that predates it.
# This script uses scripts/systemd-user.sh for the user-manager reload. The wrapper owns bus-address selection.
#
# Each installed unit has @REPO_ROOT@ replaced by <repo>. The boot start unit (charliebot-boot.service) names the
# runtime clone that way, so run the install from that clone: the path is derived here, not stored in the repo.
# This script only copies the boot unit; enabling it is a separate step:
#   systemctl --user enable charliebot-boot.service
#
# The reload applies the slice properties to the running agents slice without restarting it. A controller that a unit
# adds (cpu for CPUWeight, io for IOWeight) joins the slice's cgroup.controllers that way.
#
# Restore: a release that stops after this install leaves the commit's units installed while the live server keeps
# running on the live release. Put the live release's units back with the live release's commit and the runtime clone:
#   scripts/install-slice-units.sh <runtime clone> "$(basename "$(readlink -f "$HOME/charliebot-releases/current")")"
# The switch's own restore runs the same call with the previous commit, after it moves the clone back.
#
# Exit: 0 the units are installed and the user manager reloaded; a failed step stops the script with its exit status.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ $# -ne 2 ]; then
  echo "usage: install-slice-units.sh <repo> <commit>" >&2
  exit 2
fi
REPO=$1
COMMIT=$2

UNIT_DIR="${CHARLIEBOT_SYSTEMD_USER_DIR:-$HOME/.config/systemd/user}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
git -C "$REPO" archive --format=tar "$COMMIT" scripts/systemd | tar -x -C "$WORK"
units=("$WORK"/scripts/systemd/*.slice)
shopt -s nullglob
units+=("$WORK"/scripts/systemd/*.service)
shopt -u nullglob
sed -i "s|@REPO_ROOT@|$REPO|g" "${units[@]}"
install -d "$UNIT_DIR"
install -m 0644 "${units[@]}" "$UNIT_DIR/"
echo "install-slice-units: installed ${#units[@]} unit(s) of $COMMIT into $UNIT_DIR; reloading the user manager"
"$SCRIPT_DIR/systemd-user.sh" systemctl daemon-reload

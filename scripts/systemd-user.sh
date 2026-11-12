#!/usr/bin/env bash
# Run systemctl and systemd-run user commands through the user bus selected by XDG_RUNTIME_DIR.
#
#   scripts/systemd-user.sh systemctl <arguments...>
#   scripts/systemd-user.sh systemd-run <arguments...>
#   scripts/systemd-user.sh --check
#
# A caller may inherit DBUS_SESSION_BUS_ADDRESS=disabled:. This wrapper sets the session bus address for release,
# switch and startup commands that use the user manager.
set -euo pipefail

usage() {
  echo "usage: systemd-user.sh systemctl <arguments...> | systemd-run <arguments...> | --check" >&2
  exit 2
}

if ! declare -p XDG_RUNTIME_DIR >/dev/null 2>&1 || [ -z "$XDG_RUNTIME_DIR" ]; then
  echo "systemd-user: XDG_RUNTIME_DIR is unset; cannot select the user manager bus address" >&2
  exit 1
fi
readonly USER_MANAGER_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"

select_user_manager_bus() {
  export DBUS_SESSION_BUS_ADDRESS="$USER_MANAGER_BUS_ADDRESS"
}

if [ "$#" -gt 0 ] && [ "$1" = "--check" ]; then
  [ "$#" -eq 1 ] || usage
  select_user_manager_bus
  if ! systemctl --user show-environment >/dev/null 2>&1; then
    echo "systemd-user: user manager did not answer at DBUS_SESSION_BUS_ADDRESS=$USER_MANAGER_BUS_ADDRESS" >&2
    exit 1
  fi
  exit 0
fi

[ "$#" -ge 2 ] || usage
case "$1" in
  systemctl|systemd-run) executable=$1 ;;
  *) usage ;;
esac
shift
select_user_manager_bus
exec "$executable" --user "$@"

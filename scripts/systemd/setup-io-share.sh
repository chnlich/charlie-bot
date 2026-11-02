#!/usr/bin/env bash
set -euo pipefail

# Gives every CharlieBot agent session its share of the root disk. The kernel only enforces a
# cgroup's io.weight under the io.cost controller, so this script, run once as root, does the three
# things the user manager cannot do for itself:
#   1. delegate io to the user manager, with a drop-in for user@.service and a daemon-reload;
#   2. enable io.cost on the root disk with the kernel's auto cost model, the disk's major:minor
#      derived from the root filesystem at run time;
#   3. install and enable a root oneshot that re-applies io.cost at boot.
# Each write is read back and printed. The script never restarts user@<uid>.service: when io is
# missing from the user manager's cgroup.subtree_control after the reload, it stops and says that a
# user-manager restart is needed (that restart also restarts the CharlieBot server and tmux, and
# only the user starts it).
#
# Run:        sudo scripts/systemd/setup-io-share.sh
# Undo all:   sudo scripts/systemd/setup-io-share.sh --undo
# Print only: sudo scripts/systemd/setup-io-share.sh --dry-run
#
# The undos, in reverse order of the writes (the script runs them with --undo and reads each back):
#   - disable and remove the boot unit: systemctl disable --now charliebot-io-cost.service;
#     rm /etc/systemd/system/charliebot-io-cost.service; rm /usr/local/sbin/charliebot-io-cost;
#     systemctl daemon-reload
#   - disable io.cost: echo "<maj:min> enable=0" > /sys/fs/cgroup/io.cost.qos
#   - remove the delegation drop-in: rm /etc/systemd/system/user@.service.d/charliebot-io-share.conf;
#     systemctl daemon-reload (whether io leaves the running user manager without a restart is open;
#     the undo reports what the readback sees)

DROPIN=/etc/systemd/system/user@.service.d/charliebot-io-share.conf
BOOT_UNIT=/etc/systemd/system/charliebot-io-cost.service
BOOT_HELPER=/usr/local/sbin/charliebot-io-cost

DRY_RUN=0
UNDO=0
usage() {
  echo "Usage: $0 [--dry-run] [--undo]" >&2
}
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --undo) UNDO=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 1 ;;
  esac
done
if [[ $DRY_RUN == 1 && $UNDO == 1 ]]; then
  echo "setup-io-share: --dry-run and --undo do not combine" >&2
  exit 1
fi

# The user manager to delegate io to: the one the CharlieBot server runs in, that of the user who
# invoked the script through sudo.
SERVER_USER=${SUDO_USER:-}
if [[ -z "$SERVER_USER" || "$SERVER_USER" == root ]]; then
  echo "setup-io-share: run with sudo as the user whose manager runs the CharlieBot server (SUDO_USER is unset)" >&2
  exit 1
fi
USER_UID=$(id -u "$SERVER_USER")
USER_SLICE_CGROUP=/sys/fs/cgroup/user.slice/user-${USER_UID}.slice/user@${USER_UID}.service

# A write and its undo, printed in the same words the dry-run and the applied run use.
announce() {
  local undo=$1 write=$2
  echo "write: $write"
  echo "undo:  $undo"
}
apply() {
  if [[ $DRY_RUN == 1 ]]; then
    return 0
  fi
  "$@"
}
readback() {
  if [[ $DRY_RUN == 1 ]]; then
    return 0
  fi
  local what=$1 value=$2
  echo "readback: $what = $value"
}

# The whole-disk major:minor that carries the root filesystem, from sysfs rather than a name rule.
resolve_root_disk() {
  local source dev_sys
  source=$(findmnt -no SOURCE --target /)
  dev_sys=$(readlink -f "/sys/class/block/$(basename "$source")")
  while [[ -e "$dev_sys/partition" ]]; do
    dev_sys=$(dirname "$dev_sys")
  done
  basename "$dev_sys"
  cat "$dev_sys/dev"
}

if [[ $UNDO == 1 ]]; then
  echo "undo 3: the boot unit"
  apply systemctl disable --now charliebot-io-cost.service || true
  apply rm -f "$BOOT_UNIT" "$BOOT_HELPER"
  apply systemctl daemon-reload
  if [[ $DRY_RUN == 0 ]]; then
    readback "charliebot-io-cost.service" "$(systemctl is-enabled charliebot-io-cost.service 2>&1 || true)"
  fi
  echo "undo 2: io.cost"
  DISK_MAJ_MIN=$(resolve_root_disk | tail -1)
  apply sh -c "echo '$DISK_MAJ_MIN enable=0' > /sys/fs/cgroup/io.cost.qos"
  if [[ $DRY_RUN == 0 ]]; then
    readback "io.cost.qos" "$(cat /sys/fs/cgroup/io.cost.qos)"
  fi
  echo "undo 1: the delegation drop-in"
  apply rm -f "$DROPIN"
  apply systemctl daemon-reload
  if [[ $DRY_RUN == 0 ]]; then
    readback "DelegateControllers of user@${USER_UID}.service" \
      "$(systemctl show "user@${USER_UID}.service" -p DelegateControllers --value)"
    readback "user manager cgroup.subtree_control" "$(cat "$USER_SLICE_CGROUP/cgroup.subtree_control")"
  fi
  echo "undo done"
  exit 0
fi

echo "== 1. delegate io to the user manager =="
DISK_NAME=$(resolve_root_disk | head -1)
DISK_MAJ_MIN=$(resolve_root_disk | tail -1)
announce \
  "rm $DROPIN; systemctl daemon-reload" \
  "write $DROPIN with Delegate=pids memory cpu io, then systemctl daemon-reload"
apply mkdir -p "$(dirname "$DROPIN")"
apply tee "$DROPIN" >/dev/null <<'EOF'
# Installed by scripts/systemd/setup-io-share.sh: io reaches the session cgroups only when the user
# manager holds the controller itself.
[Service]
Delegate=pids memory cpu io
EOF
apply systemctl daemon-reload
if [[ $DRY_RUN == 0 ]]; then
  readback "DelegateControllers of user@${USER_UID}.service" \
    "$(systemctl show "user@${USER_UID}.service" -p DelegateControllers --value)"
  SUBTREE=$(cat "$USER_SLICE_CGROUP/cgroup.subtree_control")
  readback "user manager cgroup.subtree_control" "$SUBTREE"
  if [[ " $SUBTREE " != *" io "* ]]; then
    echo "setup-io-share: io is still missing from $USER_SLICE_CGROUP/cgroup.subtree_control." >&2
    echo "setup-io-share: the running user manager takes a new delegate only at a restart." >&2
    echo "setup-io-share: a user-manager restart (systemctl restart user@${USER_UID}.service) also" >&2
    echo "setup-io-share: restarts the CharlieBot server and tmux; only you start it. Nothing below" >&2
    echo "setup-io-share: this line ran; rerun this script after the restart." >&2
    exit 2
  fi
fi

echo "== 2. enable io.cost on $DISK_NAME ($DISK_MAJ_MIN) with the auto cost model =="
announce \
  "echo '$DISK_MAJ_MIN enable=0' > /sys/fs/cgroup/io.cost.qos" \
  "echo '$DISK_MAJ_MIN enable=1 ctrl=auto' > /sys/fs/cgroup/io.cost.qos"
apply sh -c "echo '$DISK_MAJ_MIN enable=1 ctrl=auto' > /sys/fs/cgroup/io.cost.qos"
if [[ $DRY_RUN == 0 ]]; then
  readback "io.cost.qos" "$(cat /sys/fs/cgroup/io.cost.qos)"
fi

echo "== 3. install and enable the boot unit that re-applies io.cost =="
announce \
  "systemctl disable --now charliebot-io-cost.service; rm $BOOT_UNIT $BOOT_HELPER; systemctl daemon-reload" \
  "write $BOOT_HELPER, $BOOT_UNIT, then systemctl daemon-reload and enable --now"
apply tee "$BOOT_HELPER" >/dev/null <<'EOF'
#!/bin/sh
# Installed by scripts/systemd/setup-io-share.sh: re-applies io.cost with the kernel's auto cost
# model on the disk that carries the root filesystem, derived here again at run time.
set -eu
source=$(findmnt -no SOURCE --target /)
dev_sys=$(readlink -f "/sys/class/block/$(basename "$source")")
while [ -e "$dev_sys/partition" ]; do
  dev_sys=$(dirname "$dev_sys")
done
echo "$(cat "$dev_sys/dev") enable=1 ctrl=auto" > /sys/fs/cgroup/io.cost.qos
EOF
apply chmod 755 "$BOOT_HELPER"
apply tee "$BOOT_UNIT" >/dev/null <<'EOF'
[Unit]
Description=Re-apply io.cost on the root disk for the CharlieBot session io shares
After=local-fs.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/charliebot-io-cost
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF
apply systemctl daemon-reload
apply systemctl enable --now charliebot-io-cost.service
if [[ $DRY_RUN == 0 ]]; then
  readback "charliebot-io-cost.service" "$(systemctl is-enabled charliebot-io-cost.service)"
  readback "charliebot-io-cost.service state" "$(systemctl is-active charliebot-io-cost.service)"
  readback "io.cost.qos after the boot unit ran" "$(cat /sys/fs/cgroup/io.cost.qos)"
fi

echo "done: io is delegated, io.cost is on $DISK_NAME, and the boot unit is enabled"
echo "the session io shares bind when the server next writes them (enable_session_io_weight is on by default)"

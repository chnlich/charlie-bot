#!/usr/bin/env bash
# Removes each apt source file that names packages.microsoft.com, so the `apt-get update` after it never contacts
# that host. The GitHub runner image adds that source (microsoft-prod.list on Ubuntu 24.04), and the host answers
# 403 ("repository is no longer signed") when its signing key lapses. The CI jobs that install system tools take
# nothing from that host.
#
# A file is removed whole, so a file that also lists other sources would lose them too; the runner image keeps
# Microsoft's source in a file of its own. Each removed path is printed. With no such file, the script removes
# nothing and succeeds.
set -euo pipefail

shopt -s nullglob
for source in /etc/apt/sources.list.d/*; do
  if grep -q 'packages\.microsoft\.com' "$source"; then
    echo "Removing apt source: $source"
    sudo rm -- "$source"
  fi
done

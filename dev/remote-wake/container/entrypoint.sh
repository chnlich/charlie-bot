#!/bin/sh
# Start sshd in the foreground and put a timestamp on every log line, because the bench attributes the container's
# authenticated connections to a wake by their time. sshd's file log carries no timestamp; its stderr log does not
# either, so this script prefixes each line with the epoch second it was read.
#
# The container is recreated per run, so the host keys are generated on the first start and nothing is pinned.
set -eu

mkdir -p /run/sshd /var/log
ssh-keygen -A >/dev/null 2>&1

/usr/sbin/sshd -D -e 2>&1 | while IFS= read -r line; do
  printf '%s %s\n' "$(date -u +%s.%N)" "$line"
done >>/var/log/bench-sshd.log

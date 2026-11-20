#!/bin/sh
set -eu
for argument in "$@"; do
  case "$argument" in
    --print=*) printf '%s' "${argument#--print=}" > received_prompt ;;
  esac
done
printf '%s' $$ > started
while [ ! -e release ]; do sleep 0.01; done
cat reply.json
printf warned >&2

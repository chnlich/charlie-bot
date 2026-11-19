#!/bin/sh
set -eu
cat > received_prompt
printf '%s' $$ > started
while [ ! -e release ]; do sleep 0.01; done
cat reply.json
printf warned >&2

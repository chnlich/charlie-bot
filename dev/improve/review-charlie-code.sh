#!/usr/bin/env bash
# The review's charlie-code on an improve host. remote-runner.sh installs this file as charlie-code in its bin
# directory, which the round's PATH lists before the host's own charlie-code. The runner also sources it for
# credential_key.
#
# No round's environment holds the review key. This script reads the key from the round's credentials file under the
# review credential's name, sets it with the review option's model and API base for this one process, and runs the
# host's charlie-code with the arguments it was given. exec keeps the process named charlie-code, which the runner's
# review tracking matches.

# The api_key under one top-level section of a credentials file, found by the section's name. Prints nothing when the
# section holds no api_key.
credential_key() {
  awk -v section="$2" '
    /^[^ ]/ { inside = ($0 == section ":") }
    inside && /^  api_key:/ { value = $0; sub(/^  api_key:[ \t]*/, "", value); print value; exit }
  ' "$1"
}

# A sourced copy stops here, after credential_key is defined. The installed copy runs as the review's charlie-code.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  set -euo pipefail
  key=$(credential_key "$CHARLIEBOT_HOME/credentials.yaml" "$IMPROVE_REVIEW_CREDENTIAL")
  if [[ -z $key ]]; then
    printf 'charlie-code: no api_key for the review credential %s in %s/credentials.yaml\n' \
      "$IMPROVE_REVIEW_CREDENTIAL" "$CHARLIEBOT_HOME" >&2
    exit 1
  fi
  export CHARLIE_CODE_API_KEY=$key
  export CHARLIE_CODE_MODEL="openai/$IMPROVE_REVIEW_MODEL"
  export CHARLIE_CODE_API_BASE="$IMPROVE_REVIEW_API_BASE/v1"
  exec "$HOME/.local/bin/charlie-code" "$@"
fi

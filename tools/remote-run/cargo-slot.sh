#!/usr/bin/env bash
# Run a Cargo command on the build host through run.sh, from the directory to run in.
#
#   cargo-slot.sh <cargo args...>
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd -P)
exec "$SCRIPT_DIR/run.sh" cargo "$@"

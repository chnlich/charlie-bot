#!/usr/bin/env bash
# Prints a fresh trial root for one side of one invocation, under TMPDIR. run.sh passes it to trial.sh as --tree-root.
# The name is a mktemp template nine bytes beyond TMPDIR. trial.sh puts Chrome's singleton socket 68 bytes below the
# root, and its preflight refuses a socket path of 108 bytes or more, so the name keeps the path under the limit for a
# TMPDIR of up to 25 bytes. A root named for the run id and the side gave 119 bytes on host3.
set -euo pipefail
mktemp -d "${TMPDIR:-/tmp}/pn-XXXXXX"

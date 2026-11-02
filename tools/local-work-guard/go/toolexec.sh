#!/usr/bin/env bash
# The -toolexec program of the GOFLAGS that the local work guard sets. go runs each tool through it as
# `toolexec <tool> <args>`. A compile or a link is refused; the other tools run as they are.
set -euo pipefail

here=$(cd -- "$(dirname -- "$0")" && pwd)
case $(basename -- "$1") in
  compile|link) exec bash "$here/../refuse.sh" go ;;
esac
exec "$@"

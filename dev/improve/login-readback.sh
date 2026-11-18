#!/usr/bin/env bash
# Check that a member kind's login works on the improve host, for the manager's release steps.
#
#   IMPROVE_REMOTE_HOST=<host> dev/improve/login-readback.sh <claude|codex>
#
# claude: the host runs one turn with its login file exported as CLAUDE_CODE_OAUTH_TOKEN, in the same environment as a
# round. The check passes only when the reply is OK.
# codex: the check passes when the host's Codex home holds a non-empty auth.json. The home is the value that
# dev/improve/remote-runner.sh names in CODEX_HOME_DIR, so the two cannot diverge.
#
# The token is read on the host and never reaches this host or a command line. The script prints PASS or FAIL and
# no token and no reply.
#
# Exit codes: 0 the login answers; 1 it does not, or the host cannot be reached; 2 a usage or configuration error.
set -euo pipefail

readonly STDERR_PREFIX='improve login readback:'
readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SSH=(ssh -T -n -o BatchMode=yes -o ConnectTimeout=10)

fail() {
  local code=$1
  shift
  printf '%s %s\n' "$STDERR_PREFIX" "$*" >&2
  exit "$code"
}

kind=${1-}
[[ $kind == claude || $kind == codex ]] || fail 2 'usage: login-readback.sh <claude|codex>'
[[ -n ${IMPROVE_REMOTE_HOST:-} ]] || fail 2 'IMPROVE_REMOTE_HOST is not set; it names the improve host in host config'

case $kind in
  claude)
    remote='export PATH="$HOME/.local/bin:$PATH" CLAUDE_CONFIG_DIR="$HOME/.claude-improve"
export CLAUDE_CODE_OAUTH_TOKEN=$(<"$HOME/improve/logins/claude-oauth-token")
reply=$(claude -p --max-turns 1 "Reply with the single word OK." </dev/null) && [[ $reply == OK ]]'
    ;;
  codex)
    codex_home=$(sed -n 's|^readonly CODEX_HOME_DIR="\$HOME_DIR/\(.*\)"$|\1|p' "$SCRIPT_DIR/remote-runner.sh")
    [[ -n $codex_home ]] || fail 2 'dev/improve/remote-runner.sh names no CODEX_HOME_DIR under $HOME_DIR'
    remote="test -s \"\$HOME/$codex_home/auth.json\""
    ;;
esac

if "${SSH[@]}" "$IMPROVE_REMOTE_HOST" "$remote" >/dev/null 2>&1; then
  printf 'PASS %s: the improve host'\''s login answers\n' "$kind"
else
  printf 'FAIL %s: the improve host'\''s login does not answer, or the host cannot be reached\n' "$kind" >&2
  exit 1
fi

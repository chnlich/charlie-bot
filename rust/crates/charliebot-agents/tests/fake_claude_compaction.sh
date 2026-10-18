#!/bin/sh
set -eu
printf '%s\n' "$@" > "$ARG_LOG"
cat > "$PROMPT_LOG"
case "$FAKE_MODE" in
  failure)
    printf '%s\n' '{"is_error":true,"result":"synthetic compaction failure"}'
    exit 7
    ;;
  timeout)
    sleep 5
    exit 0
    ;;
  success)
    printf '%s\n' '{"type":"system","subtype":"compact_boundary"}' >> "$TRANSCRIPT"
    printf '%s\n' '{"is_error":false,"modelUsage":{"claude-sonnet-5":{"inputTokens":1}}}'
    ;;
  *)
    exit 9
    ;;
esac

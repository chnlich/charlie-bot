#!/bin/sh
# A stand-in for `devin --print`. The test links it as `devin` in its own directory, which this script reads its staged
# files from: `export.json` is copied to the `--export=` path, `answer.txt` is printed on stdout, `stderr.txt` on
# stderr, and `exit_code` names the exit status. Its argv goes to `argv.txt` in the same directory.
set -eu
dir=$(dirname "$0")
printf '%s\n' "$@" >"$dir/argv.txt"
export_path=
for argument in "$@"; do
  case "$argument" in
    --export=*) export_path=${argument#--export=} ;;
  esac
done
if [ -f "$dir/export.json" ]; then
  cp "$dir/export.json" "$export_path"
fi
if [ -f "$dir/stderr.txt" ]; then
  cat "$dir/stderr.txt" >&2
fi
if [ -f "$dir/answer.txt" ]; then
  cat "$dir/answer.txt"
fi
code=0
if [ -f "$dir/exit_code" ]; then
  code=$(cat "$dir/exit_code")
fi
exit "$code"

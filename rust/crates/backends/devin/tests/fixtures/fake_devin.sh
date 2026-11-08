#!/bin/sh
# A stand-in for `devin --print`. The test links it as `devin` in its own directory. Staged `responses/<call>/` files
# provide each call's output; without that directory, the root-level files provide the response as before. Each call's
# argv is written to `call-<n>-argv.txt` and the latest argv remains in `argv.txt`.
set -eu
dir=$(dirname "$0")
call_count_file="$dir/call-count"
call_number=1
if [ -f "$call_count_file" ]; then
  call_number=$(cat "$call_count_file")
  call_number=$((call_number + 1))
fi
printf '%s\n' "$call_number" >"$call_count_file"
printf '%s\n' "$@" >"$dir/argv.txt"
printf '%s\n' "$@" >"$dir/call-$call_number-argv.txt"
response_dir=$dir
if [ -d "$dir/responses" ]; then
  response_dir="$dir/responses/$call_number"
  if [ ! -d "$response_dir" ]; then
    printf 'no staged devin response for call %s\n' "$call_number" >&2
    exit 1
  fi
fi
export_path=
for argument in "$@"; do
  case "$argument" in
    --export=*) export_path=${argument#--export=} ;;
  esac
done
if [ -f "$response_dir/export.json" ]; then
  cp "$response_dir/export.json" "$export_path"
fi
if [ -f "$response_dir/stderr.txt" ]; then
  cat "$response_dir/stderr.txt" >&2
fi
if [ -f "$response_dir/answer.txt" ]; then
  cat "$response_dir/answer.txt"
fi
code=0
if [ -f "$response_dir/exit_code" ]; then
  code=$(cat "$response_dir/exit_code")
fi
exit "$code"

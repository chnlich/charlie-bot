#!/usr/bin/env bash
# gh: the improve round's GitHub CLI shim. The runner installs it beside the round's copies of
# check-skills-host-leak.sh and leak-patterns.sh, in the tool directory of the round's commit, which the round's PATH puts
# ahead of the real gh. Every gh call of the round passes the credential check before gh runs.
#
# The check reads the call's text from three places: each argument; the content of each file argument (--body-file,
# --input, --notes-file, -F and the key=@file forms); and stdin, when an argument names it as `-`. A hit stops the call
# with the kind named and nothing sent. A pass runs the real gh, the first gh on PATH outside this directory.
#
# Exit codes: gh's own exit code after a pass; 2 a setup refusal (nothing sent); 3 a credential hit (nothing sent).
set -euo pipefail

self_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
scanner="$self_dir/check-skills-host-leak.sh"

real_gh=''
IFS=: read -r -a path_entries <<< "$PATH"
for entry in "${path_entries[@]}"; do
  entry=${entry:-.}
  dir=$(cd -- "$entry" 2>/dev/null && pwd -P) || continue
  [[ $dir != "$self_dir" && -f $dir/gh && -x $dir/gh ]] || continue
  real_gh=$dir/gh
  break
done
[[ -n $real_gh ]] || { printf 'gh shim: no gh on PATH outside %s\n' "$self_dir" >&2; exit 2; }

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
"$scanner" --credential-values > "$tmp_dir/values" || exit 2

# check_file <field> <file>: exit 3 on a credential hit, exit 2 on a setup refusal.
check_file() {
  local field=$1 file=$2 status=0
  "$scanner" --credential-check "$tmp_dir/values" "$field" < "$file" || status=$?
  case "$status" in
    0) ;;
    3)
      printf 'gh shim: credential check hit; nothing was sent to GitHub. The round stops here: the kind named above is the reason.\n' >&2
      exit 3
      ;;
    *) exit 2 ;;
  esac
}

# at_file_path <argument>: the file that a `@file` value names, in a bare `@file`, a `key=@file` form at any `=`, or an
# attached `-Fkey=@file` or `--field=key=@file`. Prints nothing and fails when the argument names no file.
at_file_path() {
  local arg=$1 rest=$1
  if [[ $arg == -F?* ]]; then
    rest=${arg#-F}
  elif [[ $arg == --field=* ]]; then
    rest=${arg#--field=}
  fi
  while [[ $rest == *=* ]]; do
    rest=${rest#*=}
    if [[ $rest == @* ]]; then
      printf '%s' "${rest#@}"
      return 0
    fi
  done
  if [[ $rest == @* ]]; then
    printf '%s' "${rest#@}"
    return 0
  fi
  return 1
}

stdin_file=''
expect=''   # file: the next argument names a file; field: the next argument is a -F field
index=0
for arg in "$@"; do
  index=$((index + 1))
  printf '%s' "$arg" > "$tmp_dir/argument"
  check_file "gh argument $index" "$tmp_dir/argument"

  path=''
  case "$expect" in
    file) path=$arg ;;
    field)
      if [[ $arg == *=* || $arg == @* ]]; then
        path=$(at_file_path "$arg") || path=''
      else
        path=$arg
      fi
      ;;
  esac
  next_expect=''
  case "$arg" in
    --body-file|--input|--notes-file) next_expect=file ;;
    -F|--field) next_expect=field ;;
    --body-file=*|--input=*|--notes-file=*) path=${arg#*=} ;;
    -F?*|--field=*) path=$(at_file_path "$arg") || path='' ;;
  esac
  expect=$next_expect

  if [[ -n $path ]]; then
    if [[ $path == - ]]; then
      if [[ -z $stdin_file ]]; then
        stdin_file=$tmp_dir/stdin
        cat > "$stdin_file"
        check_file 'stdin' "$stdin_file"
      fi
    else
      [[ -r $path ]] || { printf 'gh shim: cannot read the file argument %s: %s\n' "$index" "$path" >&2; exit 2; }
      check_file "file $path" "$path"
    fi
  fi
done

status=0
if [[ -n $stdin_file ]]; then
  "$real_gh" "$@" < "$stdin_file" || status=$?
else
  "$real_gh" "$@" || status=$?
fi
exit "$status"

#!/usr/bin/env bash
# The rule citations of a code-style pull request body: the one module that reads them.
#
# The rules of a code-style round are Google's style guide for the language of the files it changes. The kit pins the
# guide text in google-styleguide/ beside this file, and that directory's REVISION file names the copied revision. The
# body names each rule it applies on its own line, in the form that dev/improve/code_style.md states:
#   Rule: google-styleguide/<guide file>:<first line>[-<last line>] "<the exact words of those lines>"
# A line that starts with `Rule:`, or with a bullet and then `Rule:`, must hold that form. Any other such line is
# refused, not skipped, so a malformed citation cannot pass as a missing one.
#
# github-post.sh pr-create calls code_style_check_body for the code-style direction only.
#
# This file defines functions only. A caller sources it.

# The pinned guide text beside this file, which the round reads and cites.
code_style_guide_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/google-styleguide"

# The guide files of this direction. A new language takes its guide here, in dev/improve/code_style.md, and in the
# directory, in one kit change.
code_style_guides='jsguide.md htmlcssguide.md shellguide.md'

# code_style_normalize <text>: print the text with each run of whitespace collapsed to one space and both ends
# trimmed, so a quote and its cited lines compare as words.
code_style_normalize() {
  local text=$1
  text=$(printf '%s' "$text" | tr -s '[:space:]' ' ')
  text=${text# }
  text=${text% }
  printf '%s' "$text"
}

# code_style_check_body <body file>: pass when the body names at least one rule and each rule line quotes its cited
# lines of the pinned guide exactly. A refusal names each line it refuses on stderr and returns 1.
code_style_check_body() {
  local body=$1 line guide
  local rule_re='^Rule: google-styleguide/([A-Za-z0-9_-]+\.md):([1-9][0-9]*)(-([1-9][0-9]*))? "(.*)"$'
  local detect_re='^[[:space:]]*(([-*+][[:space:]]+|[0-9]+[.)][[:space:]]+))?Rule:'
  local guide_dir=$code_style_guide_dir
  local guide_file first last quote text location i
  local -a file_lines=()
  local count=0 status=0
  [[ -d $guide_dir ]] || {
    printf 'code-style rule check: the pinned guide directory is missing: %s\n' "$guide_dir" >&2
    return 1
  }
  while IFS= read -r line || [[ -n $line ]]; do
    [[ $line =~ $detect_re ]] || continue
    count=$((count + 1))
    if [[ ! $line =~ $rule_re ]]; then
      printf 'code-style rule check: "%s" is not the rule line form, %s\n' "$line" \
        'Rule: google-styleguide/<guide file>:<first line>[-<last line>] "<those lines>"' >&2
      status=1
      continue
    fi
    guide=${BASH_REMATCH[1]}
    first=${BASH_REMATCH[2]}
    last=${BASH_REMATCH[4]:-}
    quote=${BASH_REMATCH[5]}
    [[ -n $last ]] || last=$first
    location="google-styleguide/$guide:$first"
    (( last == first )) || location+="-$last"
    if [[ " $code_style_guides " != *" $guide "* ]]; then
      printf 'code-style rule check: %s is not a guide of this direction; its guides are: %s\n' \
        "$guide" "$code_style_guides" >&2
      status=1
      continue
    fi
    guide_file=$guide_dir/$guide
    if [[ ! -r $guide_file ]]; then
      printf 'code-style rule check: the pinned guide file is not readable: %s\n' "$guide_file" >&2
      status=1
      continue
    fi
    mapfile -t file_lines <"$guide_file"
    if (( first > last )); then
      printf 'code-style rule check: %s ends before it starts\n' "$location" >&2
      status=1
      continue
    fi
    if (( last > ${#file_lines[@]} )); then
      printf 'code-style rule check: %s is past the end of the file, which has %s lines\n' \
        "$location" "${#file_lines[@]}" >&2
      status=1
      continue
    fi
    text=''
    for ((i = first - 1; i < last; i++)); do
      text+="${file_lines[i]} "
    done
    text=$(code_style_normalize "$text")
    quote=$(code_style_normalize "$quote")
    if [[ -z $text ]]; then
      printf 'code-style rule check: %s is blank, so it holds no rule\n' "$location" >&2
      status=1
      continue
    fi
    if [[ $quote != "$text" ]]; then
      printf 'code-style rule check: %s reads "%s", but the body quotes "%s"\n' \
        "$location" "$text" "$quote" >&2
      status=1
    fi
  done <"$body"
  if (( count == 0 )); then
    printf 'code-style rule check: the body names no rule; a code-style body needs one line per rule it applies, %s\n' \
      'in the form Rule: google-styleguide/<guide file>:<line> "<those lines>"' >&2
    return 1
  fi
  return "$status"
}

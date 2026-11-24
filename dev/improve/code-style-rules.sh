#!/usr/bin/env bash
# The rule citations of a code-style pull request body: the one module that reads them.
#
# The rules of a code-style round are the lines of writing-style/genres/code.md under $IMPROVE_SKILLS_DIR. The body
# names each rule it applies on its own line, in the form that dev/improve/code_style.md states:
#   Rule: writing-style/genres/code.md:<line> "<that line of the file, without its surrounding whitespace>"
# A line that starts with `Rule:`, or with a bullet and then `Rule:`, must hold that form. Any other such line is
# refused, not skipped, so a malformed citation cannot pass as a missing one.
#
# github-post.sh pr-create calls code_style_check_body for the code-style direction only.
#
# This file defines functions only. A caller sources it.

# code_style_check_body <skills dir> <body file>: pass when the body names at least one rule and each rule line quotes
# its cited line of the rule file exactly. A refusal names each line it refuses on stderr and returns 1.
code_style_check_body() {
  local skills=$1 body=$2 line rule_file
  local rule_re='^Rule: writing-style/genres/code\.md:([1-9][0-9]*) "(.+)"$'
  local detect_re='^[[:space:]]*(-[[:space:]]+)?Rule:'
  local -a file_lines=()
  local count=0 status=0 n quote cited
  [[ -n $skills ]] || {
    printf 'code-style rule check: IMPROVE_SKILLS_DIR is unset, so the rule file is unknown; nothing was sent.\n' >&2
    return 1
  }
  rule_file="$skills/writing-style/genres/code.md"
  [[ -r $rule_file ]] || { printf 'code-style rule check: the rule file is not readable: %s\n' "$rule_file" >&2; return 1; }
  mapfile -t file_lines <"$rule_file"
  while IFS= read -r line; do
    [[ $line =~ $detect_re ]] || continue
    count=$((count + 1))
    if [[ ! $line =~ $rule_re ]]; then
      printf 'code-style rule check: "%s" is not the rule line form, %s\n' "$line" \
        'Rule: writing-style/genres/code.md:<line> "<that line of the file>"' >&2
      status=1
      continue
    fi
    n=${BASH_REMATCH[1]}
    quote=${BASH_REMATCH[2]}
    if (( n > ${#file_lines[@]} )); then
      printf 'code-style rule check: writing-style/genres/code.md:%s is past the end of the file, which has %s lines\n' \
        "$n" "${#file_lines[@]}" >&2
      status=1
      continue
    fi
    cited=${file_lines[n - 1]}
    cited=${cited#"${cited%%[![:space:]]*}"}
    cited=${cited%"${cited##*[![:space:]]}"}
    if [[ $quote != "$cited" ]]; then
      printf 'code-style rule check: writing-style/genres/code.md:%s is "%s", but the body quotes "%s"\n' \
        "$n" "$cited" "$quote" >&2
      status=1
    fi
  done <"$body"
  if (( count == 0 )); then
    printf 'code-style rule check: the body names no rule; a code-style body needs one line per rule it applies, %s\n' \
      'in the form Rule: writing-style/genres/code.md:<line> "<that line of the file>"' >&2
    return 1
  fi
  return "$status"
}

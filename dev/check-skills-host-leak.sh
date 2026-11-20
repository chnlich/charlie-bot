#!/usr/bin/env bash
# The one leak scanner of the repository and of the improve round. Its modes:
#
#   (no argument)                       scan the tracked files, or the files named, with the repository rules
#   --stdin                             scan stdin with the repository rules and the host patterns
#   --credential-values                 print the credential values this process holds, one `kind<TAB>value` per line
#   --credential-check <values> <field> scan stdin for the exact value of any credential in the values file
#
# Exit codes: 0 clean, 1 a pattern hit, 2 a setup refusal (nothing was checked), 3 a credential hit.
# A credential hit prints the kind and the field; it never prints the value or the matched text.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
tmp_patterns="$tmp_dir/patterns"
: > "$tmp_patterns"
source "$script_dir/leak-patterns.sh"

built_in_regex='[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}|/data/(checkpoints|datasets|home)/|/home/[^/[:space:]]+/|\.ts\.net\b|[a-z][a-z0-9-]{2,}\.slack\.com'

# A credential value shorter than this is left out of the check. The shortest credential this profile home holds is 25
# characters; values of 11 characters and fewer are region names, booleans, ports and short user names, which ordinary
# text carries, so an exact match on them would refuse text that holds no credential.
readonly CREDENTIAL_MIN_LENGTH=12

# The environment variables that hold a login token or an API key of the run. The round's environment holds the
# member's login under the first three names; the rest are the review key and the GitHub tokens that `gh` reads.
readonly CREDENTIAL_ENV_NAMES=(
  ANTHROPIC_AUTH_TOKEN
  CLAUDE_CODE_OAUTH_TOKEN
  CHARLIEBOT_RUN_TOKEN
  CHARLIE_CODE_API_KEY
  GH_TOKEN
  GITHUB_TOKEN
)

# The leaves of a credentials file: each `section.key` of a two-space leaf and each top-level scalar. The file is read
# as the subset the profile home writes (section lines, two-space `key: value` leaves, comments). Any other line stops the
# read with exit 2, so a value this reader cannot see never passes unchecked. The message names the line, never its text.
credentials_yaml_leaves() {  # <credentials file>
  LC_ALL=C awk -v min="$CREDENTIAL_MIN_LENGTH" -v file="$1" '
    function fail(message) {
      printf "%s line %d: %s\n", file, NR, message > "/dev/stderr"
      exit 2
    }
    function leaf(path, rest,   value) {
      if (substr(rest, 1, 1) == "\"") {
        if (rest !~ /^"[^"\\]*"[ \t]*(#.*)?$/) fail("a double-quoted value this reader cannot read")
        value = substr(rest, 2)
        sub(/".*$/, "", value)
      } else if (substr(rest, 1, 1) == "'\''") {
        if (rest !~ /^'\''[^'\'']*'\''[ \t]*(#.*)?$/) fail("a single-quoted value this reader cannot read")
        value = substr(rest, 2)
        sub(/'\''.*$/, "", value)
      } else {
        if (rest ~ /^[][{}&*!|>%@`]/) fail("a value this reader cannot read")
        value = rest
        sub(/[ \t]+#.*$/, "", value)
        sub(/[ \t]+$/, "", value)
      }
      if (length(value) >= min) printf "credentials.yaml leaf %s\t%s\n", path, value
    }
    { sub(/\r$/, "") }
    /^[ \t]*(#.*)?$/ { next }
    /^[A-Za-z0-9_.-]+:/ {
      key = $0
      sub(/:.*$/, "", key)
      rest = substr($0, length(key) + 2)
      sub(/^[ \t]+/, "", rest)
      if (rest == "" || rest ~ /^#/) {
        section = key
      } else {
        section = ""
        leaf(key, rest)
      }
      next
    }
    /^  [A-Za-z0-9_.-]+:/ {
      if (section == "") fail("a leaf outside a section")
      key = substr($0, 3)
      sub(/:.*$/, "", key)
      rest = substr($0, length(key) + 4)
      sub(/^[ \t]+/, "", rest)
      if (rest == "" || rest ~ /^#/) next
      leaf(section "." key, rest)
      next
    }
    { fail("a line this reader does not parse") }
  ' "$1"
}

# The string values that a jq filter selects from a JSON credentials file, each as `kind<TAB>value`. A value that spans
# lines fails the filter, and a file that does not parse fails the read; both stop with exit 2.
json_credential_values() {  # <kind> <file> <jq filter>
  local kind=$1 file=$2 filter=$3 values line
  values=$(jq -r "$filter" "$file") || {
    printf '%s cannot be read as JSON, or holds a value that spans lines: %s\n' "$kind" "$file" >&2
    return 2
  }
  while IFS= read -r line; do
    printf '%s\t%s\n' "$kind" "$line"
  done <<< "$values"
}

# Every credential value of this process, as `kind<TAB>value`, shorter values left out. The sources are the profile
# home's credentials file, the environment names above, the Claude credentials file of the config directory in use and
# the Codex auth.json of CODEX_HOME. A source that does not exist contributes nothing; a source that exists and cannot be
# read stops the call with exit 2 before anything is printed.
credential_values() {
  local profile_home=${CHARLIEBOT_HOME:-$HOME/.charliebot}
  local claude_file=${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.credentials.json
  local codex_file=${CODEX_HOME:-$HOME/.codex}/auth.json
  local all="$tmp_dir/all-values" name
  profile_home=${profile_home/#\~/$HOME}
  : > "$all"

  if [[ -e $profile_home/credentials.yaml || -L $profile_home/credentials.yaml ]]; then
    credentials_yaml_leaves "$profile_home/credentials.yaml" >> "$all" || return 2
  fi
  for name in "${CREDENTIAL_ENV_NAMES[@]}"; do
    [[ -n ${!name-} ]] || continue
    if [[ ${!name} == *$'\n'* ]]; then
      printf 'the environment variable %s holds a value that spans lines\n' "$name" >&2
      return 2
    fi
    printf 'environment variable %s\t%s\n' "$name" "${!name}" >> "$all"
  done
  if [[ -e $claude_file || -L $claude_file ]]; then
    json_credential_values 'Claude credentials file' "$claude_file" \
      '[paths(type == "string") as $p | select(($p[-1] | tostring) | test("token$"; "i")) | getpath($p)]
       | if any(contains("\n")) then error("a value spans lines") else .[] end' >> "$all" || return 2
  fi
  if [[ -e $codex_file || -L $codex_file ]]; then
    json_credential_values 'Codex auth.json' "$codex_file" \
      '[((.tokens // {}) | to_entries[] | select(.key | test("token$"; "i")) | .value | strings),
        (.OPENAI_API_KEY | strings)]
       | if any(contains("\n")) then error("a value spans lines") else .[] end' >> "$all" || return 2
  fi
  LC_ALL=C awk -v min="$CREDENTIAL_MIN_LENGTH" '
    { value = substr($0, index($0, "\t") + 1); if (length(value) >= min) print }
  ' "$all"
}

# Scan stdin for the exact value of any credential in the values file. The values come from the file, never from an
# argument, and the scan matches them as fixed strings. The first hit prints the kind and the field, then exits 3.
credential_check() {  # <values file> <field>
  local values=$1 field=$2 status=0
  [[ -r $values ]] || {
    printf 'cannot read the credential values file: %s\n' "$values" >&2
    return 2
  }
  LC_ALL=C awk -v values="$values" -v field="$field" '
    BEGIN {
      count = 0
      while ((getline line < values) > 0) {
        tab = index(line, "\t")
        kinds[++count] = substr(line, 1, tab - 1)
        secrets[count] = substr(line, tab + 1)
      }
    }
    {
      for (i = 1; i <= count; i++) {
        if (index($0, secrets[i]) > 0) {
          printf "Credential hit in %s: %s\n", field, kinds[i]
          exit 3
        }
      }
    }
  ' || status=$?
  return "$status"
}

case "${1:-}" in
  --credential-values)
    [[ $# -eq 1 ]] || { printf 'usage: --credential-values takes no argument\n' >&2; exit 2; }
    credential_values
    exit
    ;;
  --credential-check)
    [[ $# -eq 3 ]] || { printf 'usage: --credential-check <values file> <field>, with the text on stdin\n' >&2; exit 2; }
    credential_check "$2" "$3"
    exit
    ;;
esac

# Stdin mode scans one text with the full rules: built-in rules plus host patterns,
# with no path test, so text cannot slip past the check by suffix or location.
# It exits 1 on a hit, 0 on clean text, and 2 when the pattern file is unreadable.
if [[ "${1:-}" == '--stdin' ]]; then
  load_host_patterns
  cat > "$tmp_dir/content"
  matches=0
  if grep -aiEn "$built_in_regex" "$tmp_dir/content" > "$tmp_dir/matches"; then
    printf 'Built-in pattern matches in stdin:\n'
    cat "$tmp_dir/matches"
    matches=$((matches + 1))
  else
    status=$?
    [[ "$status" -eq 1 ]] || exit "$status"
  fi
  if grep -aiEn '[a-z0-9]{6,}\.feishu\.cn' "$tmp_dir/content" > "$tmp_dir/candidates"; then
    if grep -vE 'open\.feishu\.cn|<tenant>\.feishu\.cn|xxx\.feishu\.cn' "$tmp_dir/candidates" > "$tmp_dir/matches"; then
      printf 'Built-in pattern matches in stdin:\n'
      cat "$tmp_dir/matches"
      matches=$((matches + 1))
    else
      status=$?
      [[ "$status" -eq 1 ]] || exit "$status"
    fi
  else
    status=$?
    [[ "$status" -eq 1 ]] || exit "$status"
  fi
  if [[ -s "$tmp_patterns" ]]; then
    if grep -aiEn -f "$tmp_patterns" "$tmp_dir/content" > "$tmp_dir/matches"; then
      printf 'Host pattern matches in stdin (patterns: %s)\n' "$extra_patterns_file"
      cat "$tmp_dir/matches"
      matches=$((matches + 1))
    else
      status=$?
      [[ "$status" -eq 1 ]] || exit "$status"
    fi
  fi
  printf 'Summary: %d matches in stdin\n' "$matches"
  [[ "$matches" -eq 0 ]]
  exit
fi

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
load_host_patterns

manifest_scan=()
shopt -s nullglob
for manifest in web/features/*/leak_scan_files.txt; do
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" ]] || manifest_scan+=("$line")
  done < "$manifest"
done
shopt -u nullglob

host_files=()
builtin_files=()
if [[ $# -eq 0 ]]; then
  while IFS= read -r -d '' file; do host_files+=("$file"); done < <(git ls-files -z)
  while IFS= read -r -d '' file; do builtin_files+=("$file"); done < <(find skills prompts -type f -name '*.md' -print0)
  for file in "${manifest_scan[@]}"; do
    [[ ! -f "$file" ]] || builtin_files+=("$file")
  done
else
  host_files=("$@")
  for input in "$@"; do
    rel=${input#./}
    rel=${rel#"$repo_root"/}
    for scan_file in "${manifest_scan[@]}"; do
      if [[ "$rel" == "$scan_file" ]]; then
        builtin_files+=("$input")
        continue 2
      fi
    done
    [[ "$input" == *.md ]] || continue
    if [[ "$input" = /* ]]; then
      case "$input" in
        "$repo_root"/skills/*.md|"$repo_root"/prompts/*.md) builtin_files+=("$input") ;;
        "$repo_root"/*) ;;
        *) builtin_files+=("$input") ;;
      esac
    else
      case "$rel" in skills/*.md|prompts/*.md) builtin_files+=("$input") ;; esac
    fi
  done
fi

read_file() {
  if [[ ${LEAK_SCAN_STAGED:-0} == 1 ]]; then
    git show ":${1#./}"
  else
    cat -- "$1"
  fi
}

matches=0
for file in "${builtin_files[@]}"; do
  read_file "$file" > "$tmp_dir/content"
  if grep -aiEn "$built_in_regex" "$tmp_dir/content" > "$tmp_dir/matches"; then
    printf 'Built-in pattern matches in %s:\n' "$file"
    cat "$tmp_dir/matches"
    matches=$((matches + 1))
  else
    status=$?
    [[ "$status" -eq 1 ]] || exit "$status"
  fi
  if grep -aiEn '[a-z0-9]{6,}\.feishu\.cn' "$tmp_dir/content" > "$tmp_dir/candidates"; then
    if grep -vE 'open\.feishu\.cn|<tenant>\.feishu\.cn|xxx\.feishu\.cn' "$tmp_dir/candidates" > "$tmp_dir/matches"; then
      printf 'Built-in pattern matches in %s:\n' "$file"
      cat "$tmp_dir/matches"
      matches=$((matches + 1))
    else
      status=$?
      [[ "$status" -eq 1 ]] || exit "$status"
    fi
  else
    status=$?
    [[ "$status" -eq 1 ]] || exit "$status"
  fi
done

if [[ -s "$tmp_patterns" ]]; then
  for file in "${host_files[@]}"; do
    read_file "$file" > "$tmp_dir/content"
    if [[ "$file" == *.gz ]]; then
      gzip -cd "$tmp_dir/content" > "$tmp_dir/uncompressed"
      scan="$tmp_dir/uncompressed"
    else
      scan="$tmp_dir/content"
    fi
    if grep -aiEq -f "$tmp_patterns" "$scan"; then
      printf 'Host pattern matches in %s (patterns: %s)\n' "$file" "$extra_patterns_file"
      matches=$((matches + 1))
    else
      status=$?
      [[ "$status" -eq 1 ]] || exit "$status"
    fi
  done
fi
printf 'Summary: %d files with matches\n' "$matches"
[[ "$matches" -eq 0 ]]

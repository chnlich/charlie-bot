# The live sessions record: the sessions that the live server knows, read when a round launches. A web page PR's session
# View lines are checked against it.
#
# The launcher (remote-launch.sh) writes the record on the agent-box side, the one host that reaches the live server. It
# walks the server's task tree, including archived rows, with `charliebot session tree` and keeps each session that
# `charliebot screenshots resolve` resolves for `/?session=<id>`: the check that `charliebot land` makes, so both judge
# "known" alike. The runner keeps a copy in the round's evidence directory with the other merge facts, and pr-create and
# pr-merge read it. No round path reaches the live server.
#
# The record is TSV. Its first line is `read_at<TAB><UTC time>`; each further line is `<session id><TAB><name>`, the
# name escaped as `@tsv` writes it.
#
# This file defines functions for the scripts that source it, and runs nothing.

# live_sessions_record <file>: write the record of sessions the live server resolves to <file>. The tree is walked from
# its roots, including archived rows, a page of 500 at a time, and each row with children is walked in turn. A session
# that the tree lists but the server does not resolve is left out. On a failure the function names it and writes no file.
live_sessions_record() {
  local out=$1 read_at parent cursor page rows children id name status err content
  local -a queue=("") args order=()
  local -A seen=() names=()
  read_at=$(date -u +%Y-%m-%dT%H:%M:%SZ) || return 1
  while (( ${#queue[@]} > 0 )); do
    parent=${queue[0]}
    queue=("${queue[@]:1}")
    cursor=
    while :; do
      args=(session tree --include-archived --limit 500)
      [[ -z $parent ]] || args+=(--root "$parent")
      [[ -z $cursor ]] || args+=(--cursor "$cursor")
      if ! page=$(charliebot "${args[@]}"); then
        printf 'live sessions: charliebot session tree failed; no record was written\n' >&2
        return 1
      fi
      rows=$(jq -r '.items[] | [(.child_count | tostring), .id, .name] | @tsv' <<<"$page") || {
        printf 'live sessions: the tree page is not the expected JSON; no record was written\n' >&2
        return 1
      }
      while IFS=$'\t' read -r children id name; do
        [[ -n $id && -z ${seen[$id]+x} ]] || continue
        seen[$id]=1
        names[$id]=$name
        order+=("$id")
        if (( children > 0 )); then queue+=("$id"); fi
      done <<<"$rows"
      cursor=$(jq -r '.next_cursor // empty' <<<"$page") || {
        printf 'live sessions: the tree page has no readable next_cursor; no record was written\n' >&2
        return 1
      }
      [[ -n $cursor ]] || break
    done
  done
  content="read_at"$'\t'"$read_at"$'\n'
  for id in "${order[@]}"; do
    status=0
    err=$(charliebot screenshots resolve "/?session=$id" 2>&1 >/dev/null) || status=$?
    if (( status == 0 )); then
      content+="$id"$'\t'"${names[$id]}"$'\n'
    elif (( status == 1 )) && [[ $err == *"does not know session $id"* ]]; then
      continue
    else
      printf 'live sessions: resolving session %s failed (status %s): %s; no record was written\n' \
        "$id" "$status" "$err" >&2
      return 1
    fi
  done
  printf '%s' "$content" > "$out" || return 1
}

# live_sessions_read_at <file>: print the read time that the record's first line names.
live_sessions_read_at() {
  local header
  header=$(head -n 1 -- "$1") || return 1
  if [[ $header != $'read_at\t'* ]]; then
    printf 'the live sessions record %s does not open with its read time\n' "$1" >&2
    return 1
  fi
  printf '%s\n' "${header#*$'\t'}"
}

# live_sessions_knows <file> <id>: exit 0 when the record lists <id> as a session the live server knows.
live_sessions_knows() {
  awk -F '\t' -v id="$2" 'NR > 1 && $1 == id { found = 1 } END { exit !found }' "$1"
}

# live_session_views <body file>: print the session id that each `View:` line of a PR body names, in the order the body
# names them. A session is the `session` query of a `/?` view, the first one the line carries, as the land check
# reads it. A line that names no session prints nothing.
live_session_views() {
  local view pair id
  local -a pairs
  while IFS= read -r view; do
    [[ $view == '/?'* ]] || continue
    IFS='&' read -ra pairs <<<"${view#'/?'}"
    for pair in "${pairs[@]}"; do
      [[ $pair == session=* ]] || continue
      id=${pair#session=}
      if [[ -n $id ]]; then printf '%s\n' "$id"; fi
      break
    done
  done < <(sed -n 's/^View:[[:space:]]*//p' -- "$1" | sed 's/[[:space:]]*$//')
}

# live_sessions_web_change <changed paths file>: exit 0 and print `web` when a changed path alters a CharlieBot web
# page, and exit 0 with no output when none does. `charliebot screenshots check` passes a change that alters no web
# page at once, and refuses a web page change that carries no screenshot directory with a message that names the web
# page change. Any other failure exits 2 with the check's message.
live_sessions_web_change() {
  local changed=$1 err status=0
  err=$(charliebot screenshots check < "$changed" 2>&1 >/dev/null) || status=$?
  if (( status == 0 )); then
    return 0
  fi
  if [[ $err == *"alter a web page"* ]]; then
    printf 'web\n'
    return 0
  fi
  printf 'cannot tell whether the changed paths alter a web page: %s\n' "$err" >&2
  return 2
}

# live_sessions_check <body file> <record file> <changed paths file>: exit 0 when the PR may go on. A PR whose View
# lines name no session passes. A PR that names a session passes unless its changed paths alter a web page. A web page
# PR needs the record: it stops when the record is missing, and it is refused for each session the record does not
# list, with the session and the record's read time named. The callers print the outcome of the refusal.
live_sessions_check() {
  local body=$1 record=$2 changed=$3 web id read_at
  local -a ids
  mapfile -t ids < <(live_session_views "$body")
  (( ${#ids[@]} > 0 )) || return 0
  web=$(live_sessions_web_change "$changed") || return 2
  [[ -n $web ]] || return 0
  if [[ ! -r $record ]]; then
    printf 'the PR changes a web page and its View line names session %s, but the round has no live sessions record' \
      "${ids[0]}" >&2
    printf ' (%s).\n' "$record" >&2
    return 1
  fi
  read_at=$(live_sessions_read_at "$record") || return 1
  for id in "${ids[@]}"; do
    if ! live_sessions_knows "$record" "$id"; then
      printf 'the PR changes a web page and its View line names session %s, which the live server did not know' \
        "$id" >&2
      printf ' at %s (the launch record).\n' "$read_at" >&2
      return 1
    fi
  done
}

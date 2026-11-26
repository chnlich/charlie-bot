#!/usr/bin/env bash
# The moved-code reference check of a pull request: the one module that reads it.
#
# A pull request that renames or deletes a file leaves the old path named wherever the tree still spells it. This
# check reads the tree at the pull request's head and refuses each line that still names a deleted or renamed path,
# except recorded data, which keeps its old text when the pull request body names the line as <path>:<line>.
# It spells each path in full, so a name written another way (a bare file name, a module or a symbol) is not seen.
#
# github-post.sh pr-create calls moved_references_check. This file defines functions only. A caller sources it.

# moved_references_recorded <path>: succeed when the path is recorded data: a file under a fixtures or golden
# directory, or a captured reply named *-fixture.txt.
moved_references_recorded() {
  [[ $1 =~ (^|/)(fixtures|golden)/ || $1 == *-fixture.txt ]]
}

# moved_references_ere <text>: print the text with each extended-regex metacharacter escaped.
moved_references_ere() {
  printf '%s' "$1" | sed 's#[][\\.^$*+?(){}|]#\\&#g'
}

# moved_references_named <body file> <path> <line>: succeed when the body names <path>:<line> as a whole token, so
# line 2 does not match a mention of line 21.
moved_references_named() {
  grep -q -E -e "(^|[^[:alnum:]_.-])$(moved_references_ere "$2"):$3(\$|[^0-9])" -- "$1"
}

# moved_references_check <work dir> <head> <body file>: refuse, with status 1, each line of the head's tree that
# still names a path the pull request deletes or renames away from. A recorded-data line passes when the body names
# it. Status 2 means that git could not read the diff or the tree. Each refusal names its line on stderr.
moved_references_check() {
  local work=$1 head=$2 body=$3 sha status path old rc rest line_no key found=0
  local -a gone=()
  local -A reported=()
  sha=$(git rev-parse --verify --quiet "$head^{commit}") || {
    printf 'moved-code check: cannot resolve %s to a commit; nothing was sent.\n' "$head" >&2
    return 2
  }
  # --no-renames reports a rename as its old path's deletion and its new path's addition, so each old path is a D.
  git diff -z --no-renames --name-status "origin/main...$head" > "$work/moved-status" || {
    printf 'moved-code check: cannot read the changed paths of %s; nothing was sent.\n' "$head" >&2
    return 2
  }
  while IFS= read -r -d '' status; do
    IFS= read -r -d '' path || return 2
    if [[ $status == D ]]; then
      gone+=("$path")
    fi
  done < "$work/moved-status"
  for old in "${gone[@]}"; do
    # A path that the head still holds has no stale name: the change put it back.
    if git cat-file -e "$sha:$old" 2>/dev/null; then
      continue
    fi
    rc=0
    git grep -n -I -E -e "(^|[^[:alnum:]_.-])$(moved_references_ere "$old")($|[^[:alnum:]_/-]|[.]($|[^[:alnum:]_/-]))" \
      "$sha" -- > "$work/moved-hits" || rc=$?
    if [[ $rc -gt 1 ]]; then
      printf 'moved-code check: git grep failed (status %s) for %s; nothing was sent.\n' "$rc" "$old" >&2
      return 2
    fi
    while IFS= read -r hit; do
      # Each hit reads sha:path:line:text. A tracked path holds no colon, so the first two colons split it.
      rest=${hit#"$sha:"}
      path=${rest%%:*}
      rest=${rest#*:}
      line_no=${rest%%:*}
      key="$path:$line_no"
      if [[ -n ${reported[$key]+x} ]]; then
        continue
      fi
      reported[$key]=1
      if moved_references_recorded "$path"; then
        if moved_references_named "$body" "$path" "$line_no"; then
          continue
        fi
        printf 'moved-code check: recorded data %s names %s, which this pull request moves or deletes; ' \
          "$key" "$old" >&2
        printf 'name %s in the body, since recorded data keeps its old text\n' "$key" >&2
      else
        printf 'moved-code check: %s names %s, which this pull request moves or deletes; update the reference\n' \
          "$key" "$old" >&2
      fi
      found=1
    done < "$work/moved-hits"
  done
  if [[ $found -ne 0 ]]; then
    printf 'PR moves or deletes a path that the tree still names; nothing was sent.\n' >&2
    return 1
  fi
}

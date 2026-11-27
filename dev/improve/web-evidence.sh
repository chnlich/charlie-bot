#!/usr/bin/env bash
# The screenshot layout of an improve round's evidence directory, checked before the review.
#
# The top level of the round's evidence directory holds only the screenshots the pull request body names: the before
# and after screenshot of each view at each window size. Every other screenshot the round takes goes in the evidence
# directory's extra subfolder, which landing.md names. The merge's check (`charliebot screenshots check`) reads the top
# level only and refuses a PNG at a size outside the window sizes, so a side shot, click state or probe shot at the top
# level can fail the merge on a size the check does not know.
#
# github-post.sh pr-create runs web_evidence_check on the evidence directory and the body before it opens the pull
# request, so a layout fault stops the round before the review instead of at the merge.
#
# This file defines functions only. A caller sources it.

# web_evidence_body_names <body file>: print the screenshot file names that a body names, one per line, sorted and
# without repeats, as the merge's check reads them: every token that ends in `.png`, by its file name.
web_evidence_body_names() {
  local body=$1 names
  # A token is a run of ASCII letters, digits, `.`, `_`, `-` and `/`, and a token's trailing dots are shed.
  names=$(tr -c 'A-Za-z0-9._/-' '\n' <"$body" | sed -e 's/\.*$//' | grep -E '\.png$' | sed -e 's#.*/##' \
    | LC_ALL=C sort -u) || true
  [[ -z $names ]] || printf '%s\n' "$names"
}

# web_evidence_top_names <evidence dir>: print the PNG file names at the top level of the evidence directory, one per
# line, sorted. A subdirectory is not read: the merge's check reads the top level only, and the extra subfolder holds
# every other screenshot.
web_evidence_top_names() {
  local dir=$1
  find "$dir" -mindepth 1 -maxdepth 1 -iname '*.png' -printf '%f\n' | LC_ALL=C sort
}

# web_evidence_record <evidence dir> <record file>: write the pre-review record that the screenshot check reads. The
# review has not run, so pr-create records a successful Read of each PNG at the top level of the evidence directory.
# With the layout check holding the body and the top level to the same names, the check's read rule passes, and its
# window-size and size-coverage rules still run. The merge checks the review's own record instead.
web_evidence_record() {
  local dir=$1 record=$2 name path index=0
  : >"$record" || return 1
  while IFS= read -r name; do
    path=${dir}/$name
    path=${path//\\/\\\\}
    path=${path//\"/\\\"}
    printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_%d",' "$index" \
      >>"$record" || return 1
    printf '"name":"Read","input":{"file_path":"%s"}}]}}\n' "$path" >>"$record" || return 1
    printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_%d",' "$index" \
      >>"$record" || return 1
    printf '"is_error":false,"content":"ok"}]}}\n' >>"$record" || return 1
    index=$((index + 1))
  done < <(find "$dir" -mindepth 1 -maxdepth 1 -iname '*.png' -printf '%f\n')
  return 0
}

# web_evidence_check <evidence dir> <body file> <changed paths file> <scratch dir>: refuse a web page change whose
# evidence directory does not pass the layout and the merge's screenshot check. A change that alters no web page
# passes at once, because the check reads no evidence for it. A web page change needs an evidence directory whose top
# level holds exactly the PNGs the body names, and it must pass `charliebot screenshots check` on that directory, the
# body and the changed paths. Each refusal prints its reason and returns 1; nothing was sent.
web_evidence_check() {
  local dir=$1 body=$2 changed=$3 scratch=$4 status
  local -a top=() named=()
  local -A at_top=() in_body=()
  local name found=0
  # This call names no evidence directory, so a web page change refuses it (exit 1), and a change that alters no web
  # page passes at once. The refusal is read as the web page change, and the checks below run with the evidence named.
  # The check's stdout stays out of pr-create's, which carries gh's URL.
  set +e
  charliebot screenshots check --pr-body "$body" <"$changed" >/dev/null 2>"$scratch/screenshots.err"
  status=$?
  set -e
  if (( status == 0 )); then
    return 0
  fi
  if (( status != 1 )); then
    cat -- "$scratch/screenshots.err" >&2
    printf 'pr-create could not run the screenshot check; nothing was sent.\n' >&2
    return 1
  fi
  [[ -n $dir ]] || {
    printf 'The change alters a web page, so pr-create needs --evidence-dir naming the round evidence directory; ' >&2
    printf 'nothing was sent.\n' >&2
    return 1
  }
  [[ -d $dir ]] || { printf 'The evidence directory is missing: %s; nothing was sent.\n' "$dir" >&2; return 1; }
  web_evidence_body_names "$body" >"$scratch/body-names" || return 1
  web_evidence_top_names "$dir" >"$scratch/top-names" || return 1
  mapfile -t named <"$scratch/body-names"
  mapfile -t top <"$scratch/top-names"
  for name in "${top[@]}"; do at_top[$name]=1; done
  for name in "${named[@]}"; do in_body[$name]=1; done
  for name in "${top[@]}"; do
    [[ -n ${in_body[$name]+x} ]] || {
      printf 'The top level of %s holds %s, which the body does not name; move every screenshot the body does not ' \
        "$dir" "$name" >&2
      printf "name into the evidence directory's extra subfolder; nothing was sent.\n" >&2
      found=1
    }
  done
  for name in "${named[@]}"; do
    [[ -n ${at_top[$name]+x} ]] || {
      printf 'The body names %s, which is not a PNG at the top level of %s; name each screenshot the body carries by ' \
        "$name" "$dir" >&2
      printf 'the file that sits at the top level; nothing was sent.\n' >&2
      found=1
    }
  done
  (( found == 0 )) || return 1
  web_evidence_record "$dir" "$scratch/pr-create-review.ndjson" || return 1
  set +e
  charliebot screenshots check --pr-body "$body" --dir "$dir" \
    --review-record "$scratch/pr-create-review.ndjson" <"$changed" >/dev/null 2>"$scratch/screenshots.err"
  status=$?
  set -e
  (( status == 0 )) || {
    cat -- "$scratch/screenshots.err" >&2
    printf 'The round evidence does not pass the screenshot check; nothing was sent.\n' >&2
    return 1
  }
  return 0
}
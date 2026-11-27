#!/usr/bin/env bash
# The screenshot gate of an improve round's pull request: the layout of the evidence directory and the merge's
# screenshot check. pr-create runs it before the review, and pr-merge runs the layout check at the merge.
# landing.md (Web page evidence) states the layout; this file enforces it.
#
# This file defines functions only. A caller sources it.

# The first line of a pre-review record, which pr-create writes for its screenshot check before any review has run.
# pr-merge refuses a review record that holds this line (web_evidence_refuse_pre_review). A review's event stream never
# holds it: each line of the stream is an event object, and a tool result carries a file's text inside a JSON string,
# where its quotes are escaped.
WEB_EVIDENCE_PRE_REVIEW_LINE='{"type":"charliebot_pre_review"}'

# web_evidence_body_names <body file>: print the screenshot file names that a body names, one per line, sorted and
# without repeats, as the merge's check reads them: every token that ends in `.png`, by its file name.
web_evidence_body_names() {
  local body=$1
  # A token is a run of ASCII letters, digits, `.`, `_`, `-` and `/`, and a token's trailing dots are shed. grep exits
  # 1 when no token ends in `.png`, which is an empty list; any other grep failure fails the pipeline.
  LC_ALL=C tr -c 'A-Za-z0-9._/-' '\n' <"$body" | sed -e 's/\.*$//' | { grep -E '\.png$' || [[ $? -eq 1 ]]; } \
    | sed -e 's#.*/##' | LC_ALL=C sort -u
}

# web_evidence_top_names <evidence dir>: print the PNG file names at the top level of the evidence directory, one per
# line, sorted. The merge's check reads the top level only.
web_evidence_top_names() {
  local dir=$1
  find "$dir" -mindepth 1 -maxdepth 1 -iname '*.png' -printf '%f\n' | LC_ALL=C sort
}

# web_evidence_layout <evidence dir> <body file> <scratch dir>: print each way the top level of the evidence directory
# differs from the screenshots the body names, and return 1 when it differs. The caller says what it refuses.
web_evidence_layout() {
  local dir=$1 body=$2 scratch=$3 name found=0
  local -a top=() named=()
  local -A at_top=() in_body=()
  web_evidence_body_names "$body" >"$scratch/body-names" || return 1
  web_evidence_top_names "$dir" >"$scratch/top-names" || return 1
  mapfile -t named <"$scratch/body-names"
  mapfile -t top <"$scratch/top-names"
  for name in "${top[@]}"; do at_top[$name]=1; done
  for name in "${named[@]}"; do in_body[$name]=1; done
  for name in "${top[@]}"; do
    [[ -n ${in_body[$name]+x} ]] || {
      printf 'The top level of %s holds %s, which the body does not name; landing.md places each such screenshot in ' \
        "$dir" "$name" >&2
      printf "the evidence directory's extra subfolder.\n" >&2
      found=1
    }
  done
  for name in "${named[@]}"; do
    [[ -n ${at_top[$name]+x} ]] || {
      printf 'The body names %s, which is not a PNG at the top level of %s.\n' "$name" "$dir" >&2
      found=1
    }
  done
  (( found == 0 ))
}

# web_evidence_record <evidence dir> <top names file> <record file>: write the pre-review record that pr-create's
# screenshot check reads. No review has run, so the record opens with WEB_EVIDENCE_PRE_REVIEW_LINE and names a
# successful Read of each PNG the top-names file lists. The check's read rule then holds for a body that
# web_evidence_layout accepted, and its window-size and coverage rules still run on the files. The merge checks the
# review's own record instead.
web_evidence_record() {
  local dir=$1 names=$2 record=$3 name path index=0
  printf '%s\n' "$WEB_EVIDENCE_PRE_REVIEW_LINE" >"$record" || return 1
  while IFS= read -r name; do
    path=${dir}/$name
    path=${path//\\/\\\\}
    path=${path//\"/\\\"}
    printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_%d",' "$index" >>"$record" \
      || return 1
    printf '"name":"Read","input":{"file_path":"%s"}}]}}\n' "$path" >>"$record" || return 1
    printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_%d",' "$index" \
      >>"$record" || return 1
    printf '"is_error":false,"content":"ok"}]}}\n' >>"$record" || return 1
    index=$((index + 1))
  done <"$names"
}

# web_evidence_check <evidence dir> <body file> <changed paths file> <scratch dir>: pr-create's screenshot gate. A
# change that alters no web page passes at once. A web page change needs an evidence directory whose layout passes
# web_evidence_layout and whose screenshots pass `charliebot screenshots check`, with the pre-review record as the
# review record. Each refusal prints its reason and returns 1; nothing was sent.
web_evidence_check() {
  local dir=$1 body=$2 changed=$3 scratch=$4 status
  # Without an evidence directory the check refuses a web page change with exit 1 and passes any other change with exit
  # 0. Its stdout stays out of pr-create's, which carries gh's URL.
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
  web_evidence_layout "$dir" "$body" "$scratch" || {
    printf 'The evidence directory does not match the body; nothing was sent.\n' >&2
    return 1
  }
  web_evidence_record "$dir" "$scratch/top-names" "$scratch/pre-review.ndjson" || return 1
  set +e
  charliebot screenshots check --pr-body "$body" --dir "$dir" \
    --review-record "$scratch/pre-review.ndjson" <"$changed" >/dev/null 2>"$scratch/screenshots.err"
  status=$?
  set -e
  (( status == 0 )) || {
    cat -- "$scratch/screenshots.err" >&2
    printf 'The round evidence does not pass the screenshot check; nothing was sent.\n' >&2
    return 1
  }
  return 0
}

# web_evidence_refuse_pre_review <review record>: pr-merge's refusal of a pre-review record. It prints a refusal and
# returns 1 when the record holds WEB_EVIDENCE_PRE_REVIEW_LINE, returns 0 when it does not, and returns 2 with a message
# when the record cannot be read.
web_evidence_refuse_pre_review() {
  local record=$1 status=0
  grep -qxF -e "$WEB_EVIDENCE_PRE_REVIEW_LINE" -- "$record" || status=$?
  case $status in
    0)
      printf 'Refused pr-merge: %s is a pre-review record that pr-create wrote, not a review; nothing was merged.\n' \
        "$record" >&2
      return 1
      ;;
    1) return 0 ;;
    *)
      printf 'Cannot read the review record %s; nothing was merged.\n' "$record" >&2
      return 2
      ;;
  esac
}

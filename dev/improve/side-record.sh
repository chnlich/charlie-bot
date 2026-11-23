#!/usr/bin/env bash
# The improve trial side record: the one module that writes it and reads it.
#
# dev/improve/trial-side.sh writes one record for each serve trial side, and github-post.sh checks the records that a
# pull request body cites. trial-side.sh's header lists the record's four lines. Each record is a file that mktemp
# creates in the round's evidence directory, so a later side run never names the same file.
#
# This file defines functions only. A caller sources it.

# side_record_open <run dir> <commit>: create the side record and its output directory under the run directory, write
# the commit and output lines, and set SIDE_RECORD and SIDE_OUTPUT to their paths.
side_record_open() {
  local run_dir=$1 commit=$2 stamp
  stamp=$(date -u +%Y%m%dT%H%M%SZ)
  mkdir -p -- "$run_dir" || return 1
  SIDE_RECORD=$(mktemp "$run_dir/side-record-$stamp-XXXXXX") || return 1
  SIDE_OUTPUT=$(mktemp -d "$run_dir/side-output-$stamp-XXXXXX") || return 1
  printf 'commit=%s\noutput=%s\n' "$commit" "$SIDE_OUTPUT" >>"$SIDE_RECORD"
}

# side_record_take_binary <record> <binary>: append the sha256 of the server binary that the trial starts with.
side_record_take_binary() {
  local digest
  digest=$(sha256sum -- "$2") || return 1
  printf 'binary_sha256=%s\n' "${digest%% *}" >>"$1"
}

# side_record_close <record> <exit status>: append the side's exit status, which ends the record.
side_record_close() {
  printf 'exit_status=%s\n' "$2" >>"$1"
}

# side_record_refuse <record name> <reason...>: print the refusal on stderr and return 1.
side_record_refuse() {
  local name=$1
  shift
  printf 'side record check: %s: %s\n' "$name" "$*" >&2
  return 1
}

# side_record_check_one <evidence dir> <record name>: pass when the named record is complete. A complete record has one
# commit, one binary_sha256, an output directory that holds at least one entry, and an exit_status of 0.
side_record_check_one() {
  local dir=$1 name=$2 file key value output
  local -A field=()
  if [[ $name == */* || $name != side-record-* ]]; then
    printf 'side record check: a cited record is a side-record file name in the evidence directory: %s\n' "$name" >&2
    return 1
  fi
  file="$dir/$name"
  [[ -f $file ]] || { printf 'side record check: the cited side record is missing: %s\n' "$file" >&2; return 1; }
  while IFS='=' read -r key value; do
    case $key in
      commit | binary_sha256 | output | exit_status) ;;
      *) side_record_refuse "$name" "a line is not one of the four record fields: $key" || return 1 ;;
    esac
    if [[ -n ${field[$key]+x} ]]; then
      side_record_refuse "$name" "the record holds $key twice" || return 1
    fi
    field[$key]=$value
  done <"$file"

  [[ ${field[commit]-} =~ ^[0-9a-f]{40}$ ]] \
    || { side_record_refuse "$name" "no full commit sha"; return 1; }
  [[ ${field[binary_sha256]-} =~ ^[0-9a-f]{64}$ ]] \
    || { side_record_refuse "$name" "no binary_sha256: the trial never took its server binary"; return 1; }
  output=${field[output]-}
  [[ -d $output ]] || { side_record_refuse "$name" "the raw output location is missing: ${output:-none}"; return 1; }
  [[ -n $(find "$output" -mindepth 1 -print -quit) ]] \
    || { side_record_refuse "$name" "the raw output location is empty: $output"; return 1; }
  [[ -n ${field[exit_status]+x} ]] || { side_record_refuse "$name" "no exit_status: the side did not end"; return 1; }
  [[ ${field[exit_status]} == 0 ]] || { side_record_refuse "$name" "the side exited ${field[exit_status]}"; return 1; }
  return 0
}

# side_record_check_body <evidence dir> <body file> <draft>: check the side records that one pull request body cites.
# Each `Side record:` line names one record file in the evidence directory, and each named record must pass
# side_record_check_one. The body must name a record when the pull request is ready (draft 0) and the evidence
# directory holds side records. draft is 1 for a draft pull request, 0 for a ready one. An empty evidence directory
# means the caller gave none: the body passes only when it cites no record, since no record can be checked without one.
side_record_check_body() {
  local dir=$1 body=$2 draft=$3 line name status=0
  local -a cited=()
  while IFS= read -r line; do
    if [[ $line =~ ^Side\ record:[[:space:]]+(.+)$ ]]; then
      cited+=("${BASH_REMATCH[1]}")
    fi
  done <"$body"
  if [[ -z $dir ]]; then
    if (( ${#cited[@]} > 0 )); then
      printf 'side record check: the body cites a side record, which needs --evidence-dir.\n' >&2
      return 1
    fi
    return 0
  fi
  [[ -d $dir ]] || { printf 'side record check: the evidence directory is missing: %s\n' "$dir" >&2; return 1; }
  if (( ${#cited[@]} == 0 )) && (( draft == 0 )) && compgen -G "$dir/side-record-*" >/dev/null; then
    printf 'side record check: the body names no side record, but %s holds side records.\n' "$dir" >&2
    printf 'side record check: cite each record on its own line that starts "Side record:".\n' >&2
    return 1
  fi
  for name in "${cited[@]}"; do
    side_record_check_one "$dir" "$name" || status=1
  done
  return "$status"
}

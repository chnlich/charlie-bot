#!/usr/bin/env bash
# The text collection the push checks share (dev/git-hooks/pre-push and dev/improve/github-post.sh): one commit's
# outbound texts, written as files and listed as `field<TAB>path` lines so the scanner checks them in batch.
#
# Three lists per commit:
#   cred.list   message, diff, identity and decompressed `*.gz` blob — the credential stage
#   all.list    the same texts — the host-pattern stage
#   doc.list    the message plus the designated doc blobs — the generic stage
# A designated doc blob is a touched `skills/**/*.md` or `prompts/**/*.md`, or a path a
# `web/features/*/leak_scan_files.txt` manifest of that commit names.

# collect_commit_texts <sha> <dir> <tag> <cred_list> <all_list> <doc_list>
# Writes the commit's texts into <dir> as <kind>-<tag> files and appends their `field<TAB>path` lines to the lists.
# A git read or a gzip failure prints its defect to stderr and returns 2, so a push never skips a text unchecked.
collect_commit_texts() {
  local sha=$1 dir=$2 tag=$3 cred_list=$4 all_list=$5 doc_list=$6
  local message_file diff_file identity_file manifests manifest manifest_text scan_path touched path
  local blob_file doc_file file_index added_file
  local -A doc_paths=()

  message_file="$dir/message-$tag"
  git show -s --format=%B "$sha" > "$message_file" \
    || { printf 'cannot read the message of %s for the push check\n' "$sha" >&2; return 2; }
  printf 'commit %s message\t%s\n' "$sha" "$message_file" >> "$cred_list"
  printf 'commit %s message\t%s\n' "$sha" "$message_file" >> "$all_list"
  printf 'commit %s message\t%s\n' "$sha" "$message_file" >> "$doc_list"

  diff_file="$dir/diff-$tag"
  git show --format= --patch --no-color --no-ext-diff "$sha" > "$diff_file" \
    || { printf 'cannot read the diff of %s for the push check\n' "$sha" >&2; return 2; }
  printf 'commit %s diff\t%s\n' "$sha" "$diff_file" >> "$cred_list"
  # The host-pattern stage reads only what the commit adds: a removed line adds no bytes to the remote, and a cleanup
  # that deletes a leaked literal must not be refused for that literal.
  added_file="$dir/added-$tag"
  awk '/^(\+\+\+ |diff --git )/ { print; next } /^\+/ { print substr($0, 2) }' "$diff_file" > "$added_file" \
    || { printf 'cannot read the added lines of %s for the push check\n' "$sha" >&2; return 2; }
  printf 'commit %s diff\t%s\n' "$sha" "$added_file" >> "$all_list"

  identity_file="$dir/identity-$tag"
  git show -s --format='%an <%ae>%n%cn <%ce>' "$sha" > "$identity_file" \
    || { printf 'cannot read the identity of %s for the push check\n' "$sha" >&2; return 2; }
  printf 'commit %s identity\t%s\n' "$sha" "$identity_file" >> "$cred_list"
  printf 'commit %s identity\t%s\n' "$sha" "$identity_file" >> "$all_list"

  manifests=$(git ls-tree -r --name-only "$sha" -- 'web/features/*/leak_scan_files.txt') \
    || { printf 'cannot read the doc manifests of %s\n' "$sha" >&2; return 2; }
  while IFS= read -r manifest; do
    [[ -n "$manifest" ]] || continue
    manifest_text=$(git show "$sha:$manifest") \
      || { printf 'cannot read the doc manifest %s of %s\n' "$manifest" "$sha" >&2; return 2; }
    while IFS= read -r scan_path; do
      [[ -n "$scan_path" ]] && doc_paths[$scan_path]=1
    done <<< "$manifest_text"
  done <<< "$manifests"

  touched=$(git diff-tree --root --no-commit-id --name-only -r --diff-filter=ACMR "$sha") \
    || { printf 'cannot list the files of %s for the push check\n' "$sha" >&2; return 2; }
  file_index=0
  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    file_index=$((file_index + 1))
    if [[ "$path" == *.gz ]]; then
      blob_file="$dir/blob-$tag-$file_index"
      git show "$sha:$path" | gzip -cd > "$blob_file" || {
        printf 'cannot decompress %s of commit %s for the push check\n' "$path" "$sha" >&2
        return 2
      }
      printf 'commit %s blob %s\t%s\n' "$sha" "$path" "$blob_file" >> "$cred_list"
      printf 'commit %s blob %s\t%s\n' "$sha" "$path" "$blob_file" >> "$all_list"
    fi
    if [[ "$path" == skills/*.md || "$path" == prompts/*.md || -n "${doc_paths[$path]:-}" ]]; then
      doc_file="$dir/doc-$tag-$file_index"
      git show "$sha:$path" > "$doc_file" \
        || { printf 'cannot read %s of commit %s for the push check\n' "$path" "$sha" >&2; return 2; }
      printf 'commit %s doc %s\t%s\n' "$sha" "$path" "$doc_file" >> "$doc_list"
    fi
  done <<< "$touched"
}

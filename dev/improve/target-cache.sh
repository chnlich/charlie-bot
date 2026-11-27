# The pinned cargo target cache of a round's direction: the trim that bounds it and keeps the filesystem's free space above
# a floor. remote-runner.sh sources this file in the round's scope, after the `started` marker, and calls
#
#   trim_target_cache <home directory> <direction>
#
# once, before the round's agent or build starts. The file defines functions and constants only. It relies on the
# runner's `fail` and STDERR_PREFIX.

# A direction's pinned cargo target cache is capped at 128 GiB. Host3's perf target measured 79 GiB after its
# incremental data went, so the cap leaves about 49 GiB for one release and debug target to grow before the next trim.
readonly TARGET_CACHE_CAP_MIB=$((128 * 1024))
# The filesystem that holds the target caches keeps this much free space before a round builds. The floor's basis is the
# measured growth of the largest round build times the number of directions that can build at once. No such growth is
# measured, so the floor is the fallback of 200 GiB.
readonly TARGET_FREE_FLOOR_MIB=$((200 * 1024))

# target_size_mib <directory>: the size of a directory in MiB, 0 when it does not exist.
target_size_mib() {
  local directory=$1 output
  if [[ ! -e $directory ]]; then
    printf '0\n'
    return
  fi
  output=$(du -sm -- "$directory") || fail 1 "cannot read the size of $directory"
  printf '%s\n' "${output%%$'\t'*}"
}

# target_free_mib <directory>: the free space in MiB of the filesystem that holds the directory.
target_free_mib() {
  local directory=$1 output available
  output=$(df -P -k -- "$directory") || fail 1 "cannot read the free space of $directory"
  available=$(awk 'NR == 2 { print $4 }' <<<"$output")
  [[ $available =~ ^[0-9]+$ ]] || fail 1 "cannot read the free space of $directory from df: $output"
  printf '%s\n' $((available / 1024))
}

# remove_incremental_data <target directory>: removes the incremental data of every target tree in the directory, only
# that. A round can keep a target tree below the direction's cache instead of at it: the perf direction's dirs on host3
# are <cache>/<name>/<profile>/incremental, one level deeper than <cache>/<profile>/incremental. A two-level match leaves
# that data, frees nothing, and lets the next step delete the whole cache.
remove_incremental_data() {
  [[ -d $1 ]] || return 0
  find "$1" -mindepth 2 -type d -name incremental -prune -exec rm -rf -- {} +
}

# trim_target_cache <home directory> <direction>: bounds the direction's pinned target cache. It removes only this
# direction's cache. Above the cap, it removes the incremental data and then the whole target. Then, while the
# filesystem's free space is below the floor, it takes the same two steps, reading the free space after each. The round
# stops before any build when the floor is still not met.
trim_target_cache() {
  local home_dir=$1 direction=$2
  local cache_root=$home_dir/.cache/improve-target
  local target_dir=$cache_root/$direction
  local before after free action=none
  mkdir -p -- "$cache_root"
  before=$(target_size_mib "$target_dir")
  after=$before
  if (( before > TARGET_CACHE_CAP_MIB )); then
    remove_incremental_data "$target_dir"
    after=$(target_size_mib "$target_dir")
    action=incremental
    if (( after > TARGET_CACHE_CAP_MIB )); then
      rm -rf -- "$target_dir"
      after=0
      action=incremental+directory
    fi
  fi
  printf '%s target cache: before=%sMiB action=%s after=%sMiB cap=%sMiB\n' \
    "$STDERR_PREFIX" "$before" "$action" "$after" "$TARGET_CACHE_CAP_MIB" >&2

  free=$(target_free_mib "$cache_root")
  (( free < TARGET_FREE_FLOOR_MIB )) || return 0
  remove_incremental_data "$target_dir"
  free=$(target_free_mib "$cache_root")
  printf '%s target floor: removed the incremental data: free=%sMiB floor=%sMiB\n' \
    "$STDERR_PREFIX" "$free" "$TARGET_FREE_FLOOR_MIB" >&2
  (( free < TARGET_FREE_FLOOR_MIB )) || return 0
  rm -rf -- "$target_dir"
  free=$(target_free_mib "$cache_root")
  printf '%s target floor: removed the target directory: free=%sMiB floor=%sMiB\n' \
    "$STDERR_PREFIX" "$free" "$TARGET_FREE_FLOOR_MIB" >&2
  (( free >= TARGET_FREE_FLOOR_MIB )) || fail 1 \
    "the round stops before any build: free space on $cache_root is ${free}MiB, below the floor of" \
    "${TARGET_FREE_FLOOR_MIB}MiB, after $target_dir is removed"
}

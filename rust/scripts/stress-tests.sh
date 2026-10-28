#!/usr/bin/env bash
# Run the test binaries of the given packages repeatedly, several copies at
# once, and report every failing test with its failure text. Run it on the
# build host, from anywhere inside the rust/ tree:
#   scripts/stress-tests.sh <runs> <copies> <package>...
# <runs> is the number of executions per test binary; the binary runs <copies>
# at a time until <runs> executions are done.
set -u

if [ "$#" -lt 3 ]; then
  echo "usage: stress-tests.sh <runs> <copies> <package>..." >&2
  exit 2
fi
runs=$1
copies=$2
shift 2

cd "$(dirname "$0")/.."
log_dir=target/stress-logs
rm -rf "$log_dir"
mkdir -p "$log_dir"

total_failures=0
total_executions=0

for pkg in "$@"; do
  # The test binaries come from cargo's own JSON: a compiler-artifact whose
  # target kind is a test or lib target and whose executable exists. jq serves
  # both the build host and CI.
  mapfile -t bins < <(cargo test --no-run -p "$pkg" --message-format=json 2>/dev/null \
    | jq -r 'select(.reason == "compiler-artifact") | select(.target.kind | any(. == "test" or . == "lib")) | select(.executable != null) | .executable' \
    | sort -u)
  if [ "${#bins[@]}" -eq 0 ]; then
    echo "stress-tests: no test binaries found for $pkg" >&2
    exit 1
  fi
  for bin in "${bins[@]}"; do
    name="$(basename "$bin")"
    pkg_dir="$log_dir/$pkg/$name"
    mkdir -p "$pkg_dir"
    done_count=0
    while [ "$done_count" -lt "$runs" ]; do
      pids=()
      batch=0
      while [ "$batch" -lt "$copies" ] && [ "$done_count" -lt "$runs" ]; do
        log="$pkg_dir/run-$((done_count + 1)).log"
        timeout 600 "$bin" >"$log" 2>&1 &
        pids+=($!)
        done_count=$((done_count + 1))
        batch=$((batch + 1))
      done
      wait "${pids[@]}"
    done
    for log in "$pkg_dir"/run-*.log; do
      total_executions=$((total_executions + 1))
      if ! grep -q "test result:" "$log"; then
        echo "FAIL (crashed or hung): $pkg/$name ($(basename "$log"))"
        sed -n '1,40p' "$log"
        total_failures=$((total_failures + 1))
        continue
      fi
      if grep -q "^test result: FAILED\|panicked at\|^error:" "$log"; then
        echo "FAIL: $pkg/$name ($(basename "$log"))"
        awk '/^failures:|^test result: FAILED|panicked at/{show=1} show' "$log" | sed -n '1,60p'
        total_failures=$((total_failures + 1))
      fi
    done
  done
done

echo "stress-tests: $total_executions executions, $total_failures failures"
[ "$total_failures" -eq 0 ]

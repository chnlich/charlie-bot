#!/usr/bin/env bash
set -euo pipefail

guard_dir=$(cd -P -- "$(dirname -- "$0")" && pwd -P)
entry="$guard_dir/prescreen-run.sh"
test_guard_dir=${PRESCREEN_TEST_GUARD_DIR:-$guard_dir}
test_python=${PRESCREEN_TEST_PYTHON:-python3}
test_root=$(mktemp -d "${TMPDIR:-/tmp}/prescreen-run-test.XXXXXX")
trap 'rm -rf -- "$test_root"' EXIT

home="$test_root/home"
scope_dir="$home/local-work-guard"
session_ids=$(for _ in 1 2 3; do cat /proc/sys/kernel/random/uuid; done)
mapfile -t session_ids <<< "$session_ids"
root_id=${session_ids[0]}
child_id=${session_ids[1]}
outside_id=${session_ids[2]}
mkdir -p "$scope_dir" "$home/sessions/$root_id" "$home/sessions/$child_id" "$home/sessions/$outside_id"
printf '{"task_parent_id":null}\n' > "$home/sessions/$root_id/metadata.json"
printf '{"task_parent_id":"%s"}\n' "$root_id" > "$home/sessions/$child_id/metadata.json"
printf '{"task_parent_id":null}\n' > "$home/sessions/$outside_id/metadata.json"
printf '%s\n' "$root_id" > "$scope_dir/prescreen-scope.txt"

export PATH="$test_guard_dir/bin:$PATH"
export RUSTUP_TOOLCHAIN="$test_guard_dir/rustup"
export RUSTC_WRAPPER="$test_guard_dir/rustup/rustc-wrapper"
export PYTEST_PLUGINS=refuse_local_tests
export PYTHONPATH="$test_guard_dir/python${PYTHONPATH:+:$PYTHONPATH}"
export NODE_OPTIONS="--require=$test_guard_dir/node/refuse-local-tests.cjs${NODE_OPTIONS:+ $NODE_OPTIONS}"
export MAKEFILES="$test_guard_dir/make/refuse.mk${MAKEFILES:+ $MAKEFILES}"
export GOFLAGS="-toolexec=$test_guard_dir/go/toolexec.sh${GOFLAGS:+ $GOFLAGS}"
export CC="$test_guard_dir/bin/cc"
export CXX="$test_guard_dir/bin/c++"
existing_path_entries=()
IFS=: read -r -a existing_path_entries <<< "$PATH"
base_path_entries=()
for path_entry in "${existing_path_entries[@]}"; do
  if [[ $path_entry == */dev/local-work-guard/bin || $path_entry == "$test_guard_dir/bin" ]]; then
    continue
  fi
  base_path_entries+=("$path_entry")
done
base_path=$(IFS=:; printf '%s' "${base_path_entries[*]}")
export PATH="$test_guard_dir/bin:$test_guard_dir/bin:$base_path"

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

run_scoped() {
  local caller_id=$1
  shift
  CHARLIEBOT_HOME="$home" CHARLIEBOT_SESSION_ID="$caller_id" "$entry" "$@"
}

assert_scoped_compilers_with_path() {
  local input_path=$1 expected_path=$2
  if PRESCREEN_TEST_EXPECT_PATH="$expected_path" PATH="$input_path" run_scoped "$child_id" bash -c '
    set -euo pipefail
    if [[ $PATH != "$PRESCREEN_TEST_EXPECT_PATH" ]]; then
      printf "unexpected PATH after removing the guard directory: %s\n" "$PATH" >&2
      exit 1
    fi
    cc -fsyntax-only "$1"
    c++ -fsyntax-only "$2"
  ' _ "$test_root/small.c" "$test_root/small.cpp"; then
    :
  else
    fail 'an in-scope C or C++ compile failed after guard PATH removal'
  fi
}

printf 'int main(void) { return 0; }\n' > "$test_root/small.c"
printf 'int main() { return 0; }\n' > "$test_root/small.cpp"

path_before_guard="$test_root/path-before-guard"
path_between_guard_copies="$test_root/path-between-guard-copies"
mkdir -p "$path_before_guard" "$path_between_guard_copies"
expected_path="$path_before_guard:$path_between_guard_copies:$base_path"
assert_scoped_compilers_with_path \
  "$path_before_guard:$test_guard_dir/bin:$path_between_guard_copies:$base_path" "$expected_path"
assert_scoped_compilers_with_path \
  "$path_before_guard:$test_guard_dir/bin:$path_between_guard_copies:$test_guard_dir/bin:$base_path" \
  "$expected_path"

if PATH="$base_path" run_scoped "$child_id" true > "$test_root/missing-guard-path.log" 2>&1; then
  fail 'an absent guard compiler directory allowed the entry point to run'
else
  status=$?
fi
if (( status != 1 )); then
  fail "an absent guard compiler directory exited with $status instead of 1"
fi

if cc -fsyntax-only "$test_root/small.c" > "$test_root/direct-cc.log" 2>&1; then
  fail 'a direct cc invocation compiled outside the entry point'
else
  status=$?
fi
if (( status != 97 )); then
  fail "a direct cc invocation exited with $status instead of 97"
fi

run_scoped "$child_id" cc -c "$test_root/small.c" -o "$test_root/cc.o"
run_scoped "$child_id" gcc -c "$test_root/small.c" -o "$test_root/gcc.o"
run_scoped "$child_id" c++ -c "$test_root/small.cpp" -o "$test_root/cxx.o"
run_scoped "$child_id" g++ -c "$test_root/small.cpp" -o "$test_root/gxx.o"
run_scoped "$child_id" bash -c '"$CC" -fsyntax-only "$1"' _ "$test_root/small.c"
run_scoped "$child_id" bash -c '"$CXX" -fsyntax-only "$1"' _ "$test_root/small.cpp"

missing_home="$test_root/missing-home"
mkdir -p "$missing_home/sessions/$child_id"
printf '{"task_parent_id":null}\n' > "$missing_home/sessions/$child_id/metadata.json"
if CHARLIEBOT_HOME="$missing_home" \
  CHARLIEBOT_SESSION_ID="$child_id" \
  "$entry" true > "$test_root/missing-scope.log" 2>&1; then
  fail 'a missing scope file allowed the entry point to run'
else
  status=$?
fi
if (( status != 1 )) || \
  ! grep -Fq "$missing_home/local-work-guard/prescreen-scope.txt" "$test_root/missing-scope.log"; then
  fail 'a missing scope file did not refuse with its path'
fi

if run_scoped "$outside_id" true > "$test_root/outside-scope.log" 2>&1; then
  fail 'an out-of-scope session entered the compiler exception'
else
  status=$?
fi
if (( status != 1 )); then
  fail "an out-of-scope session exited with $status instead of 1"
fi

printf 'def test_sample():\n  assert True\n' > "$test_root/test_sample.py"
if run_scoped "$child_id" "$test_python" -m pytest -q "$test_root/test_sample.py" > "$test_root/pytest.log" 2>&1; then
  fail 'pytest ran inside the compiler exception'
else
  status=$?
fi
if (( status != 4 )); then
  fail "pytest exited with $status instead of the guard refusal code 4"
fi

serial_dir="$test_root/serial"
mkdir -p "$serial_dir"
cat > "$serial_dir/run.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if ! mkdir "$SERIAL_TEST_DIR/active" 2>/dev/null; then
  touch "$SERIAL_TEST_DIR/overlap"
  exit 1
fi
sleep 0.7
rmdir "$SERIAL_TEST_DIR/active"
EOF
chmod +x "$serial_dir/run.sh"
SERIAL_TEST_DIR="$serial_dir" run_scoped "$child_id" "$serial_dir/run.sh" > "$serial_dir/first.log" 2>&1 &
first_pid=$!
for _ in {1..100}; do
  if [[ -d $serial_dir/active ]]; then
    break
  fi
  sleep 0.02
done
if [[ ! -d $serial_dir/active ]]; then
  fail 'the first serialized run did not start'
fi
SERIAL_TEST_DIR="$serial_dir" run_scoped "$child_id" "$serial_dir/run.sh" > "$serial_dir/second.log" 2>&1 &
second_pid=$!
waiting_seen=0
for _ in {1..100}; do
  if grep -iq 'wait' "$serial_dir/second.log"; then
    waiting_seen=1
    break
  fi
  sleep 0.02
done
if (( ! waiting_seen )); then
  fail 'the second serialized run did not report that it waits'
fi
wait "$first_pid"
wait "$second_pid"
if [[ -e $serial_dir/overlap ]]; then
  fail 'two entry-point commands ran at the same time'
fi

printf 'All prescreen entry-point checks passed.\n'

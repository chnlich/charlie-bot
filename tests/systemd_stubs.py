"""Stubs for the systemctl and systemd-run calls the launcher makes, for tests that run it.

The launcher's preflight reads the committed unit files and asks systemctl for the live
values; the start itself runs through systemd-run. A test that executes the launcher puts
these two programs on PATH so neither the host's slices nor the user manager are touched.
The systemd-run stub retains successful scope names for the test lifetime because earlier
scopes may still own live processes.
"""

import json
import pathlib


def _memtotal_percent_bytes(percent: int) -> int:
  """A percentage of host memory in bytes, by the launcher's rounding rule.

  systemd rounds down to a whole 4096-byte page: floor(MemTotal pages * percent / 100) * 4096,
  where a page is MemTotal kB / 4.
  """
  for line in pathlib.Path("/proc/meminfo").read_text(encoding="ascii").splitlines():
    if line.startswith("MemTotal:"):
      return ((int(line.split()[1]) // 4) * percent // 100) * 4096
  raise RuntimeError("/proc/meminfo has no MemTotal line")


# The values the committed unit files declare, in the bytes `systemctl show` reports.
AGENTS_MEMORY_MAX = _memtotal_percent_bytes(80)  # charliebot-agents.slice's MemoryMax=80%


def write_systemctl(
    bin_dir: pathlib.Path,
    *,
    agents_memory_max: int | str = AGENTS_MEMORY_MAX,
    agents_memory_swap_max: int | str = 0,
    agents_active_state: str = "active") -> pathlib.Path:
  """A ``systemctl --user show`` stand-in reporting the agents' slice's committed values."""
  stub = bin_dir / "systemctl"
  stub.write_text(
      f"""#!/usr/bin/env bash
case "$*" in
  *charliebot-agents.slice*)
    echo ActiveState={agents_active_state}
    echo MemoryMax={agents_memory_max}
    echo MemorySwapMax={agents_memory_swap_max}
    ;;
  *)
    exit 1
    ;;
esac
""",
      encoding="utf-8")
  stub.chmod(0o755)
  return stub


def write_systemd_run(bin_dir: pathlib.Path) -> pathlib.Path:
  """Write a scope stub that records successful units and rejects loaded names."""
  stub = bin_dir / "systemd-run"
  stub.write_text(
      """#!/usr/bin/env bash
if [ "${SYSTEMD_RUN_REFUSE_ALL:-0}" = "1" ]; then
  echo "stub systemd-run: start refused by test" >&2
  exit 1
fi
# The program is a variable, not stdin: the command after -- keeps the caller's stdin, as under systemd-run.
read -r -d '' RECORD_PY <<'PY'
import json
import os
import pathlib
import sys

arguments = sys.argv[1:]
separator = arguments.index("--")
unit_name = next(argument.split("=", 1)[1] for argument in arguments if argument.startswith("--unit="))
records_path = pathlib.Path(os.environ["SYSTEMD_RUN_RECORDS"])
if records_path.exists():
  records = [json.loads(line) for line in records_path.read_text(encoding="utf-8").splitlines()]
else:
  records = []
if any(record["unit_name"] == unit_name for record in records):
  print(f"Failed to start transient scope unit: Unit {unit_name}.scope was already loaded or has a fragment file.",
        file=sys.stderr)
  raise SystemExit(1)
command = arguments[separator + 1:]
record = {"unit_name": unit_name, "arguments": arguments}
with records_path.open("a", encoding="utf-8") as records_file:
  records_file.write(json.dumps(record) + "\\n")
os.execvp(command[0], command)
PY
SYSTEMD_RUN_RECORDS="${BASH_SOURCE[0]%/*}/systemd-run.records" python3 -c "$RECORD_PY" "$@"
""",
      encoding="utf-8")
  stub.chmod(0o755)
  return stub


def read_systemd_run_records(bin_dir: pathlib.Path) -> list[dict[str, object]]:
  """Read the unit names and argument lists recorded beside the systemd-run stub."""
  records_path = bin_dir / "systemd-run.records"
  if not records_path.exists():
    return []
  return [json.loads(line) for line in records_path.read_text(encoding="utf-8").splitlines()]


def install(bin_dir: pathlib.Path) -> None:
  """Both stubs, with the committed limits."""
  write_systemctl(bin_dir)
  write_systemd_run(bin_dir)

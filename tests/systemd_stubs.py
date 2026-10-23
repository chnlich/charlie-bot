"""Stubs for the systemctl and systemd-run calls the launcher makes, for tests that run it.

The launcher's preflight reads the committed unit files and asks systemctl for the live
values; the start itself runs through systemd-run. A test that executes the launcher puts
these two programs on PATH so neither the host's slices nor the user manager are touched.
"""

import pathlib

# The values the committed unit files declare, in the bytes `systemctl show` reports.
AGENTS_MEMORY_HIGH = 34 * 1024**3
AGENTS_MEMORY_MAX = 36 * 1024**3
REPAIR_MEMORY_MAX = 8 * 1024**3


def write_systemctl(
    bin_dir: pathlib.Path,
    *,
    agents_memory_max: int | str = AGENTS_MEMORY_MAX,
    agents_active_state: str = "active") -> pathlib.Path:
  """A ``systemctl --user show`` stand-in reporting the slices' committed values."""
  stub = bin_dir / "systemctl"
  stub.write_text(
      f"""#!/usr/bin/env bash
case "$*" in
  *charliebot-agents.slice*)
    echo ActiveState={agents_active_state}
    echo MemoryHigh={AGENTS_MEMORY_HIGH}
    echo MemoryMax={agents_memory_max}
    echo MemorySwapMax=0
    ;;
  *charliebot-repair.slice*)
    echo ActiveState=active
    echo MemoryHigh=infinity
    echo MemoryMax={REPAIR_MEMORY_MAX}
    echo MemorySwapMax=0
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
  """A ``systemd-run --user --scope`` stand-in: exec the command after the ``--`` separator."""
  stub = bin_dir / "systemd-run"
  stub.write_text(
      """#!/usr/bin/env bash
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
  shift
done
if [ "$#" -eq 0 ]; then
  echo "stub systemd-run: no command after --" >&2
  exit 1
fi
shift
exec "$@"
""",
      encoding="utf-8")
  stub.chmod(0o755)
  return stub


def install(bin_dir: pathlib.Path) -> None:
  """Both stubs, with the committed limits."""
  write_systemctl(bin_dir)
  write_systemd_run(bin_dir)

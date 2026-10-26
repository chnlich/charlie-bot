"""The launcher checks its slices and trial scope before start, and both shell entry points parse."""

import os
import pathlib
import re
import shutil
import subprocess

import pytest
import systemd_stubs

ROOT = pathlib.Path(__file__).resolve().parents[1]
UNIT_FILES = ("charliebot.slice", "charliebot-agents.slice", "charliebot-repair.slice")


def stash_launcher(tmp_path: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path]:
  """A copy of the launcher and its unit directory, plus a PATH directory the test stubs systemctl in."""
  scripts = tmp_path / "scripts"
  (scripts / "systemd").mkdir(parents=True)
  launcher = scripts / "start-server.sh"
  shutil.copy(ROOT / "scripts" / "start-server.sh", launcher)
  for name in UNIT_FILES:
    shutil.copy(ROOT / "scripts" / "systemd" / name, scripts / "systemd" / name)
  bin_dir = tmp_path / "bin"
  bin_dir.mkdir()
  return launcher, bin_dir


def run_preflight(launcher: pathlib.Path, bin_dir: pathlib.Path) -> subprocess.CompletedProcess[str]:
  return subprocess.run(
      ["bash", "-c", 'source "$1"; preflight_slices', "_",
       str(launcher)],
      env={
          **os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}"
      },
      capture_output=True,
      text=True,
      check=False)


def run_launcher_preflight(launcher: pathlib.Path, bin_dir: pathlib.Path, log_dir: pathlib.Path,
                           **extra_env: str) -> subprocess.CompletedProcess[str]:
  env = {
      **os.environ,
      "PATH": f"{bin_dir}:{os.environ['PATH']}",
      "CHARLIEBOT_LOG_DIR": str(log_dir),
  }
  env.update(extra_env)
  return subprocess.run(["bash", str(launcher), "--preflight"], env=env, capture_output=True, text=True, check=False)


def test_preflight_accepts_limits_that_match_the_unit_files(tmp_path: pathlib.Path) -> None:
  launcher, bin_dir = stash_launcher(tmp_path)
  systemd_stubs.write_systemctl(bin_dir)
  result = run_preflight(launcher, bin_dir)
  assert result.returncode == 0, result.stderr
  assert result.stderr == ""


def test_preflight_reads_a_unit_file_whose_last_line_has_no_newline(tmp_path: pathlib.Path) -> None:
  launcher, bin_dir = stash_launcher(tmp_path)
  # The repair unit's last assignment is the swap limit; bash's read drops an unterminated last line.
  unit_file = launcher.parent / "systemd" / "charliebot-repair.slice"
  unit_file.write_text("[Slice]\nMemoryMax=8G\nMemorySwapMax=0", encoding="utf-8")
  systemd_stubs.write_systemctl(bin_dir)
  result = run_preflight(launcher, bin_dir)
  assert result.returncode == 0, result.stderr


def test_preflight_rejects_a_slice_limit_that_differs_from_its_unit_file(tmp_path: pathlib.Path) -> None:
  launcher, bin_dir = stash_launcher(tmp_path)
  # The unit file is the single home: reading 36G from systemd against a 40G unit file stops the start.
  unit_file = launcher.parent / "systemd" / "charliebot-agents.slice"
  unit_file.write_text(unit_file.read_text().replace("MemoryMax=36G", "MemoryMax=40G"), encoding="utf-8")
  systemd_stubs.write_systemctl(bin_dir)
  result = run_preflight(launcher, bin_dir)
  assert result.returncode != 0
  assert "charliebot-agents.slice" in result.stderr
  assert "MemoryMax" in result.stderr
  assert str(systemd_stubs.AGENTS_MEMORY_MAX) in result.stderr
  assert str(40 * 1024**3) in result.stderr


def test_preflight_rejects_a_missing_required_unit_limit(tmp_path: pathlib.Path) -> None:
  launcher, bin_dir = stash_launcher(tmp_path)
  unit_file = launcher.parent / "systemd" / "charliebot-agents.slice"
  unit_file.write_text(unit_file.read_text().replace("MemoryMax=36G\n", ""), encoding="utf-8")
  systemd_stubs.write_systemctl(bin_dir, agents_memory_max="infinity")
  result = run_preflight(launcher, bin_dir)
  assert result.returncode != 0
  assert "charliebot-agents.slice unit file has no finite MemoryMax" in result.stderr


def test_preflight_rejects_an_inactive_slice(tmp_path: pathlib.Path) -> None:
  launcher, bin_dir = stash_launcher(tmp_path)
  systemd_stubs.write_systemctl(bin_dir, agents_active_state="inactive")
  result = run_preflight(launcher, bin_dir)
  assert result.returncode != 0
  assert "charliebot-agents.slice" in result.stderr
  assert "inactive" in result.stderr


def test_launcher_preflight_checks_slices_and_records_a_trial_scope(tmp_path: pathlib.Path) -> None:
  launcher, bin_dir = stash_launcher(tmp_path)
  systemd_stubs.write_systemctl(bin_dir)
  systemd_stubs.write_systemd_run(bin_dir)
  log_dir = tmp_path / "logs"

  result = run_launcher_preflight(launcher, bin_dir, log_dir)

  assert result.returncode == 0, result.stdout + result.stderr
  records = systemd_stubs.read_systemd_run_records(bin_dir)
  assert len(records) == 1
  record = records[0]
  unit_name = str(record["unit_name"])
  assert re.fullmatch(r"charliebot-preflight-\d{8}_\d{6}-\d+", unit_name)
  assert record["arguments"] == [
      "--user", "--scope", f"--unit={unit_name}", "--slice=charliebot.slice", "-p", "MemorySwapMax=0", "--", "true"
  ]
  assert not log_dir.exists()


def test_launcher_preflight_exits_1_when_systemd_refuses_the_trial_scope(tmp_path: pathlib.Path) -> None:
  launcher, bin_dir = stash_launcher(tmp_path)
  systemd_stubs.write_systemctl(bin_dir)
  systemd_stubs.write_systemd_run(bin_dir)
  log_dir = tmp_path / "logs"

  result = run_launcher_preflight(launcher, bin_dir, log_dir, SYSTEMD_RUN_REFUSE_ALL="1")

  assert result.returncode == 1
  assert "start refused by test" in result.stderr
  assert systemd_stubs.read_systemd_run_records(bin_dir) == []
  assert not log_dir.exists()


def test_launcher_preflight_exits_1_when_a_slice_limit_differs(tmp_path: pathlib.Path) -> None:
  launcher, bin_dir = stash_launcher(tmp_path)
  systemd_stubs.write_systemctl(bin_dir, agents_memory_max=systemd_stubs.AGENTS_MEMORY_MAX + 1)
  systemd_stubs.write_systemd_run(bin_dir)
  log_dir = tmp_path / "logs"

  result = run_launcher_preflight(launcher, bin_dir, log_dir)

  assert result.returncode == 1
  assert "charliebot-agents.slice MemoryMax" in result.stderr
  assert systemd_stubs.read_systemd_run_records(bin_dir) == []
  assert not log_dir.exists()


def test_launcher_rejects_an_unknown_argument_before_writing_a_server_log(tmp_path: pathlib.Path) -> None:
  launcher, bin_dir = stash_launcher(tmp_path)
  log_dir = tmp_path / "logs"

  result = subprocess.run(
      ["bash", str(launcher), "--unknown"],
      env={
          **os.environ,
          "PATH": f"{bin_dir}:{os.environ['PATH']}",
          "CHARLIEBOT_LOG_DIR": str(log_dir),
      },
      capture_output=True,
      text=True,
      check=False)

  assert result.returncode == 1
  assert "usage: start-server.sh [--preflight]" in result.stderr
  assert not log_dir.exists()


@pytest.mark.parametrize("script", ["start-server.sh", "setup.sh"])
def test_shell_entry_points_parse(script: str) -> None:
  result = subprocess.run(["bash", "-n", str(ROOT / "scripts" / script)], capture_output=True, text=True, check=False)
  assert result.returncode == 0, result.stderr

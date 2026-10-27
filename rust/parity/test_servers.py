"""Port reservation, listener ownership and server log events for the parity servers."""

import concurrent.futures
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Iterator

import pytest

from rust.parity import parity
from rust.parity import servers

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
# One event line as each server writes it: the Python server's console renderer, then the Rust server's JSON.
PYTHON_EVENT = "2026-10-18 22:38:55 [info     ] server_started purpose='server startup' ok=True\n"
RUST_EVENT = (
    '{"event": "server_started", "purpose": "server startup", "ok": true, "level": "info", '
    '"timestamp": "2026-10-18T22:38:55.000Z"}\n')
PYTHON_FIELDS = "2026-10-18 22:38:55 [info     ] server_started purpose='server startup' ok=True\n"
RUST_FIELDS = (
    '{"event": "server_started", "purpose": "server startup", "ok": true, "level": "info", '
    '"timestamp": "2026-10-18T22:38:55.000Z"}\n')


@pytest.fixture
def short_root() -> Iterator[pathlib.Path]:
  """A tree root directly under /tmp: the runner's TMPDIR makes the tmux socket path pass 107 bytes."""
  root = pathlib.Path(tempfile.mkdtemp(dir="/tmp"))
  try:
    yield root
  finally:
    shutil.rmtree(root)


def test_wait_ready_refuses_a_listener_outside_the_server_tree(short_root: pathlib.Path) -> None:
  # This test process holds the listener, so the server's tree never owns the port.
  with servers.pick_ports(1) as ports, socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    listener.bind((servers.HOST, ports[0]))
    listener.listen()
    tree = servers.make_tree("candidate", short_root / "candidate")
    server = servers.ServerProcess(
        "candidate", ["/usr/bin/sleep", "60"], tree, ports[0], REPO_ROOT, 1, servers.STRACE_BIN)
    server.start()
    try:
      with pytest.raises(RuntimeError, match=f"port {ports[0]}") as error:
        server.wait_ready()
    finally:
      server.kill()
  assert str(os.getpid()) in str(error.value)


def test_concurrent_pick_ports_return_disjoint_ports() -> None:
  barrier = threading.Barrier(2, timeout=10)

  def hold_two_ports() -> list[int]:
    # Each call keeps its reservation until both have picked, so the second pick runs against the first's locks.
    with servers.pick_ports(2) as ports:
      barrier.wait()
      return ports

  with concurrent.futures.ThreadPoolExecutor(2) as pool:
    futures = [pool.submit(hold_two_ports) for _ in range(2)]
    first, second = (future.result() for future in futures)
  assert len(first) == len(second) == 2
  assert not set(first) & set(second)


@pytest.mark.parametrize("line", [PYTHON_EVENT, RUST_EVENT], ids=["python-console", "rust-json"])
def test_log_event_wait_returns_on_the_event_in_either_servers_form(tmp_path: pathlib.Path, line: str) -> None:
  log = tmp_path / "baseline.server-1.log"
  log.write_text("Traceback (most recent call last):\n  File \"synthetic.py\", line 1\n" + line, encoding="utf-8")
  assert servers.wait_for_log_events({"baseline": log}, ("server_started",), {}, timeout_seconds=0.2) == []


@pytest.mark.parametrize("line", [PYTHON_FIELDS, RUST_FIELDS], ids=["python-console", "rust-json"])
def test_log_event_wait_matches_each_field_as_its_logged_text(tmp_path: pathlib.Path, line: str) -> None:
  log = tmp_path / "candidate.server-1.log"
  log.write_text(line, encoding="utf-8")
  fields = {"purpose": "server startup", "ok": "true"}
  assert servers.wait_for_log_events({"candidate": log}, ("server_started",), fields, timeout_seconds=0.2) == []
  mismatch = {"purpose": "server startup", "ok": "false"}
  assert servers.wait_for_log_events({"candidate": log}, ("server_started",), mismatch, timeout_seconds=0.2) == [
      "candidate server did not log server_started with purpose=server startup, ok=false within 0.2s"
  ]


def test_log_event_wait_reads_each_side_from_its_own_log(tmp_path: pathlib.Path) -> None:
  baseline = tmp_path / "baseline.server-1.log"
  candidate = tmp_path / "candidate.server-1.log"
  baseline.write_text(PYTHON_EVENT, encoding="utf-8")
  candidate.write_text("", encoding="utf-8")
  logs = {"baseline": baseline, "candidate": candidate}
  assert servers.wait_for_log_events(
      logs, ("server_started",), {},
      timeout_seconds=0.2) == ["candidate server did not log server_started within 0.2s"]


def test_log_event_wait_names_both_events_when_neither_arrives(tmp_path: pathlib.Path) -> None:
  log = tmp_path / "baseline.server-1.log"
  log.write_text(PYTHON_FIELDS, encoding="utf-8")
  assert servers.wait_for_log_events(
      {"baseline": log}, ("synthetic_event_a", "synthetic_event_b"), {},
      timeout_seconds=0.2) == ["baseline server did not log synthetic_event_a or synthetic_event_b within 0.2s"]


def test_log_event_wait_ignores_a_line_until_its_newline_arrives(tmp_path: pathlib.Path) -> None:
  log = tmp_path / "baseline.server-1.log"
  log.write_text(PYTHON_EVENT.rstrip("\n"), encoding="utf-8")
  assert servers.read_log_events(log) == []

  def finish_line() -> None:
    time.sleep(0.1)
    with log.open("a", encoding="utf-8") as handle:
      handle.write("\n")

  writer = threading.Thread(target=finish_line)
  writer.start()
  try:
    assert servers.wait_for_log_events({"baseline": log}, ("server_started",), {}, timeout_seconds=5) == []
  finally:
    writer.join()


def test_tmux_link_gives_each_tree_the_given_executable_and_nothing_beside_it(short_root: pathlib.Path) -> None:
  tools = short_root / "tools" / "bin"
  tools.mkdir(parents=True)
  tmux = tools / "tmux"
  tmux.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
  tmux.chmod(0o755)
  beside = tools / "beside-tmux"
  beside.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
  beside.chmod(0o755)
  sides = [
      parity.Side(label, servers.make_tree(label, short_root / label), 1, servers.STRACE_BIN)
      for label in ("base", "cand")
  ]
  parity._link_tmux(sides, tmux)
  for side in sides:
    link = side.tree.fake_bin / "tmux"
    assert link.is_symlink()
    assert link.resolve() == tmux.resolve()
    path = servers.server_env(side.tree, REPO_ROOT)["PATH"]
    assert shutil.which("tmux", path=path) == str(link)
    assert shutil.which("beside-tmux", path=path) is None


def test_headless_chrome_link_points_each_tree_at_the_binary(short_root: pathlib.Path) -> None:
  chrome = short_root / "headless_shell"
  chrome.write_bytes(b"")
  sides = [
      parity.Side(label, servers.make_tree(label, short_root / label), 1, servers.STRACE_BIN)
      for label in ("base", "cand")
  ]
  parity._link_headless_chrome(sides, chrome)
  for side in sides:
    link = side.tree.root / "bin" / "headless-chrome"
    assert link.is_symlink()
    assert link.resolve() == chrome.resolve()


def test_stop_headless_browsers_counts_and_stops_a_browser_the_server_left(short_root: pathlib.Path) -> None:
  tree = servers.make_tree("candidate", short_root / "candidate")
  profile = tree.tmp / f"{servers.HEADLESS_PROFILE_PREFIX}leaked"
  browser = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)", f"--user-data-dir={profile}"])
  try:
    with servers.pick_ports(1) as ports:
      server = servers.ServerProcess(
          "candidate", ["/usr/bin/sleep", "60"], tree, ports[0], REPO_ROOT, 1, servers.STRACE_BIN)
      assert server._stop_headless_browsers() == 1
    assert browser.poll() is not None
  finally:
    browser.kill()
    browser.wait()


def test_traced_argv_runs_the_strace_it_is_given(tmp_path: pathlib.Path) -> None:
  tree = servers.Tree(label="candidate", root=tmp_path)
  argv = servers.traced_argv(["/usr/bin/true"], tree, tmp_path / "trace.log", pathlib.Path("/opt/tools/strace"))
  assert argv[0] == "/opt/tools/strace"


def test_preflight_exits_naming_a_missing_strace(tmp_path: pathlib.Path) -> None:
  missing = tmp_path / "strace"
  with pytest.raises(SystemExit, match="missing strace") as error:
    parity._preflight(None, missing, pathlib.Path(sys.executable))
  assert str(missing) in str(error.value)


def test_preflight_exits_naming_a_missing_tmux(tmp_path: pathlib.Path) -> None:
  missing = tmp_path / "tmux"
  with pytest.raises(SystemExit, match="missing tmux") as error:
    parity._preflight(None, pathlib.Path(sys.executable), missing)
  assert str(missing) in str(error.value)


def test_preflight_exits_naming_a_tmux_that_is_not_an_executable_file(tmp_path: pathlib.Path) -> None:
  directory = tmp_path / "tmux"
  directory.mkdir()
  with pytest.raises(SystemExit, match="missing tmux") as error:
    parity._preflight(None, pathlib.Path(sys.executable), directory)
  assert str(directory) in str(error.value)

"""Tests for src/features/artifacts/artifact_check.py and the ``charliebot artifact check`` CLI.

The goal-length and page-height measurements moved here from src/features/artifacts/plans.py with their
budgets unchanged; the DOM assertions are the new mechanical half of each genre's GRAMMAR.
Renderer work and the probe's model call are doubled out — no headless chrome, no backend
subprocess, no HTTP.
"""

import contextlib
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    CLI_COMMON_GET_CONFIG_PATCH_TARGET,
    ROOT,
    make_plan_setup,
    plan_page_html,
    write_plan_artifact,
    write_stub_chrome,
)

from src.features.artifacts import artifact_check, artifact_shared
from src.features.artifacts import cli as artifact_cli
from src.features.artifacts.artifact_check import run_assertions
from src.features.artifacts.cli import main as artifact_main
from src.infra.config import CharlieBotConfig


def _genre_doc(genre: str, body: str) -> str:
  """Full HTML document for *genre*: its template's <style> block verbatim plus *body*."""
  template = (ROOT / "prompts" / artifact_shared.GENRE_TEMPLATES[genre]).read_text(encoding="utf-8")
  style = re.search(r"<style>.*?</style>", template, re.DOTALL).group(0)
  return f"<html><head>{style}</head><body>{body}</body></html>"


def _write(tmp_path: Path, content: str, name: str = "page.html") -> Path:
  artifact = tmp_path / name
  artifact.write_text(content, encoding="utf-8")
  return artifact


def _by_name(outcomes: list[artifact_check.AssertionOutcome]) -> dict[str, list[artifact_check.AssertionOutcome]]:
  by_name: dict[str, list[artifact_check.AssertionOutcome]] = {}
  for outcome in outcomes:
    by_name.setdefault(outcome.name, []).append(outcome)
  return by_name


def _run(
    genre: str,
    artifact: Path,
    cfg: CharlieBotConfig | None = None,
) -> dict[str, list[artifact_check.AssertionOutcome]]:
  return _by_name(run_assertions(genre, artifact, cfg))


def _chrome_cfg(tmp_path: Path) -> SimpleNamespace:
  return SimpleNamespace(headless_chrome_bin=write_stub_chrome(tmp_path, 800))


def _sitrep_ok_doc() -> str:
  return _genre_doc(
      "sitrep", '<section><h2><span class="n">1</span> What waits on you?</h2><p>Nothing.</p></section>'
      '<section><h2><span class="n">2</span> What is this and why?</h2>'
      '<p>Why. <span class="req">r1</span></p></section>'
      '<section><h2><span class="n">3</span> What was verified?</h2><p>Done. <span class="src">s</span></p></section>'
      '<section><h2><span class="n">4</span> Risks</h2>'
      '<p><span class="tag fact">Fact</span> The reading holds. <span class="src">s</span></p></section>'
      '<section><h2><span class="n">5</span> What happens next?</h2><p>Next.</p></section>')


# ---------------------------------------------------------------------------
# style-verbatim
# ---------------------------------------------------------------------------


def test_style_verbatim_fails_on_tampered_style(tmp_path: Path) -> None:
  doc = _sitrep_ok_doc().replace("</style>", "body{color:red}\n</style>", 1)
  outcomes = _run("sitrep", _write(tmp_path, doc))
  (outcome,) = outcomes["style-verbatim"]
  assert not outcome.passed
  assert "differs from the genre template prompts/sitrep_template.html" in outcome.detail


# ---------------------------------------------------------------------------
# Shipped templates pass their own genre
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("genre", "template"),
    [
        ("plan", "plan_template.html"),
        ("sitrep", "sitrep_template.html"),
        ("debug", "debug_template.html"),
        ("explain", "explain_template.html"),
    ],
)
def test_shipped_template_passes_its_own_genre(tmp_path: Path, genre: str, template: str) -> None:
  cfg = _chrome_cfg(tmp_path)
  outcomes = run_assertions(genre, ROOT / "prompts" / template, cfg)
  assert [o for o in outcomes if not o.passed] == []


# ---------------------------------------------------------------------------
# page-height: a measurement report against the 2000 px target, passing at any height
# ---------------------------------------------------------------------------


def test_page_height_over_the_target_passes_and_names_the_overage(tmp_path: Path) -> None:
  cfg = SimpleNamespace(headless_chrome_bin=write_stub_chrome(tmp_path, 2600))
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html()), cfg)["page-height"]
  assert outcome.passed
  assert outcome.detail == "2600 px: 600 px over the 2000 px target"


def test_page_height_at_or_under_the_target_reports_the_target(tmp_path: Path) -> None:
  cfg = SimpleNamespace(headless_chrome_bin=write_stub_chrome(tmp_path, 1967))
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html()), cfg)["page-height"]
  assert outcome.passed
  assert outcome.detail == "1967 px (target 2000)"


def test_page_height_without_a_renderer_fails(tmp_path: Path) -> None:
  cfg = SimpleNamespace(headless_chrome_bin=None)
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html()), cfg)["page-height"]
  assert not outcome.passed


# ---------------------------------------------------------------------------
# CLI: charliebot artifact check
# ---------------------------------------------------------------------------


def _invoke(argv_tail: list[str]) -> SystemExit:
  with patch("sys.argv", ["artifact", "check", *argv_tail]), pytest.raises(SystemExit) as exc_info:
    artifact_main()
  return exc_info.value


def _run_cli(argv_tail: list[str]) -> int:
  exit_ = _invoke(argv_tail)
  assert isinstance(exit_.code, int)
  return exit_.code


def _probe_cfg(tmp_path: Path) -> tuple[SimpleNamespace, dict[str, SimpleNamespace]]:
  options = {name: SimpleNamespace(id=name) for name in ("alpha", "beta")}
  cfg = SimpleNamespace(
      headless_chrome_bin=write_stub_chrome(tmp_path, 800),
      backends=SimpleNamespace(preference=["alpha", "beta"]),
      get_backend_option=options.get,
      sessions_dir=tmp_path,
  )
  return cfg, options


class _FakeBackend:

  def __init__(self, answer: str | None = None, error: str | None = None) -> None:
    self._answer = answer
    self._error = error
    self.calls: list[dict] = []

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float) -> str:
    self.calls.append({"prompt": prompt, "system_prompt": system_prompt, "timeout": timeout})
    if self._error is not None:
      raise RuntimeError(self._error)
    assert self._answer is not None
    return self._answer


def _patch_backends(monkeypatch: pytest.MonkeyPatch, backends: dict[str, _FakeBackend]) -> None:
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda option, cfg: backends[option.id])


def test_cli_probe_runs_after_assertions_pass_and_prints_backend_and_answers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  backends = {
      "alpha": _FakeBackend(error="boom"),
      "beta": _FakeBackend(answer="(1) The reader's problem.\n(2)-(6) fine.")
  }
  _patch_backends(monkeypatch, backends)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 0
  lines = capsys.readouterr().out.splitlines()
  assert lines[-5:] == [
      "--- cold read ---",
      "attempt alpha failed: boom",
      "backend beta",
      "(1) The reader's problem.",
      "(2)-(6) fine.",
  ]
  # The page text and the substituted trigger ride in one prompt; the placeholder is gone.
  prompt = backends["beta"].calls[0]["prompt"]
  assert artifact.read_text(encoding="utf-8") in prompt
  assert '"where are we?"' in prompt
  assert "(7) Read as an engineer who knows the domain" in prompt
  assert "<trigger message verbatim>" not in prompt
  assert backends["beta"].calls[0]["timeout"] == artifact_check.ARTIFACT_PROBE_TIMEOUT == 300.0


def test_cli_assertions_only_passes_a_page_over_the_height_target(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, plan_page_html())
  cfg = SimpleNamespace(headless_chrome_bin=write_stub_chrome(tmp_path, 2600))
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  assert _run_cli([str(artifact), "--genre", "plan", "--assertions-only"]) == 0
  assert "ok page-height 2600 px: 600 px over the 2000 px target" in capsys.readouterr().out


def test_cli_assertions_only_fails_when_the_height_cannot_be_measured(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, plan_page_html())
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: SimpleNamespace(headless_chrome_bin=None))
  assert _run_cli([str(artifact), "--genre", "plan", "--assertions-only"]) == 1
  assert "FAIL page-height" in capsys.readouterr().out


def test_cli_unknown_genre_is_usage_error() -> None:
  assert _run_cli(["page.html", "--genre", "weird"]) == 2


# ---------------------------------------------------------------------------
# CLI: charliebot artifact check --background
#
# The detached child is doubled out on every path: _spawn_cold_read stand-ins either start a
# real sleep 30 in a new session (payload pid, kill checks) or capture their arguments; the
# child output itself runs in process below, against the stub backends. post_internal_api is
# doubled too, so no test calls a server or registers a real trigger.
# ---------------------------------------------------------------------------


def _enter_via_unified_cli(monkeypatch: pytest.MonkeyPatch) -> None:
  """Stand in for src.app.main.main(): the unified entry records its module for subprocess re-entry."""
  monkeypatch.setattr("src.runtime.cli.common._CLI_ENTRY_MODULE", "src.app.main")


def _sleep_spawn(tracked: list[subprocess.Popen]) -> Callable:
  """_spawn_cold_read stand-in starting a real sleep 30 in a new session."""

  def spawn(argv: list[str], log_path: Path, cwd: Path) -> subprocess.Popen:
    proc = subprocess.Popen(
        ["sleep", "30"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True)
    tracked.append(proc)
    return proc

  return spawn


def _reap_spawned(tracked: list[subprocess.Popen]) -> None:
  for proc in tracked:
    if proc.poll() is None:
      os.killpg(proc.pid, signal.SIGKILL)
      proc.wait()


def _clean_coldread_dir(session_id: str) -> None:
  """Remove the real log directory the background path writes under /tmp for *session_id*."""
  shutil.rmtree(Path("/tmp/charliebot-coldread") / session_id[:8], ignore_errors=True)


def _registering_post(cfg: SimpleNamespace, session_id: str, calls: list[dict]) -> Callable:
  """post_internal_api stand-in: persists the trigger the way the server would, then answers."""

  def post(endpoint: str, payload: dict, *, readback: object = None, rejection_exit_codes: object = None) -> dict:
    calls.append({"endpoint": endpoint, "payload": payload})
    triggers_dir = cfg.sessions_dir / session_id / "triggers"
    triggers_dir.mkdir(parents=True, exist_ok=True)
    stored = {
        "id": "trig-1",
        "status": "pending",
        "message": payload["message"],
        "watch_targets": payload["watch_targets"],
        "created_at": "2026-10-08T00:00:00+00:00",
        "fire_at": "2026-10-08T00:11:00+00:00",
    }
    (triggers_dir / "trig-1.json").write_text(json.dumps(stored), encoding="utf-8")
    return {"trigger_id": "trig-1", "fire_at": "2026-10-08T00:11:00+00:00"}

  return post


def test_background_requires_trigger(tmp_path: Path) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  assert _run_cli([str(artifact), "--genre", "sitrep", "--background"]) == 2


def test_background_refuses_assertions_only() -> None:
  argv = ["page.html", "--genre", "sitrep", "--trigger", "t", "--background", "--assertions-only"]
  assert _run_cli(argv) == 2


def test_background_with_a_failing_assertion_starts_no_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  broken = _sitrep_ok_doc().replace('<span class="n">5</span>', '<span class="n">7</span>')
  artifact = _write(tmp_path, broken)
  cfg, _options = _probe_cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "sess-coldread-fail")
  spawn = MagicMock()
  monkeypatch.setattr(artifact_cli, "_spawn_cold_read", spawn)
  post = MagicMock()
  monkeypatch.setattr("src.runtime.cli.common.post_internal_api", post)

  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?", "--background"]) == 1
  assert "FAIL sections-numbered" in capsys.readouterr().out
  spawn.assert_not_called()
  post.assert_not_called()


def test_background_starts_the_cold_read_and_registers_its_wake(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """Passing assertions: the command returns at once (the sleep outlives it), one trigger
  watches the detached pid with backends x 300 + 60 seconds of max wait and a label holding
  the log path, and the log's first line names the page and its sha256."""
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  session_id = "sess-coldread-run"
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", session_id)
  _enter_via_unified_cli(monkeypatch)
  spawned: list[subprocess.Popen] = []
  monkeypatch.setattr(artifact_cli, "_spawn_cold_read", _sleep_spawn(spawned))
  calls: list[dict] = []
  monkeypatch.setattr("src.runtime.cli.common.post_internal_api", _registering_post(cfg, session_id, calls))
  try:
    assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?", "--background"]) == 0
  finally:
    _reap_spawned(spawned)
    _clean_coldread_dir(session_id)
  (proc,) = spawned
  (call,) = calls
  out = capsys.readouterr().out
  payload = call["payload"]
  log_path = Path(payload["message"].removeprefix("cold read: "))
  assert call["endpoint"] == "/api/internal/schedule-trigger"
  assert payload["session_id"] == session_id
  assert payload["delay_seconds"] == 2 * 300 + 60
  assert payload["watch_targets"] == [{"kind": "local_pid", "pid": proc.pid}]
  assert len(payload["message"]) <= 200
  assert f"cold read started: pid {proc.pid} log {log_path} trigger trig-1" in out
  assert log_path.parent == Path("/tmp/charliebot-coldread") / session_id[:8]
  assert log_path.name.startswith(f"{artifact.name}.")


def test_background_log_first_line_names_the_page_and_its_sha256(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  session_id = "sess-coldread-log"
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", session_id)
  _enter_via_unified_cli(monkeypatch)
  spawned: list[subprocess.Popen] = []
  monkeypatch.setattr(artifact_cli, "_spawn_cold_read", _sleep_spawn(spawned))
  calls: list[dict] = []
  monkeypatch.setattr("src.runtime.cli.common.post_internal_api", _registering_post(cfg, session_id, calls))
  try:
    assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?", "--background"]) == 0
    log_path = Path(calls[0]["payload"]["message"].removeprefix("cold read: "))
    header = log_path.read_text(encoding="utf-8").splitlines()[0]
  finally:
    _reap_spawned(spawned)
    _clean_coldread_dir(session_id)
  assert header == f"page {artifact} sha256 {hashlib.sha256(artifact.read_bytes()).hexdigest()}"


def test_background_kills_the_process_group_when_the_wake_is_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  session_id = "sess-coldread-full"
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", session_id)
  _enter_via_unified_cli(monkeypatch)
  spawned: list[subprocess.Popen] = []
  monkeypatch.setattr(artifact_cli, "_spawn_cold_read", _sleep_spawn(spawned))
  reason = f"session {session_id} has 5 pending triggers (limit 5); trigger rejected"

  def rejecting_post(endpoint: str, payload: dict, *, readback: object = None,
                     rejection_exit_codes: object = None) -> dict:
    print(json.dumps({"error": reason, "code": "server_error", "effect": "none"}), file=sys.stderr)
    raise SystemExit(1)

  monkeypatch.setattr("src.runtime.cli.common.post_internal_api", rejecting_post)
  try:
    assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?", "--background"]) == 1
  finally:
    _reap_spawned(spawned)
    _clean_coldread_dir(session_id)
  (proc,) = spawned
  assert proc.poll() is not None
  with pytest.raises(ProcessLookupError):
    os.killpg(proc.pid, 0)
  assert f"wake registration rejected: {reason}" in capsys.readouterr().out


def test_background_child_argv_runs_the_same_tree_and_interpreter(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  session_id = "sess-coldread-argv"
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", session_id)
  _enter_via_unified_cli(monkeypatch)
  captured_spawn: dict = {}

  def spawn(argv: list[str], log_path: Path, cwd: Path) -> SimpleNamespace:
    captured_spawn.update(argv=argv, log_path=log_path, cwd=cwd)
    return SimpleNamespace(pid=43210)

  monkeypatch.setattr(artifact_cli, "_spawn_cold_read", spawn)
  calls: list[dict] = []
  monkeypatch.setattr("src.runtime.cli.common.post_internal_api", _registering_post(cfg, session_id, calls))
  try:
    assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?", "--background"]) == 0
  finally:
    _clean_coldread_dir(session_id)
  assert captured_spawn["argv"] == [
      sys.executable, "-m", "src.app.main", "artifact", "check",
      str(artifact), "--genre", "sitrep", "--trigger", "where are we?"
  ]
  assert captured_spawn["cwd"] == Path(artifact_cli.__file__).resolve().parents[3]


def test_background_label_over_the_trigger_limit_exits_before_the_spawn(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc(), name=f"{'p' * 170}.html")
  cfg, _options = _probe_cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "sess-coldread-long")
  spawn = MagicMock()
  monkeypatch.setattr(artifact_cli, "_spawn_cold_read", spawn)

  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "t", "--background"]) == 1
  assert "at most 200 characters" in capsys.readouterr().err
  spawn.assert_not_called()
  _clean_coldread_dir("sess-coldread-long")


def test_cold_read_log_holds_the_header_then_the_probe_output(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The detached child's log, reproduced in process: the parent's first line, then exactly
  the foreground command's output (assertion lines, then the cold-read block)."""
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  backends = {
      "alpha": _FakeBackend(error="boom"),
      "beta": _FakeBackend(answer="(1) The reader's problem.\n(2)-(6) fine."),
  }
  _patch_backends(monkeypatch, backends)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  log_path = tmp_path / "cold.log"
  artifact_cli._start_cold_read_log(log_path, artifact)
  with log_path.open("a", encoding="utf-8") as log, contextlib.redirect_stdout(log):
    assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 0
  lines = log_path.read_text(encoding="utf-8").splitlines()
  assert lines[0] == f"page {artifact} sha256 {hashlib.sha256(artifact.read_bytes()).hexdigest()}"
  assert lines[-5:] == [
      "--- cold read ---",
      "attempt alpha failed: boom",
      "backend beta",
      "(1) The reader's problem.",
      "(2)-(6) fine.",
  ]


def test_cold_read_log_ends_with_probe_could_not_run_when_every_backend_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  backends = {"alpha": _FakeBackend(error="boom"), "beta": _FakeBackend(error="kaput")}
  _patch_backends(monkeypatch, backends)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  log_path = tmp_path / "cold.log"
  artifact_cli._start_cold_read_log(log_path, artifact)
  with log_path.open("a", encoding="utf-8") as log, contextlib.redirect_stdout(log):
    assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 1
  lines = log_path.read_text(encoding="utf-8").splitlines()
  assert lines[-1] == "probe could not run: every backend failed (2 tried)"


# ---------------------------------------------------------------------------
# Registration parity: plan present enforces exactly artifact check --genre plan
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plan_present_and_artifact_check_reject_the_same_assertions_on_one_fixture(tmp_path: Path) -> None:
  broken = plan_page_html().replace('<span class="n">4</span>', '<span class="n">9</span>').replace(
      '<div class="foot"><p>How to respond.</p></div>', "")
  cfg, _session_blocks, plan_mgr, meta = await make_plan_setup(tmp_path)
  file_rel = write_plan_artifact(cfg, meta.id, "plan_01.html", content=broken)

  cli_failures = {o.name for o in run_assertions("plan", cfg.sessions_dir / meta.id / file_rel, cfg) if not o.passed}
  assert cli_failures == {"sections-numbered", "foot-present"}

  with pytest.raises(ValueError) as exc_info:
    await plan_mgr.present(meta.id, file=file_rel, title="P1")
  message = str(exc_info.value)
  for name in cli_failures:
    assert name in message
  assert message.endswith(
      "Measure locally with: charliebot artifact check <artifact.html> --genre plan --assertions-only")


@pytest.mark.asyncio
async def test_plan_present_accepts_a_page_over_the_height_target(tmp_path: Path) -> None:
  cfg, _session_blocks, plan_mgr, meta = await make_plan_setup(tmp_path)
  cfg.headless_chrome_bin = write_stub_chrome(tmp_path, 2600)
  file_rel = write_plan_artifact(cfg, meta.id, "plan_01.html")

  result = await plan_mgr.present(meta.id, file=file_rel, title="P1")

  assert result["v"] == 1


# ---------------------------------------------------------------------------
# byte-integrity
# ---------------------------------------------------------------------------

_DAMAGED_BYTES = [(b"\t", "0x09"), (b"\x0c", "0x0c"), (b"\r", "0x0d")]


@pytest.mark.parametrize("damaged_byte,hex_name", _DAMAGED_BYTES)
def test_byte_integrity_fails_on_non_lf_control_byte(tmp_path: Path, damaged_byte: bytes, hex_name: str) -> None:
  """TAB / formfeed / CR in the raw source bytes fail the gate, each offset and value named."""
  doc = _sitrep_ok_doc().replace("Nothing.", f"Before{damaged_byte.decode('latin-1')}After")
  artifact = tmp_path / "damaged.html"
  artifact.write_bytes(doc.encode("utf-8"))
  (outcome,) = _run("sitrep", artifact)["byte-integrity"]
  assert not outcome.passed
  assert hex_name in outcome.detail
  assert "at offset" in outcome.detail

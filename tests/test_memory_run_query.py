"""Run-scoped memory query identity: a present CHARLIEBOT_RUN_TOKEN fixes the
audience from the verified, active owning Run's role; every failure mode fails
visibly instead of falling back to operator CLI behavior."""

from __future__ import annotations

import io
import subprocess
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

import pytest
from conftest import make_home_config

from src.core.models import RunRecord, TaskSpec
from src.core.run_token import CallerIdentity, RunTokenClaims, sign_run_token
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

pytestmark = pytest.mark.asyncio


def _write_store(cfg) -> None:
  memory_dir = cfg.memory_dir
  memory_dir.mkdir(parents=True, exist_ok=True)
  (memory_dir / "topics").write_text("alpha resident\nbeta\n", encoding="utf-8")
  for topic, slug, audience, title, body in [
      ("alpha", "m1", "master", "Master Entry", "master body"),
      ("beta", "w1", "worker", "Worker Entry", "worker body"),
  ]:
    entry_dir = memory_dir / "entries" / topic
    entry_dir.mkdir(parents=True, exist_ok=True)
    front = f"---\nscope: user\ntopic: {topic}\naudience: {audience}\ntitle: {title}\n---\n"
    (entry_dir / f"{slug}.md").write_text(front + body, encoding="utf-8")


async def _launched_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *,
                        profile: str) -> tuple[object, TaskTreeManager, str, str, str]:
  cfg = make_home_config(tmp_path)
  import src.core.config as core_config
  # The CLI's store root is charliebot_home_dir() / "memory" (env-resolved, config-free):
  # pin CHARLIEBOT_HOME to this config's home so the seeded store and every reader
  # (CLI verbs, run-token audience resolution) see one home.
  monkeypatch.setenv(core_config.CHARLIEBOT_HOME_ENV, str(cfg.charliebot_home))
  core_config._credentials_cache.seed(core_config.Credentials(
      path=cfg.charliebot_home / "credentials.yaml",
      sections={"charliebot": {"access_key": "query-op-key"}}))
  _write_store(cfg)
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  meta = await tree.create_task(
      request_id="r", task_parent_id=None, profile=profile,
      task=TaskSpec(goal="g"), name="N", backend=None, caller=CallerIdentity(kind="operator"))
  run_id = "query-run"
  await tree.runs.register_run(RunRecord(id=run_id, session_id=meta.id, kind="work"))
  proc = subprocess.Popen(["/bin/sleep", "60"])
  from src.core.runs import read_pid_stat
  pair = read_pid_stat(proc.pid)
  assert pair is not None
  await tree.runs.record_launch(meta.id, run_id, pid=proc.pid, pid_start=pair[0])
  token = sign_run_token(
      RunTokenClaims(session_id=meta.id, run_id=run_id, agent="worker"), "query-op-key")
  return cfg, tree, meta.id, run_id, token, proc


def _run_cli(monkeypatch: pytest.MonkeyPatch, cfg, argv: list[str], token: str | None) -> tuple[str, str, int]:
  import src.cli.memory as cli
  monkeypatch.setattr(cli, "get_config", lambda: cfg)
  monkeypatch.setattr("sys.argv", ["charliebot", *argv])
  if token is None:
    monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  else:
    monkeypatch.setenv("CHARLIEBOT_RUN_TOKEN", token)
  out, err = io.StringIO(), io.StringIO()
  code = 0
  try:
    with redirect_stdout(out), redirect_stderr(err):
      cli.main()
  except SystemExit as e:
    code = int(e.code or 0)
  return out.getvalue(), err.getvalue(), code


async def test_active_run_token_fixes_the_audience(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, session_id, run_id, token, proc = await _launched_run(tmp_path, monkeypatch, profile="worker")
  try:
    # No --audience: the worker run's token filters to the worker audience.
    out, err, code = _run_cli(monkeypatch, cfg, ["query", "--topic", "beta"], token)
    assert code == 0, err
    assert "worker body" in out
    assert "master body" not in out
    # The same topic through the token cannot see the master-only store slice.
    out, err, code = _run_cli(monkeypatch, cfg, ["query", "--topic", "alpha"], token)
    assert code == 0, err
    assert "master body" not in out
    # A contradictory --audience cannot broaden it.
    out, err, code = _run_cli(
        monkeypatch, cfg, ["query", "--topic", "beta", "--audience", "master"], token)
    assert code == 1
    assert "contradicts" in err
  finally:
    proc.terminate()


async def test_manager_run_token_maps_to_master_audience(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, session_id, run_id, token, proc = await _launched_run(tmp_path, monkeypatch, profile="manager")
  try:
    out, err, code = _run_cli(monkeypatch, cfg, ["query", "--topic", "alpha"], token)
    assert code == 0, err
    assert "master body" in out
    assert "worker body" not in out
    # The manager token cannot pull the worker topic either.
    out, err, code = _run_cli(monkeypatch, cfg, ["query", "--topic", "beta"], token)
    assert code == 0, err
    assert "worker body" not in out
  finally:
    proc.terminate()


async def test_ended_run_token_refuses(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, session_id, run_id, token, proc = await _launched_run(tmp_path, monkeypatch, profile="worker")
  try:
    await tree.runs.record_finish(session_id, run_id, "completed", input_event_ids=[])
    out, err, code = _run_cli(monkeypatch, cfg, ["query", "--topic", "beta"], token)
    assert code == 1
    assert "active run" in err
    assert out == ""  # no operator-grade output leaked
  finally:
    proc.terminate()


async def test_invalid_and_not_launched_tokens_refuse(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, session_id, run_id, token, proc = await _launched_run(tmp_path, monkeypatch, profile="worker")
  try:
    # A token signed with a different key fails verification.
    foreign = sign_run_token(
        RunTokenClaims(session_id=session_id, run_id=run_id, agent="worker"), "other-key")
    out, err, code = _run_cli(monkeypatch, cfg, ["query", "--topic", "beta"], foreign)
    assert code == 1
    assert "invalid run token" in err
    # A run registered but never launched has no process identity to stand for.
    await tree.runs.register_run(RunRecord(id="idle-run", session_id=session_id, kind="work"))
    idle_token = sign_run_token(
        RunTokenClaims(session_id=session_id, run_id="idle-run", agent="worker"), "query-op-key")
    out, err, code = _run_cli(monkeypatch, cfg, ["query", "--topic", "beta"], idle_token)
    assert code == 1
    assert "has not launched" in err
  finally:
    proc.terminate()


async def test_wrong_instance_token_refuses(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, session_id, run_id, token, proc = await _launched_run(tmp_path, monkeypatch, profile="worker")
  try:
    # A token for a session this instance never heard of.
    stranger = sign_run_token(
        RunTokenClaims(session_id="00000000-0000-0000-0000-00000000beef",
                       run_id="r", agent="worker"),
        "query-op-key")
    out, err, code = _run_cli(monkeypatch, cfg, ["query", "--topic", "beta"], stranger)
    assert code == 1
    assert "active run" in err
  finally:
    proc.terminate()


async def test_no_token_keeps_operator_semantics(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, session_id, run_id, token, proc = await _launched_run(tmp_path, monkeypatch, profile="worker")
  try:
    out, err, code = _run_cli(
        monkeypatch, cfg, ["query", "--topic", "alpha", "--audience", "master"], None)
    assert code == 0, err
    assert "master body" in out
    assert "worker body" not in out
    out, err, code = _run_cli(
        monkeypatch, cfg, ["query", "--topic", "alpha", "--audience", "master", "--index"], None)
    assert code == 0, err
    assert "alpha/m1" in out
  finally:
    proc.terminate()

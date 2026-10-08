"""Run-scoped memory query identity: a present CHARLIEBOT_RUN_TOKEN fixes the
audience from the verified, active owning Run's role; every failure mode fails
visibly instead of falling back to operator CLI behavior."""

from __future__ import annotations

import contextlib
import io
import pathlib
import subprocess

import conftest
import pytest

from src.features.memory.store_root import memory_dir as store_memory_dir
from src.infra import constants, models
from src.runtime import run_token, sessions, task_sessions
from src.runtime.session_store import SessionStore

pytestmark = pytest.mark.asyncio


def _write_store(cfg) -> None:
  memory_dir = store_memory_dir(cfg)
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


async def _launched_run(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, *,
                        profile: str) -> tuple[object, task_sessions.TaskTreeManager, str, str, str]:
  cfg = conftest.make_home_config(tmp_path)
  import src.infra.config as core_config
  # The CLI's store root is charliebot_home_dir() / "memory" (env-resolved, config-free):
  # pin CHARLIEBOT_HOME to this config's home so the seeded store and every reader
  # (CLI verbs, run-token audience resolution) see one home.
  monkeypatch.setenv(core_config.CHARLIEBOT_HOME_ENV, str(cfg.charliebot_home))
  core_config._credentials_cache.seed(
      core_config.Credentials(
          path=cfg.charliebot_home / "credentials.yaml", sections={"charliebot": {
              "access_key": "query-op-key"
          }}))
  _write_store(cfg)
  session_mgr = sessions.SessionManager(cfg, SessionStore(cfg))
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  meta = await tree.create_task(
      request_id="r",
      task_parent_id=None,
      profile=profile,
      task=models.TaskSpec(goal="g"),
      name="N",
      backend=None,
      caller=conftest.OPERATOR)
  run_id = "query-run"
  await tree.runs.register_run(models.RunRecord(id=run_id, session_id=meta.id, kind="work"))
  proc = subprocess.Popen(["/bin/sleep", "60"])
  from src.runtime import runs
  pair = runs.read_pid_stat(proc.pid)
  assert pair is not None
  await tree.runs.record_launch(meta.id, run_id, pid=proc.pid, pid_start=pair[0])
  token = run_token.sign_run_token(
      run_token.RunTokenClaims(session_id=meta.id, run_id=run_id, agent="worker"), "query-op-key")
  return cfg, tree, meta.id, run_id, token, proc


def _run_cli(monkeypatch: pytest.MonkeyPatch, argv: list[str], token: str | None) -> tuple[str, str, int]:
  from src.features.memory import cli
  monkeypatch.setattr("sys.argv", ["charliebot", *argv])
  if token is None:
    monkeypatch.delenv(constants.RUN_TOKEN_ENV, raising=False)
  else:
    monkeypatch.setenv(constants.RUN_TOKEN_ENV, token)
  out, err = io.StringIO(), io.StringIO()
  code = 0
  try:
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
      cli.main()
  except SystemExit as e:
    code = int(e.code or 0)
  return out.getvalue(), err.getvalue(), code


async def test_active_run_token_fixes_the_audience(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _tree, _session_id, _run_id, token, proc = await _launched_run(tmp_path, monkeypatch, profile="worker")
  try:
    # No --audience: the worker run's token filters to the worker audience.
    out, err, code = _run_cli(monkeypatch, ["query", "--topic", "beta"], token)
    assert code == 0, err
    assert "worker body" in out
    assert "master body" not in out
    # The same topic through the token cannot see the master-only store slice.
    out, err, code = _run_cli(monkeypatch, ["query", "--topic", "alpha"], token)
    assert code == 0, err
    assert "master body" not in out
    # A contradictory --audience cannot broaden it.
    out, err, code = _run_cli(monkeypatch, ["query", "--topic", "beta", "--audience", "master"], token)
    assert code == 1
    assert "contradicts" in err
  finally:
    proc.terminate()


async def test_unlaunched_run_token_refuses(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, tree, session_id, run_id, token, proc = await _launched_run(tmp_path, monkeypatch, profile="worker")
  try:
    # A fresh Run whose launch identity is never pinned: registered but not
    # launched, the refusal names the missing pin, not an unknown run.
    run_id = run_id + "-unlaunched"
    await tree.runs.register_run(models.RunRecord(id=run_id, session_id=session_id, kind="work"))
    token = run_token.sign_run_token(
        run_token.RunTokenClaims(session_id=session_id, run_id=run_id, agent="worker"), "query-op-key")
    out, err, code = _run_cli(monkeypatch, ["query", "--topic", "beta"], token)
    assert code == 1
    assert "has not launched" in err
    assert out == ""
  finally:
    proc.terminate()


async def test_ended_run_token_refuses(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, tree, session_id, run_id, token, proc = await _launched_run(tmp_path, monkeypatch, profile="worker")
  try:
    await tree.runs.record_finish(session_id, run_id, "completed", input_event_ids=[])
    out, err, code = _run_cli(monkeypatch, ["query", "--topic", "beta"], token)
    assert code == 1
    assert "active run" in err
    assert out == ""  # no operator-grade output leaked
  finally:
    proc.terminate()

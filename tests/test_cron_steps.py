"""Tests for the ``steps`` cron prompt source and its worker chain.

Loader coverage (src/core/config.py): a ``steps`` cron file loads with each
step's resolved body and preserved pointer, colliding prompt sources are load
errors naming the key, and duplicate step names fail. Chain coverage
(src/core/task_chain.py): a fire spawns step 0 pointing its chain_root at
itself, a clean exit advances exactly one step carrying the previous result in
its prompt, a non-zero exit spawns nothing and wakes the master, the last
step's completion wakes once with one block per step and a completion-handler
rerun converges to a no-op, and an empty previous result stops the chain with
a note.
"""

import json
from collections.abc import Callable, Coroutine
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
import yaml
from conftest import (
  OPUS_BACKEND_ID,
  OPUS_BACKEND_OPTION,
  REVIEW_TRIGGER_MASTER_PATCH_TARGET,
  SCHEDULER_GET_CONFIG_PATCH_TARGET,
  SPAWNER_SPAWN_WORKER_PATCH_TARGET,
  build_scheduler_cfg,
  close_create_logged_task,
  read_chat_events,
)

from src.core import event_types as ET
from src.core import task_chain
from src.core.config import (
  ScheduledTaskConfig,
  StepConfig,
  _load_cron_file,
)
from src.core.models import (
  CreateSessionRequest,
  SessionMetadata,
  SpawnRequest,
  TaskType,
  ThreadMetadata,
)
from src.core.scheduler import Scheduler
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

SELECTOR_BODY = "Select memory candidates.\n"
REVIEWER_BODY = "Review the memory diff.\n"
SELECTOR_RESULT = "revise entries/workflow/focus.md plus three proof lines"
REVIEWER_RESULT = "report written to the session artifacts"


async def _noop() -> None:
  return None


# --- (a) loader --------------------------------------------------------------


def _seed_steps_task(cron_dir: Path, tmp_path: Path, extra_body: dict | None = None) -> tuple[Path, Path, Path, str, str]:
  """Write a two-step 'chained' job; returns (yaml, selector_md, reviewer_md, bodies).

  The host file carries the paths to its prompt sources and the pointed files
  own the bodies, exactly as production host files look.
  """
  prompts = tmp_path / "prompts"
  prompts.mkdir(parents=True, exist_ok=True)
  sel_path = prompts / "selector.md"
  sel_body = "Select candidates.\n"
  sel_path.write_text(sel_body, encoding="utf-8")
  rev_path = prompts / "reviewer.md"
  rev_body = "Review the diff.\n"
  rev_path.write_text(rev_body, encoding="utf-8")
  body: dict[str, Any] = {
      "cron": "0 3 * * *",
      "steps": [
          {"name": "selector", "prompt_file": str(sel_path)},
          {"name": "reviewer", "prompt_file": str(rev_path), "backend": "codex-o3"},
      ],
  }
  body.update(extra_body or {})
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  return yaml_path, sel_path, rev_path, sel_body, rev_body


def test_load_cron_file_loads_steps(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = build_scheduler_cfg(tmp_path)
  yaml_path, sel_path, rev_path, sel_body, rev_body = _seed_steps_task(cron_dir, tmp_path)

  task, mtimes = _load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")

  assert task.prompt is None
  assert [s.name for s in task.steps] == ["selector", "reviewer"]
  assert task.steps[0].prompt == sel_body
  assert task.steps[0].prompt_file == str(sel_path)
  assert task.steps[1].prompt == rev_body
  assert task.steps[1].prompt_file == str(rev_path)
  assert task.steps[1].backend == "codex-o3"
  # the hot-reload fingerprint covers both step prompt files
  assert set(mtimes) == {sel_path, rev_path}


def _load_error(cron_dir: Path, tmp_path: Path, extra_body: dict | None = None) -> str:
  cfg = build_scheduler_cfg(tmp_path)
  yaml_path, _sel, _rev, _sel_body, _rev_body = _seed_steps_task(cron_dir, tmp_path, extra_body)
  with pytest.raises(ValueError) as exc_info:
    _load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  return str(exc_info.value)


def test_load_cron_file_rejects_steps_with_prompt_file(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  sel_path = tmp_path / "prompts" / "selector.md"
  error = _load_error(cron_dir, tmp_path, {"prompt_file": str(sel_path)})
  assert "exactly one of" in error and "'prompt'" in error and "'steps'" in error


def test_load_cron_file_rejects_steps_with_handler(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  error = _load_error(cron_dir, tmp_path, {"handler": "backup"})
  assert "exactly one of" in error and "'handler'" in error and "'steps'" in error


def test_load_cron_file_rejects_steps_with_loop(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  error = _load_error(
      cron_dir,
      tmp_path,
      {"loop": {
          "backlog": "backlog/backlog.yaml", "role": "agent", "scope_files": ["src/"]
      }})
  assert "exactly one of" in error and "'loop'" in error and "'steps'" in error


def test_load_cron_file_rejects_steps_with_mode_master(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  error = _load_error(cron_dir, tmp_path, {"mode": "master", "project": "proj"})
  assert "master" in error and "'steps'" in error


def test_load_cron_file_rejects_duplicate_step_names(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = build_scheduler_cfg(tmp_path)
  sel_path = tmp_path / "prompts" / "selector.md"
  sel_path.parent.mkdir(parents=True, exist_ok=True)
  sel_path.write_text("Select.\n", encoding="utf-8")
  body = {
      "cron": "0 3 * * *",
      "steps": [
          {"name": "selector", "prompt_file": str(sel_path)},
          {"name": "selector", "prompt_file": str(sel_path)},
      ],
  }
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  with pytest.raises(ValueError) as exc_info:
    _load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  assert "duplicate step name 'selector'" in str(exc_info.value)


def test_load_cron_file_rejects_empty_steps(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = build_scheduler_cfg(tmp_path)
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump({"cron": "0 3 * * *", "steps": []}), encoding="utf-8")
  with pytest.raises(ValueError):
    _load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")


def test_load_cron_file_rejects_step_with_inline_prompt(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = build_scheduler_cfg(tmp_path)
  body = {
      "cron": "0 3 * * *",
      "steps": [
          {"name": "selector", "prompt": "inline body"},
      ],
  }
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  with pytest.raises(ValueError) as exc_info:
    _load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  error = str(exc_info.value)
  assert "inline 'prompt'" in error and "step" in error


def test_load_cron_file_rejects_step_without_prompt_source(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = build_scheduler_cfg(tmp_path)
  body = {
      "cron": "0 3 * * *",
      "steps": [
          {"name": "selector"},
      ],
  }
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  with pytest.raises(ValueError) as exc_info:
    _load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  assert "step 'selector'" in str(exc_info.value)


# --- (b)-(d) the chain --------------------------------------------------------


def _steps_task_cfg() -> ScheduledTaskConfig:
  return ScheduledTaskConfig(
      name="chained",
      cron="* * * * *",
      backend=OPUS_BACKEND_ID,
      steps=[
          StepConfig(name="selector", prompt=SELECTOR_BODY, prompt_file="prompts/selector.md"),
          StepConfig(
              name="reviewer",
              prompt=REVIEWER_BODY,
              prompt_file="prompts/reviewer.md",
              backend="codex-o3"),
      ],
  )


def _capture_spawn(calls: list[dict[str, Any]]) -> Callable[..., Coroutine[Any, Any, None]]:
  """A spawn_worker stand-in recording each call's kwargs (notably the request)."""

  def fake_spawn_worker(**kwargs: Any) -> Coroutine[Any, Any, None]:
    calls.append(kwargs)
    return _noop()

  return fake_spawn_worker


def _patch_chain_pipes(monkeypatch: pytest.MonkeyPatch, spawns: list[dict[str, Any]]) -> None:
  """Patch the chain's three external seams: the task list, the spawn, and the task handle."""
  monkeypatch.setattr(task_chain, "create_logged_task", close_create_logged_task)
  monkeypatch.setattr(task_chain, "get_scheduled_tasks", lambda: [_steps_task_cfg()])
  monkeypatch.setattr(SPAWNER_SPAWN_WORKER_PATCH_TARGET, _capture_spawn(spawns))


async def _make_chain_session(cfg: Any, session_mgr: SessionManager) -> SessionMetadata:
  return await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: chained", scheduled_task="chained"),
      backend=OPUS_BACKEND_ID)


async def _make_chain_thread(
    thread_mgr: ThreadManager,
    session: SessionMetadata,
    step_name: str,
    chain_root: str | None,
    step_index: int,
) -> ThreadMetadata:
  """Create a chain thread exactly the way spawn_step persists it."""
  thread = await thread_mgr.create_thread(session, f"chained · {step_name}", require_review=False)
  thread.chain_root = chain_root if chain_root is not None else thread.id
  thread.step_index = step_index
  await thread_mgr.save_metadata(thread)
  return thread


def _write_result_events(cfg: Any, session_id: str, thread_id: str, result_text: str) -> None:
  """Stage a thread events log whose last RESULT event carries result_text."""
  events_path = cfg.sessions_dir / session_id / "threads" / thread_id / "data" / "events.jsonl"
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(
      json.dumps({"type": ET.RESULT, "result": result_text}) + "\n",
      encoding="utf-8")


@pytest.mark.asyncio
async def test_fire_spawns_first_step_thread(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)
  spawns: list[dict[str, Any]] = []
  _patch_chain_pipes(monkeypatch, spawns)
  monkeypatch.setattr(SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)

  result = await scheduler._execute_task(_steps_task_cfg())

  assert result["session_id"]
  assert result["thread_id"]
  thread_mgr = ThreadManager(cfg)
  thread = await thread_mgr.get_thread(result["session_id"], result["thread_id"])
  assert thread is not None
  assert thread.description == "chained · selector"
  assert thread.require_review is False
  assert thread.chain_root == thread.id
  assert thread.step_index == 0
  assert len(spawns) == 1
  request = spawns[0]["request"]
  assert request.resolved_backend == OPUS_BACKEND_ID
  assert request.resolved_model == OPUS_BACKEND_OPTION.model
  assert request.prompt_override == SELECTOR_BODY
  assert request.task_type == TaskType.IMPLEMENT
  delegated = [
      e for e in read_chat_events(cfg.charliebot_home, result["session_id"])
      if e.get("type") == ET.TASK_DELEGATED
  ]
  assert len(delegated) == 1
  assert delegated[0]["task"] == "chained"
  assert delegated[0]["description"] == "chained · selector"
  assert delegated[0]["thread_id"] == thread.id
  assert delegated[0]["backend"] == OPUS_BACKEND_ID


@pytest.mark.asyncio
async def test_step_success_spawns_next_step_with_previous_result(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await _make_chain_session(cfg, session_mgr)
  step0 = await _make_chain_thread(thread_mgr, session, "selector", None, 0)
  _write_result_events(cfg, session.id, step0.id, SELECTOR_RESULT)
  spawns: list[dict[str, Any]] = []
  _patch_chain_pipes(monkeypatch, spawns)
  master = AsyncMock()
  monkeypatch.setattr(REVIEW_TRIGGER_MASTER_PATCH_TARGET, master)

  owned = await task_chain.handle_step_completion(
      session.id, step0, 0, "(events summary)", "(full summary)", thread_mgr, session_mgr, cfg)

  assert owned is True
  assert len(spawns) == 1
  request: SpawnRequest = spawns[0]["request"]
  assert request.resolved_backend == "codex-o3"
  assert request.resolved_model == "o3"
  assert request.prompt_override.startswith(REVIEWER_BODY)
  assert f"## Result of the previous step (selector)\n{SELECTOR_RESULT}" in request.prompt_override
  next_thread = await thread_mgr.get_thread(session.id, spawns[0]["thread_id"])
  assert next_thread is not None and next_thread.id != step0.id
  assert next_thread.chain_root == step0.id
  assert next_thread.step_index == 1
  master.assert_not_awaited()

  # A rerun of the same completion (finalize rerun) spawns nothing more.
  again = await task_chain.handle_step_completion(
      session.id, step0, 0, "(events summary)", "(full summary)", thread_mgr, session_mgr, cfg)
  assert again is True
  assert len(spawns) == 1
  master.assert_not_awaited()


@pytest.mark.asyncio
async def test_step_failure_spawns_nothing_and_wakes_master(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await _make_chain_session(cfg, session_mgr)
  step0 = await _make_chain_thread(thread_mgr, session, "selector", None, 0)
  _write_result_events(cfg, session.id, step0.id, SELECTOR_RESULT)
  spawns: list[dict[str, Any]] = []
  _patch_chain_pipes(monkeypatch, spawns)
  master = AsyncMock()
  monkeypatch.setattr(REVIEW_TRIGGER_MASTER_PATCH_TARGET, master)

  owned = await task_chain.handle_step_completion(
      session.id, step0, 1, "(events summary)", "(full summary)", thread_mgr, session_mgr, cfg)

  assert owned is True
  assert not spawns
  master.assert_awaited_once()
  summary = master.await_args.args[1]
  assert f"**selector result:**\n{SELECTOR_RESULT}" in summary


@pytest.mark.asyncio
async def test_last_step_completion_wakes_master_once_with_block_per_step(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await _make_chain_session(cfg, session_mgr)
  step0 = await _make_chain_thread(thread_mgr, session, "selector", None, 0)
  step1 = await _make_chain_thread(thread_mgr, session, "reviewer", step0.id, 1)
  _write_result_events(cfg, session.id, step0.id, SELECTOR_RESULT)
  _write_result_events(cfg, session.id, step1.id, REVIEWER_RESULT)
  spawns: list[dict[str, Any]] = []
  _patch_chain_pipes(monkeypatch, spawns)
  master = AsyncMock()
  monkeypatch.setattr(REVIEW_TRIGGER_MASTER_PATCH_TARGET, master)

  owned = await task_chain.handle_step_completion(
      session.id, step1, 0, "(events summary)", "(full summary)", thread_mgr, session_mgr, cfg)

  assert owned is True
  assert not spawns
  master.assert_awaited_once()
  summary = master.await_args.args[1]
  assert f"**selector result:**\n{SELECTOR_RESULT}" in summary
  assert f"**reviewer result:**\n{REVIEWER_RESULT}" in summary
  assert summary.index("**selector result:**") < summary.index("**reviewer result:**")

  # The persisted wake state (terminal summary followed by master output) makes
  # a rerun of the completion handler converge: no spawn, no second wake.
  await session_mgr.persist_and_broadcast(
      session.id, {"type": ET.WORKER_SUMMARY, "thread_id": step1.id, "status": "completed"})
  await session_mgr.persist_and_broadcast(
      session.id, {"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": "done"}]}})
  again = await task_chain.handle_step_completion(
      session.id, step1, 0, "(events summary)", "(full summary)", thread_mgr, session_mgr, cfg)
  assert again is True
  master.assert_awaited_once()
  assert not spawns


@pytest.mark.asyncio
async def test_empty_previous_result_stops_chain_and_notes_it(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await _make_chain_session(cfg, session_mgr)
  step0 = await _make_chain_thread(thread_mgr, session, "selector", None, 0)
  spawns: list[dict[str, Any]] = []
  _patch_chain_pipes(monkeypatch, spawns)
  master = AsyncMock()
  monkeypatch.setattr(REVIEW_TRIGGER_MASTER_PATCH_TARGET, master)

  owned = await task_chain.handle_step_completion(
      session.id, step0, 0, "(events summary)", "(full summary)", thread_mgr, session_mgr, cfg)

  assert owned is True
  assert not spawns
  master.assert_awaited_once()
  summary = master.await_args.args[1]
  assert "produced no result" in summary
  assert "(no result)" in summary
  assert [t.id for t in await thread_mgr.list_threads(session.id)] == [step0.id]

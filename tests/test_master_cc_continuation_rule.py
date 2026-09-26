"""The v1 continuation rule: a held native id resumes only inside its producer's
continuation domain; a cross-family turn starts a fresh native conversation,
carries the context-reset note (when the session has a completed round of its
own), and never hands one backend's id to another. The round-end funnel lands
the id and its producing backend together."""

from __future__ import annotations

from collections.abc import AsyncIterator
from dataclasses import replace
from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    TerminateFlagBackend,
    backend_option,
    drain_session_consumer,
    fresh_master_state,
    make_transcript,
    make_work_item,
    mock_session_callbacks,
    patch_instructions_content,
    pool_cfg,
)

from src.agents import master_cc, master_cc_queue, master_cc_run
from src.agents.backends import base as backend_base
from src.core import event_types as ET
from src.core.config import CLAUDE_CONFIG_DIR_ENV_VAR, CharlieBotConfig
from src.core.models import CreateSessionRequest, SessionCallbacks, SessionMetadata
from src.core.sessions import HISTORY_LOCATION_NOTE, SessionManager

# The two v1 start-note lines exactly as Required Behavior item 3 spells them;
# both carry HISTORY_LOCATION_NOTE (its one home, src/core/sessions.py).
RULE_NOTE = (
    "[Context reset: this session switched from backend claude-opus-5 to codex-o3, "
    "which starts its own conversation. Earlier turns' history remains readable in "
    "this session's chat log, data/chat_events.jsonl in the working directory.]")
DROP_NOTE = (
    "[Context reset: the previous conversation could not be resumed. Earlier turns' "
    "history remains readable in this session's chat log, data/chat_events.jsonl in "
    "the working directory.]")


def _rule_cfg(tmp_path: Path) -> CharlieBotConfig:
  """One Claude family (two models, one login dir) plus one Codex option: the
  minimal config the cross-family rule needs."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={
          "options":
              [
                  backend_option(id="claude-opus-5", label="Opus 5", type="cc-claude", model="claude-opus-5"),
                  backend_option(id="claude-fable-5", label="Fable 5", type="cc-claude", model="claude-fable-5"),
                  backend_option(id="codex-o3", label="Codex", type="codex", model="o3"),
              ]
      },
  )


class _ScriptedBackend(TerminateFlagBackend):
  """Backend double: records the prompt of every run() call and yields one
  scripted session id (or none), then a clean, non-zero-usage result."""

  exit_code = 0
  stderr_text = ""

  def __init__(self, cc_session_id: str | None, record: dict) -> None:
    self._cc_session_id = cc_session_id
    self._record = record

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    self._record["prompt"] = prompt
    result = backend_base.make_result_event(input_tokens=10, output_tokens=5)
    if self._cc_session_id is not None:
      result["session_id"] = self._cc_session_id
    yield result


def _scripted_build(landing: dict[str, list[str | None]], log: list[dict]):
  """A build_backend double driven by *landing*: option id -> the session ids
  that backend's rounds land, in order (exhausted script lands none). Every
  construction appends one record to *log*: the option id, the resume wiring
  the turn handed the backend, and (filled at run time) the prompt."""

  def build(option, cfg, **kwargs):
    ids = landing.get(option.id, [])
    cc_id = ids.pop(0) if ids else None
    record: dict = {
        "backend_id": option.id,
        "resume_session_id": kwargs.get("resume_session_id"),
        "extra_flags": kwargs.get("extra_flags"),
        "prompt": None,
    }
    log.append(record)
    return _ScriptedBackend(cc_id, record)

  return build


def _callbacks(*, completed_round: bool) -> SessionCallbacks:
  """Mocked callbacks with has_completed_round pinned to *completed_round*."""
  return replace(mock_session_callbacks(), has_completed_round=AsyncMock(return_value=completed_round))


async def _seed(
    session_mgr: SessionManager,
    name: str,
    *,
    backend: str,
    cc: str | None = None,
    native: str | None = None,
    completed: bool = False,
) -> SessionMetadata:
  """One session seeded through the authorized channels; native=None with cc
  set is the pre-rule shape (a held id with no recorded producer)."""
  session = await session_mgr.create_session(CreateSessionRequest(name=name), backend=backend)
  if cc is not None:
    await session_mgr.persist_cc_session_id(session.id, cc, native_backend=native)
  if completed:
    await session_mgr.save_chat_event(session.id, {"type": ET.MASTER_DONE, "exit_code": 0})
    session_mgr._chat_events.clear_cache(session.id)
  return session


# ---------------------------------------------------------------------------
# Direct _run_cc rig: the turn-start judgment and the note
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_pre_rule_session_resumes_as_today_on_codex(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(a) A pre-rule session (a held id, no recorded producer) on a codex option
  resumes the id as before; no note, no drop."""
  cfg = _rule_cfg(tmp_path)
  session_meta = SessionMetadata(id="session-id", name="S", backend="codex-o3", cc_session_id="c1")
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"codex-o3": [None]}, log))
  patch_instructions_content(monkeypatch)

  item = make_work_item(
      cfg,
      session_meta,
      cfg.get_backend_option("codex-o3"),
      user_content="hello",
      callbacks=_callbacks(completed_round=True))
  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 0 and error_msg is None
  assert log[0]["resume_session_id"] == "c1"
  assert log[0]["prompt"] == "hello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED] == []


@pytest.mark.asyncio
async def test_pre_rule_session_resumes_as_today_on_claude_with_transcript(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(a) The same pre-rule session on a cc-claude option with its transcript
  present resumes through --resume; no note, no drop."""
  cfg = _rule_cfg(tmp_path)
  config_dir = tmp_path / "login-dir"
  make_transcript(config_dir, "c1")
  monkeypatch.setenv(CLAUDE_CONFIG_DIR_ENV_VAR, str(config_dir))
  session_meta = SessionMetadata(id="session-id", name="S", backend="claude-opus-5", cc_session_id="c1")
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"claude-opus-5": [None]}, log))
  patch_instructions_content(monkeypatch)

  item = make_work_item(
      cfg,
      session_meta,
      cfg.get_backend_option("claude-opus-5"),
      user_content="hello",
      callbacks=_callbacks(completed_round=True))
  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 0 and error_msg is None
  assert log[0]["resume_session_id"] is None
  assert log[0]["extra_flags"] == ["--resume", "c1", "--exclude-dynamic-system-prompt-sections"]
  assert log[0]["prompt"] == "hello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED] == []


@pytest.mark.asyncio
async def test_same_domain_claude_model_change_resumes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(d) A same-domain Claude model change (the recorded producer shares the
  login dir) resumes the id; no note, no drop."""
  cfg = _rule_cfg(tmp_path)
  config_dir = tmp_path / "login-dir"
  make_transcript(config_dir, "c1")
  monkeypatch.setenv(CLAUDE_CONFIG_DIR_ENV_VAR, str(config_dir))
  session_meta = SessionMetadata(
      id="session-id", name="S", backend="claude-fable-5", cc_session_id="c1", native_backend="claude-opus-5")
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"claude-fable-5": [None]}, log))
  patch_instructions_content(monkeypatch)

  item = make_work_item(
      cfg,
      session_meta,
      cfg.get_backend_option("claude-fable-5"),
      user_content="hello",
      callbacks=_callbacks(completed_round=True))
  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 0 and error_msg is None
  assert log[0]["extra_flags"] == ["--resume", "c1", "--exclude-dynamic-system-prompt-sections"]
  assert log[0]["prompt"] == "hello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED] == []


@pytest.mark.asyncio
async def test_cross_family_without_completed_round_starts_fresh_silently(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(f) A cross-family turn on a session without a completed round of its own:
  fresh, no resume id, and no note."""
  cfg = _rule_cfg(tmp_path)
  session_meta = SessionMetadata(
      id="session-id", name="S", backend="codex-o3", cc_session_id="c1", native_backend="claude-opus-5")
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"codex-o3": [None]}, log))
  patch_instructions_content(monkeypatch)

  item = make_work_item(
      cfg,
      session_meta,
      cfg.get_backend_option("codex-o3"),
      user_content="hello",
      callbacks=_callbacks(completed_round=False))
  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 0 and error_msg is None
  assert log[0]["resume_session_id"] is None
  assert log[0]["prompt"] == "hello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED] == []


@pytest.mark.asyncio
async def test_transcript_missing_with_completed_round_drops_and_notes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(g) The transcript-missing path with a completed round still emits
  resume_context_dropped, and the prompt opens with the could-not-be-resumed
  note."""
  cfg = _rule_cfg(tmp_path)
  monkeypatch.setenv(CLAUDE_CONFIG_DIR_ENV_VAR, str(tmp_path / "empty-login-dir"))
  session_meta = SessionMetadata(
      id="session-id", name="S", backend="claude-fable-5", cc_session_id="c1", native_backend="claude-opus-5")
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"claude-fable-5": [None]}, log))
  patch_instructions_content(monkeypatch)

  item = make_work_item(
      cfg,
      session_meta,
      cfg.get_backend_option("claude-fable-5"),
      user_content="hello",
      callbacks=_callbacks(completed_round=True))
  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 0 and error_msg is None
  assert log[0]["extra_flags"] == ["--exclude-dynamic-system-prompt-sections"]
  assert log[0]["prompt"] == f"{DROP_NOTE}\n\nhello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  dropped = [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED]
  assert [d["reason"] for d in dropped] == ["transcript_missing"]
  assert HISTORY_LOCATION_NOTE in DROP_NOTE


def _pooled_rule_cfg(tmp_path: Path) -> CharlieBotConfig:
  """_rule_cfg with a one-account pool declared: every cc-claude option turns
  pooled, so a cross-family switch into Claude runs the pool placement branch."""
  return pool_cfg(
      tmp_path,
      [
          backend_option(id="claude-opus-5", label="Opus 5", type="cc-claude", model="claude-opus-5"),
          backend_option(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
      home=tmp_path / ".charliebot",
      worktree_dir=tmp_path / "worktrees",
      labels=("main",),
  )


@pytest.mark.asyncio
async def test_cross_family_into_pooled_claude_keeps_the_held_id_withheld(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(e, pooled) A fresh-by-switch turn on a pooled cc-claude option keeps its
  withheld resume id through the pool placement branch: the post-placement
  re-resolve must not hand the held id — even one a pool login still finds —
  to the new family."""
  cfg = _pooled_rule_cfg(tmp_path)
  # The held id's transcript still sits in the pool login (left over from
  # before its producer left the config): reachable bytes must not resurrect
  # a resume the rule forbids.
  make_transcript(Path(cfg.accounts.claude[0].config_dir), "c1")
  session_meta = SessionMetadata(
      id="session-id", name="S", backend="claude-opus-5", cc_session_id="c1", native_backend="codex-o3")
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"claude-opus-5": ["c2"]}, log))
  patch_instructions_content(monkeypatch)

  item = make_work_item(
      cfg,
      session_meta,
      cfg.get_backend_option("claude-opus-5"),
      user_content="hello",
      callbacks=_callbacks(completed_round=True))
  cc_id, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert exit_code == 0 and error_msg is None
  assert cc_id == "c2", "the fresh round adopts the id its own backend produced"
  assert log[0]["resume_session_id"] is None
  assert log[0]["extra_flags"] == ["--exclude-dynamic-system-prompt-sections"
                                  ], ("the pool's re-resolve must not re-hand the held id to the new family")
  assert log[0]["prompt"].startswith("[Context reset: this session switched from backend codex-o3 to claude-opus-5")
  assert extras["native_backend"] == "claude-opus-5"


# ---------------------------------------------------------------------------
# Through the consumer: round-end persistence of id + producer
# ---------------------------------------------------------------------------


def _consumer_patches(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """The seams every real-_run_cc consumer round needs: scripted build target
  set by the caller; tex skipped, broadcasts silenced."""
  monkeypatch.setattr(master_cc_queue, "get_tex_path", lambda: tmp_path / "missing.tex")
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", AsyncMock())


@pytest.mark.asyncio
async def test_round_lands_id_and_native_backend_together(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(b) A round on backend X returning id "c2" lands cc_session_id="c2" and
  native_backend=X in one anchor write; a cold-cache reader sees both."""
  cfg = _rule_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="round-lands"), backend="codex-o3")
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"codex-o3": ["c2"]}, log))
  _consumer_patches(monkeypatch, tmp_path)
  patch_instructions_content(monkeypatch)

  async with fresh_master_state(session.id):
    result = await master_cc.run_message(cfg, session, "hi", session_mgr.callbacks(), skip_user_event=True)
    assert result == "c2"
    await drain_session_consumer(session.id, timeout=5)

  cold_reader = SessionManager(cfg)
  meta = await cold_reader.get_session(session.id)
  assert meta is not None
  assert meta.cc_session_id == "c2"
  assert meta.native_backend == "codex-o3"


@pytest.mark.asyncio
async def test_same_codex_backend_consecutive_turns_resume(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(c) The same codex backend on consecutive turns resumes the id its own
  round produced — a non-Claude backend's continuation domain is its own id."""
  cfg = _rule_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="consecutive"), backend="codex-o3")
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"codex-o3": ["cx-1"]}, log))
  _consumer_patches(monkeypatch, tmp_path)
  patch_instructions_content(monkeypatch)

  async with fresh_master_state(session.id):
    await master_cc.run_message(cfg, session, "one", session_mgr.callbacks(), skip_user_event=True)
    await drain_session_consumer(session.id, timeout=5)
    meta = await session_mgr.read_metadata_fresh(session.id)
    assert meta.cc_session_id == "cx-1" and meta.native_backend == "codex-o3"
    await master_cc.run_message(cfg, meta, "two", session_mgr.callbacks(), skip_user_event=True)
    await drain_session_consumer(session.id, timeout=5)

  assert [entry["backend_id"] for entry in log] == ["codex-o3", "codex-o3"]
  assert log[0]["resume_session_id"] is None
  assert log[1]["resume_session_id"] == "cx-1", "the same codex backend resumes its own id"


async def _run_consumer_round(
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    meta: SessionMetadata,
    content: str,
    *,
    skip_user_event: bool = True,
) -> None:
  """One full round through run_message and the consumer, drained."""
  async with fresh_master_state(meta.id):
    await master_cc.run_message(cfg, meta, content, session_mgr.callbacks(), skip_user_event=skip_user_event)
    await drain_session_consumer(meta.id, timeout=5)


@pytest.mark.asyncio
async def test_cross_family_with_completed_round_starts_fresh_with_note(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(e) Cross-family with a completed round: no resume id reaches the backend,
  no drop event, the prompt opens with the exact switch note; a round that
  lands no id leaves the old id and producer on disk (the next turn is fresh
  again with the note again), and a round that lands a new id replaces both."""
  cfg = _rule_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await _seed(
      session_mgr, "cross-family", backend="codex-o3", cc="c1", native="claude-opus-5", completed=True)
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"codex-o3": [None, None, "c2"]}, log))
  _consumer_patches(monkeypatch, tmp_path)
  patch_instructions_content(monkeypatch)

  # Round 1 carries a real user event: the persisted event must stay
  # note-free — only the prompt sent to the backend carries the note.
  meta = await session_mgr.read_metadata_fresh(session.id)
  await _run_consumer_round(cfg, session_mgr, meta, "first", skip_user_event=False)
  assert log[0]["resume_session_id"] is None
  assert log[0]["prompt"] == f"{RULE_NOTE}\n\nfirst"
  disk = await session_mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id == "c1" and disk.native_backend == "claude-opus-5", (
      "a fresh round that lands no id leaves the old id and producer on disk")
  persisted_user = [
      e for e in session_mgr.load_chat_events_sync(session.id) if e.get("type") == ET.USER and e.get("content")
  ]
  assert [e["content"] for e in persisted_user] == ["first"]

  # Round 2: the same judgment runs again — fresh, note again.
  meta = await session_mgr.read_metadata_fresh(session.id)
  await _run_consumer_round(cfg, session_mgr, meta, "second")
  assert log[1]["resume_session_id"] is None
  assert log[1]["prompt"] == f"{RULE_NOTE}\n\nsecond"
  disk = await session_mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id == "c1" and disk.native_backend == "claude-opus-5"

  # Round 3: still judged fresh, but this round lands a new id — both fields
  # are replaced together.
  meta = await session_mgr.read_metadata_fresh(session.id)
  await _run_consumer_round(cfg, session_mgr, meta, "third")
  assert log[2]["resume_session_id"] is None
  assert log[2]["prompt"] == f"{RULE_NOTE}\n\nthird"
  disk = await session_mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id == "c2" and disk.native_backend == "codex-o3"

  # Not one resume_context_dropped fired: the rule's fresh turn skips the
  # pre-flight, and the id never left its producer's hands.
  events = session_mgr.load_chat_events_sync(session.id)
  assert [e for e in events if e.get("type") == ET.RESUME_CONTEXT_DROPPED] == []


@pytest.mark.asyncio
async def test_three_segment_claude_codex_claude_keeps_ids_with_their_producers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(h) Claude, then Codex, then Claude through the consumer: every round's
  resume wiring is either absent or its own backend's id — never the other
  backend's — and the disk after each round names the id's producer."""
  cfg = _rule_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="three-segments"), backend="claude-opus-5")
  log: list[dict] = []
  monkeypatch.setattr(
      BUILD_BACKEND_PATCH_TARGET, _scripted_build({
          "claude-opus-5": ["cc-1", "cc-2"],
          "codex-o3": ["cx-1"],
      }, log))
  _consumer_patches(monkeypatch, tmp_path)
  patch_instructions_content(monkeypatch)

  # Segment 1: Claude, fresh session — no id to resume, no note (no completed
  # round of its own), and the round lands cc-1 under claude-opus-5.
  meta = await session_mgr.read_metadata_fresh(session.id)
  await _run_consumer_round(cfg, session_mgr, meta, "one")
  assert log[0]["backend_id"] == "claude-opus-5"
  assert log[0]["extra_flags"] == ["--exclude-dynamic-system-prompt-sections"]
  assert log[0]["prompt"] == "one"
  disk = await session_mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id == "cc-1" and disk.native_backend == "claude-opus-5"

  # Switch to Codex. Segment 2: cross-family, completed round exists — fresh
  # turn with the switch note; the round lands cx-1 under codex-o3.
  await session_mgr.switch_backend(session.id, "codex-o3")
  meta = await session_mgr.read_metadata_fresh(session.id)
  assert meta.native_backend == "claude-opus-5"
  await _run_consumer_round(cfg, session_mgr, meta, "two")
  assert log[1]["backend_id"] == "codex-o3"
  assert log[1]["resume_session_id"] is None
  assert log[1]["prompt"] == f"{RULE_NOTE}\n\ntwo"
  disk = await session_mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id == "cx-1" and disk.native_backend == "codex-o3"

  # Switch back to Claude. Segment 3: cross-family again — the claude round
  # must not receive the codex id through --resume, and lands cc-2.
  await session_mgr.switch_backend(session.id, "claude-opus-5")
  meta = await session_mgr.read_metadata_fresh(session.id)
  assert meta.native_backend == "codex-o3"
  await _run_consumer_round(cfg, session_mgr, meta, "three")
  assert log[2]["backend_id"] == "claude-opus-5"
  assert log[2]["extra_flags"] == ["--exclude-dynamic-system-prompt-sections"
                                  ], ("the claude round must not resume the codex-produced id")
  assert "switched from backend codex-o3 to claude-opus-5" in log[2]["prompt"]
  assert log[2]["prompt"].startswith("[Context reset: this session switched from backend codex-o3 to claude-opus-5")
  disk = await session_mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id == "cc-2" and disk.native_backend == "claude-opus-5"

  events = session_mgr.load_chat_events_sync(session.id)
  assert [e for e in events if e.get("type") == ET.RESUME_CONTEXT_DROPPED] == []


@pytest.mark.asyncio
async def test_master_cc_run_reports_the_backend_it_ran_on(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """finish_extras carries the option id the round actually ran on — the
  consumer's producing-backend input, not the request's hint."""
  cfg = _rule_cfg(tmp_path)
  session_meta = SessionMetadata(id="session-id", name="S", backend="codex-o3")
  log: list[dict] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _scripted_build({"codex-o3": ["cx-9"]}, log))
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, cfg.get_backend_option("codex-o3"), user_content="hello")
  _cc, _exit, _error, extras = await master_cc_run._run_cc(item)

  assert extras["native_backend"] == "codex-o3"

"""The v1 continuation rule: a held native id resumes only inside its producer's
continuation domain; a cross-family turn starts a fresh native conversation,
carries the context-reset note (when the session has a completed round of its
own), and never hands one backend's id to another. The round-end funnel lands
the id and its producing backend together."""

from __future__ import annotations

import dataclasses
import pathlib
from collections.abc import AsyncIterator
from unittest import mock

import conftest
import pytest

from src.agents import master_cc_run, master_cc_state
from src.agents.backends import base as backend_base
from src.core import config, models, sessions
from src.core import event_types as ET


def _rule_cfg(tmp_path: pathlib.Path) -> config.CharlieBotConfig:
  """One Claude family (two models, one login dir) plus one Codex option: the
  minimal config the cross-family rule needs."""
  return config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={
          "options":
              [
                  conftest.backend_option(id="claude-opus-5", label="Opus 5", type="cc-claude", model="claude-opus-5"),
                  conftest.backend_option(
                      id="claude-fable-5", label="Fable 5", type="cc-claude", model="claude-fable-5"),
                  conftest.backend_option(id="codex-o3", label="Codex", type="codex", model="o3"),
              ]
      },
  )


class _ScriptedBackend(conftest.TerminateFlagBackend):
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


def _callbacks(*, completed_round: bool) -> models.SessionCallbacks:
  """Mocked callbacks with has_completed_round pinned to *completed_round*."""
  return dataclasses.replace(
      conftest.mock_session_callbacks(), has_completed_round=mock.AsyncMock(return_value=completed_round))


async def _run_scripted_turn(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    backend_id: str,
    native_backend: str | None,
    login_dir: str | None,
    completed_round: bool,
) -> tuple[master_cc_state._WorkItem, list[dict]]:
  """One scripted continuation-rule turn through _run_cc: the rule config, a session holding
  native id c1, the build double scripted to land no fresh id, and a work item whose user
  content is "hello". Returns (item, log); a non-zero exit or error fails here, so every
  caller's assertions read a completed turn.

  login_dir pins the cc-claude login directory the turn sees: "transcript" creates it with a
  transcript for c1, "empty" creates it without one, and None leaves no directory (codex
  turns never read one).
  """
  cfg = _rule_cfg(tmp_path)
  if login_dir is not None:
    config_dir = tmp_path / "login-dir"
    if login_dir == "transcript":
      conftest.make_transcript(config_dir, "c1")
    elif login_dir == "empty":
      config_dir.mkdir(parents=True)
    else:
      raise AssertionError(f"unknown login_dir state: {login_dir}")
    monkeypatch.setenv(config.CLAUDE_CONFIG_DIR_ENV_VAR, str(config_dir))
  session_meta = models.SessionMetadata(
      id="session-id", name="S", backend=backend_id, cc_session_id="c1", native_backend=native_backend)
  log: list[dict] = []
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, _scripted_build({backend_id: [None]}, log))
  conftest.patch_instructions_content(monkeypatch)

  item = conftest.make_work_item(
      cfg,
      session_meta,
      cfg.get_backend_option(backend_id),
      user_content="hello",
      callbacks=_callbacks(completed_round=completed_round))
  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)
  assert exit_code == 0 and error_msg is None
  return item, log


# ---------------------------------------------------------------------------
# Direct _run_cc rig: the turn-start judgment and the note
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_pre_rule_session_resumes_as_today_on_codex(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(a) A pre-rule session (a held id, no recorded producer) on a codex option
  resumes the id as before; no note, no drop."""
  item, log = await _run_scripted_turn(
      tmp_path, monkeypatch, backend_id="codex-o3", native_backend=None, login_dir=None, completed_round=True)

  assert log[0]["resume_session_id"] == "c1"
  assert log[0]["prompt"] == "hello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED] == []


@pytest.mark.asyncio
async def test_pre_rule_session_resumes_as_today_on_claude_with_transcript(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(a) The same pre-rule session on a cc-claude option with its transcript
  present resumes through --resume; no note, no drop."""
  item, log = await _run_scripted_turn(
      tmp_path,
      monkeypatch,
      backend_id="claude-opus-5",
      native_backend=None,
      login_dir="transcript",
      completed_round=True)

  assert log[0]["resume_session_id"] is None
  assert log[0]["extra_flags"] == ["--resume", "c1", "--exclude-dynamic-system-prompt-sections"]
  assert log[0]["prompt"] == "hello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED] == []


@pytest.mark.asyncio
async def test_cross_family_without_completed_round_starts_fresh_silently(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(f) A cross-family turn on a session without a completed round of its own:
  fresh, no resume id, and no note."""
  item, log = await _run_scripted_turn(
      tmp_path,
      monkeypatch,
      backend_id="codex-o3",
      native_backend="claude-opus-5",
      login_dir=None,
      completed_round=False)

  assert log[0]["resume_session_id"] is None
  assert log[0]["prompt"] == "hello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED] == []


# ---------------------------------------------------------------------------
# The note itself and the v1 turn-start notes
# ---------------------------------------------------------------------------


def test_context_reset_note_assembles_history_note_and_instruction() -> None:
  """The whole bracketed note, with and without a task goal: the reason, the
  shared history note, and the clone-style read-the-log instruction."""
  without_goal = (
      "[Context reset: this session switched from backend claude-sonnet-5 to codex-gpt-luna, "
      "which starts its own conversation. "
      f"{sessions.HISTORY_LOCATION_NOTE} {sessions.CONTEXT_RESET_INSTRUCTION}]")
  assert sessions.context_reset_note(
      "this session switched from backend claude-sonnet-5 to codex-gpt-luna, "
      "which starts its own conversation") == without_goal
  with_goal = (
      "[Context reset: this task's managed instructions changed. The task is: ship the parser. "
      f"{sessions.HISTORY_LOCATION_NOTE} {sessions.CONTEXT_RESET_INSTRUCTION}]")
  assert sessions.context_reset_note(
      "this task's managed instructions changed", task_goal="ship the parser") == with_goal


@pytest.mark.asyncio
async def test_cross_family_switch_note_carries_the_instruction(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A cc-claude session switched to a codex option: the turn starts a fresh
  native conversation whose prompt opens with the full reset note, instruction
  included."""
  _item, log = await _run_scripted_turn(
      tmp_path,
      monkeypatch,
      backend_id="codex-o3",
      native_backend="claude-opus-5",
      login_dir=None,
      completed_round=True)

  assert log[0]["resume_session_id"] is None
  note, sep, tail = log[0]["prompt"].partition("\n\n")
  assert sep and tail == "hello"
  assert note == (
      "[Context reset: this session switched from backend claude-opus-5 to codex-o3, "
      f"which starts its own conversation. {sessions.HISTORY_LOCATION_NOTE} {sessions.CONTEXT_RESET_INSTRUCTION}]")


@pytest.mark.asyncio
async def test_dropped_resume_note_carries_the_instruction(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The could-not-be-resumed path (same producer, transcript gone): the note
  names the dropped resume and still carries the instruction."""
  _item, log = await _run_scripted_turn(
      tmp_path,
      monkeypatch,
      backend_id="claude-opus-5",
      native_backend="claude-opus-5",
      login_dir="empty",
      completed_round=True)

  note, sep, tail = log[0]["prompt"].partition("\n\n")
  assert sep and tail == "hello"
  assert note == (
      "[Context reset: the previous conversation could not be resumed. "
      f"{sessions.HISTORY_LOCATION_NOTE} {sessions.CONTEXT_RESET_INSTRUCTION}]")


@pytest.mark.asyncio
async def test_switch_back_to_producer_before_sending_resumes_without_note(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A session switched away and back before the next message: the recorded
  producer equals the current backend, so the native conversation continues
  and no note is added."""
  item, log = await _run_scripted_turn(
      tmp_path, monkeypatch, backend_id="codex-o3", native_backend="codex-o3", login_dir=None, completed_round=True)

  assert log[0]["resume_session_id"] == "c1"
  assert log[0]["prompt"] == "hello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED] == []


@pytest.mark.asyncio
async def test_same_login_model_switch_resumes_without_note(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Two cc-claude options over one login directory (the claude-sonnet-5 to
  claude-opus-5 shape) share one continuation domain: the held conversation
  resumes across the model switch and no note is added."""
  item, log = await _run_scripted_turn(
      tmp_path,
      monkeypatch,
      backend_id="claude-fable-5",
      native_backend="claude-opus-5",
      login_dir="transcript",
      completed_round=True)

  assert log[0]["extra_flags"] == ["--resume", "c1", "--exclude-dynamic-system-prompt-sections"]
  assert log[0]["prompt"] == "hello"
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED] == []

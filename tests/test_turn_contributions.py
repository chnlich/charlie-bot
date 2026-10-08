"""Turn contributions: the registry's contract, and the runtime call sites that ask it.

The runtime turn path names no feature. Each feature package registers one TurnContribution, and the path
asks every registered contribution at its step: the instruction build (rule file, segments), the queue
(before a turn), the run (context window), the session funnel (after a MASTER_DONE) and the chat aggregator
(event renderers). The tests drive each call site through the real registrations that conftest makes.
"""

from __future__ import annotations

import asyncio
import subprocess
import sys
import textwrap
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import conftest
import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    BUILD_BACKEND_PATCH_TARGET,
    ScriptedRelayBackend,
    build_master_cc_cfg,
    fresh_master_state,
    install_scripted_backends,
    make_task_spawner,
    make_work_item,
    patch_instructions_content,
)

from src.features.chat_threads import thread_sessions
from src.features.discord.metadata import DiscordOrigin
from src.features.latex import latex
from src.features.latex.turn_contribution import LatexTurnContribution
from src.features.slack.metadata import SlackOrigin
from src.infra import event_types as ET
from src.infra.models import CreateSessionRequest, SessionMetadata
from src.runtime import master_cc_queue, master_cc_run, master_cc_state, message_aggregator, streaming
from src.runtime.agent_process.base import make_result_event
from src.runtime.hooks import turn_contributions
from src.runtime.sessions import SessionManager
from src.runtime.task_prompts import build_segments

SLACK_ORIGIN = SlackOrigin(team_id="T1", channel_id="C1", thread_ts="1700000000.000100")
DISCORD_ORIGIN = DiscordOrigin(guild_id="G1", parent_channel_id="C1", thread_id="1234567890")
SESSION_KINDS = {
    "slack": ("thread_session.md", {
        "slack_origin": SLACK_ORIGIN
    }),
    "discord": ("thread_session.md", {
        "discord_origin": DISCORD_ORIGIN
    }),
    "plain": ("manager_workflows.md", {}),
}


def real_repo_cfg(home: Path) -> SimpleNamespace:
  """Instruction inputs over this checkout's real prompts tree; the host override and memory store are absent."""
  return SimpleNamespace(
      charlie_bot_repo=conftest.ROOT,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      memory_dir=home / "memory",
      charliebot_home=home,
  )


def prompts_text(filename: str, session_id: str) -> str:
  return (conftest.ROOT / "prompts" / filename).read_text(encoding="utf-8").replace("{{session_id}}", session_id)


# ---------------------------------------------------------------------------
# The workflow rules file: a thread root gets its brief on the task path and in the v1 builder
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("kind", SESSION_KINDS)
def test_manager_root_task_path_reads_the_rule_file_of_its_kind(kind: str, tmp_path: Path) -> None:
  rule_file, origin = SESSION_KINDS[kind]
  meta = SessionMetadata(id="root-1", name="root", **origin)

  segments, _ = build_segments(
      real_repo_cfg(tmp_path / "home"), meta, "manager_turn", overlay=None, chain=(), node_ref=None)

  refs = [source.source_ref for segment in segments for source in segment.sources]
  other_file = ({"thread_session.md", "manager_workflows.md"} - {rule_file}).pop()
  assert f"prompts/{rule_file}" in refs
  assert f"prompts/{other_file}" not in refs
  assert refs.index(f"prompts/{rule_file}") == refs.index("prompts/master.md") + 1
  assert "prompts/task_manager.md" in refs
  by_ref = {source.source_ref: segment.text for segment in segments for source in segment.sources}
  assert by_ref[f"prompts/{rule_file}"] == prompts_text(rule_file, meta.id)


@pytest.mark.parametrize("kind", SESSION_KINDS)
def test_v1_builder_output_is_master_prompt_then_the_rule_file_of_its_kind(kind: str, tmp_path: Path) -> None:
  rule_file, origin = SESSION_KINDS[kind]
  meta = SessionMetadata(id="root-1", name="root", **origin)

  built = master_cc_run._build_instructions_content(meta, real_repo_cfg(tmp_path / "home"), None)

  assert built == prompts_text("master.md", meta.id) + "\n\n" + prompts_text(rule_file, meta.id)


# ---------------------------------------------------------------------------
# The context window: a thread session runs the thread window, any other session keeps the option's
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("kind, window", [("slack", 96_000), ("discord", 96_000), ("plain", None)])
def test_context_window_is_set_for_a_thread_session_only(kind: str, window: int | None) -> None:
  _, origin = SESSION_KINDS[kind]
  meta = SessionMetadata(id="s", name="s", **origin)

  assert turn_contributions.resolve_context_window(meta) == window


@pytest.mark.asyncio
async def test_a_backend_that_reads_no_context_window_keeps_its_option(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The run applies the thread window only to a backend type whose traits read one: the codex type
  does not, so a thread session's option reaches the backend unchanged. The reading backend's two
  cases (thread window, main session's own) run through the charlie-code tests of the backend routing suite."""
  cfg = build_master_cc_cfg(tmp_path)
  patch_instructions_content(monkeypatch)
  builds = install_scripted_backends(
      monkeypatch, [ScriptedRelayBackend([make_result_event()], exit_code=0)], BUILD_BACKEND_PATCH_TARGET)
  thread_meta = SessionMetadata(id="thread", name="thread", backend="fake", slack_origin=SLACK_ORIGIN)

  await master_cc_run._run_cc(make_work_item(cfg, thread_meta, cfg.backends.options[0]))

  assert builds[0]["option"] == cfg.backends.options[0]


# ---------------------------------------------------------------------------
# The registry's contract
# ---------------------------------------------------------------------------


class _Names(turn_contributions.TurnContribution):
  """A contribution that names a workflow rules file, a context window and one renderer."""

  def __init__(self, rules_file: str | None, window: int | None, renderers: dict) -> None:
    self._rules_file, self._window, self._renderers = rules_file, window, renderers

  def workflow_rules_file(self, meta: SessionMetadata) -> str | None:
    return self._rules_file

  def context_window(self, meta: SessionMetadata) -> int | None:
    return self._window

  def event_renderers(self) -> dict:
    return self._renderers


def _row(event: dict) -> dict:
  return {"role": "system", "content": "row"}


FIRST = _Names("first.md", 1000, {"first_event": _row})
SECOND = _Names("second.md", 2000, {"second_event": _row})
SILENT = _Names(None, None, {})


@pytest.fixture
def empty_registry(monkeypatch: pytest.MonkeyPatch) -> None:
  """A registry with no contribution; the real registrations come back at teardown."""
  monkeypatch.setattr(turn_contributions, "_registered", {})
  monkeypatch.setattr(turn_contributions, "_instances", {})


def register(**contributions: str) -> None:
  for name, attr in contributions.items():
    turn_contributions.register_turn_contribution(name, f"{__name__}:{attr}")


def test_a_second_registration_of_one_name_raises(empty_registry: None) -> None:
  register(first="FIRST")

  with pytest.raises(ValueError, match="first"):
    register(first="SECOND")

  assert turn_contributions.turn_contributions() == (FIRST,)


def test_contributions_come_back_in_registration_order(empty_registry: None) -> None:
  register(second="SECOND", first="FIRST", silent="SILENT")

  assert turn_contributions.turn_contributions() == (SECOND, FIRST, SILENT)


def test_one_named_rule_file_wins_and_none_means_the_manager_workflows(empty_registry: None) -> None:
  meta = SessionMetadata(id="s", name="s")
  assert turn_contributions.resolve_workflow_rules_file(meta) == "manager_workflows.md"

  register(silent="SILENT", first="FIRST")

  assert turn_contributions.resolve_workflow_rules_file(meta) == "first.md"
  assert turn_contributions.resolve_context_window(meta) == 1000


def test_two_named_rule_files_or_two_context_windows_raise(empty_registry: None) -> None:
  meta = SessionMetadata(id="s", name="s")
  register(first="FIRST", second="SECOND")

  with pytest.raises(ValueError, match="workflow rules file"):
    turn_contributions.resolve_workflow_rules_file(meta)
  with pytest.raises(ValueError, match="context window"):
    turn_contributions.resolve_context_window(meta)


def test_two_named_rule_files_fail_the_task_path_build(empty_registry: None, tmp_path: Path) -> None:
  register(first="FIRST", second="SECOND")

  with pytest.raises(ValueError, match="workflow rules file"):
    build_segments(
        real_repo_cfg(tmp_path / "home"),
        SessionMetadata(id="s", name="s"),
        "manager_turn",
        overlay=None,
        chain=(),
        node_ref=None)


def test_an_event_type_rendered_twice_raises(empty_registry: None) -> None:
  other_first = _Names(None, None, {"first_event": _row})
  table = {"existing": lambda event: None}

  merged = message_aggregator.merge_event_renderers(table, (FIRST, SECOND))
  assert set(merged) == {"existing", "first_event", "second_event"}
  with pytest.raises(ValueError, match="first_event"):
    message_aggregator.merge_event_renderers(table, (FIRST, other_first))
  with pytest.raises(ValueError, match="master_done"):
    message_aggregator.merge_event_renderers(
        message_aggregator._SIMPLE_HANDLERS, (_Names(None, None, {ET.MASTER_DONE: _row}),))


def test_registration_imports_no_contribution_module() -> None:
  """register_all() only records "module:attr" strings: the contribution modules import on first use."""
  probe = (
      "import sys\n"
      "from src.app import registrations\n"
      "registrations.register_all()\n"
      "def loaded():\n"
      "  return [m for m in sys.modules if m.endswith('.turn_contribution') and m.startswith('src.features.')]\n"
      "assert not loaded(), loaded()\n"
      "from src.runtime.hooks import turn_contributions\n"
      "turn_contributions.turn_contributions()\n"
      "assert 'src.features.memory.turn_contribution' in loaded(), loaded()\n")
  result = subprocess.run(
      [sys.executable, "-c", probe], cwd=conftest.ROOT, capture_output=True, text=True, timeout=60, check=False)

  assert result.returncode == 0, result.stderr


@pytest.mark.parametrize(
    "omitted, contribution_type", [
        ("src.features.latex", "LatexTurnContribution"),
        ("src.features.memory", "MemoryTurnContribution"),
    ])
def test_server_turn_paths_work_when_a_package_is_not_registered(
    tmp_path: Path, omitted: str, contribution_type: str) -> None:
  probe = tmp_path / "probe.py"
  profile = tmp_path / "profile"
  probe.write_text(
      textwrap.dedent(
          f"""\
      import asyncio
      import sys
      from pathlib import Path

      sys.path.insert(0, {str(conftest.ROOT)!r})
      from src.app import registrations
      registrations.PACKAGES = tuple(package for package in registrations.PACKAGES if package != {omitted!r})

      sys.path.insert(0, {str(conftest.ROOT / "tests")!r})
      import conftest
      import server

      from src.infra import event_types as ET
      from src.infra.models import CreateSessionRequest
      from src.runtime import sessions
      from src.runtime.hooks import turn_contributions
      from src.runtime.task_prompts import SCOPE_MEMORY, build_segments

      async def main():
        cfg = conftest.build_master_cc_cfg(Path({str(profile)!r}))
        conftest.write_memory_topics(cfg.memory_dir, ["profile resident"])
        conftest.write_memory_entry(
            cfg.memory_dir, "profile", "resident-note", audience="master", body="resident memory")
        manager = sessions.SessionManager(cfg)
        tasks = []

        def schedule(coro, *, name):
          task = asyncio.create_task(coro, name=name)
          tasks.append(task)
          return task

        sessions.create_logged_task = schedule
        meta = await manager.create_session(CreateSessionRequest(name="scratch"))
        await manager.persist_and_broadcast(meta.id, {{"type": ET.MASTER_DONE}})
        await asyncio.gather(*tasks)
        assert {contribution_type!r} not in [type(item).__name__ for item in turn_contributions.turn_contributions()]
        segments, _ = build_segments(cfg, meta, "manager_turn", overlay=None, chain=(), node_ref=None)
        has_memory = any(source.scope == SCOPE_MEMORY for segment in segments for source in segment.sources)
        assert has_memory is ({omitted!r} == "src.features.latex")

      asyncio.run(main())
      """),
      encoding="utf-8",
  )
  result = subprocess.run(
      [sys.executable, str(probe)], cwd=conftest.ROOT, capture_output=True, text=True, timeout=60, check=False)

  assert result.returncode == 0, f"{result.stdout}\n{result.stderr}"


# ---------------------------------------------------------------------------
# The chat aggregator: reply events render through the platforms' contributions
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "event_type, content, rendered", [
        (ET.SLACK_REPLY, "hello slack", "Posted to Slack: hello slack"),
        (ET.DISCORD_REPLY, "hello discord", "Posted to Discord: hello discord"),
    ])
def test_reply_events_render_as_system_rows(event_type: str, content: str, rendered: str) -> None:
  event = {"type": event_type, "content": content, "id": "e1", "timestamp": "2026-01-01T00:00:00Z"}

  deltas = list(message_aggregator.MessageAggregator().feed(event))

  assert deltas == [
      {
          "type": "message",
          "message":
              {
                  "role": "system",
                  "content": rendered,
                  "timestamp": "2026-01-01T00:00:00Z",
                  "event_index": 0,
                  "id": "e1",
              },
      }
  ]


# ---------------------------------------------------------------------------
# The session funnel: one MASTER_DONE reaches every contribution, each in its own task
# ---------------------------------------------------------------------------


class _Raises(turn_contributions.TurnContribution):

  async def after_turn(self, meta: SessionMetadata, done_event: dict, *, cfg, sessions) -> None:
    raise RuntimeError("after_turn failed")


@pytest.mark.asyncio
async def test_one_master_done_calls_each_platforms_deliver_done_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = build_master_cc_cfg(tmp_path)
  mgr = SessionManager(cfg)
  meta = await mgr.create_session(CreateSessionRequest(name="both"))
  done = {"type": ET.MASTER_DONE, "exit_code": 0, "still_thinking": False}
  # A failing contribution in front of the real ones must not stop them or the append.
  real = turn_contributions.turn_contributions()
  monkeypatch.setattr(turn_contributions, "turn_contributions", lambda: (_Raises(), *real))
  tasks: list[asyncio.Task] = []

  with (
      patch("src.features.slack.slack_listener.deliver_done", new=AsyncMock(return_value=True)) as slack,
      patch("src.features.discord.discord_listener.deliver_done", new=AsyncMock(return_value=True)) as discord,
      patch("src.runtime.sessions.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
  ):
    await mgr.persist_and_broadcast(meta.id, done)
    await mgr.persist_and_broadcast(meta.id, {"type": ET.SCHEDULED_TRIGGER, "content": "wake"})
    await asyncio.gather(*tasks, return_exceptions=True)

  slack.assert_awaited_once_with(meta.id, done, cfg, mgr)
  discord.assert_awaited_once_with(meta.id, done, cfg, mgr)
  assert len(tasks) == len(real) + 1
  assert [e["type"] for e in mgr.load_chat_events_sync(meta.id)] == [ET.MASTER_DONE, ET.SCHEDULED_TRIGGER]


# ---------------------------------------------------------------------------
# LaTeX: a turn that edits the .tex file proposes the edit after its MASTER_DONE
# ---------------------------------------------------------------------------


@pytest.fixture
def tex_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  path = tmp_path / "report.tex"
  path.write_text("original", encoding="utf-8")
  monkeypatch.setattr(latex, "get_tex_path", lambda: path)
  monkeypatch.setattr(latex, "_tex_snapshot", None)
  monkeypatch.setattr(latex, "_pending_proposal", None)
  return path


async def run_turn(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    turn: conftest.ConsumerRound,
) -> tuple[SessionManager, str, list[asyncio.Task]]:
  """Queue one turn through the real run_message and consumer, with _run_cc replaced by *turn*.

  Returns the manager, the session id and the after_turn tasks the funnel spawned.
  """
  cfg = build_master_cc_cfg(tmp_path)
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="tex"))
  tasks: list[asyncio.Task] = []
  monkeypatch.setattr(master_cc_run, "_run_cc", turn)
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", AsyncMock())
  monkeypatch.setattr("src.runtime.sessions.create_logged_task", make_task_spawner(tasks))
  async with fresh_master_state(session.id):
    await master_cc_queue.run_message(cfg, session, "edit the paper", mgr.callbacks(), ET.USER, skip_user_event=True)
    await conftest.drain_session_consumer(session.id, timeout=5)
    await asyncio.gather(*tasks)
  return mgr, session.id, tasks


def edits_tex(path: Path, content: str) -> conftest.ConsumerRound:

  async def turn(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    path.write_text(content, encoding="utf-8")
    return ("cc-1", 0, None, {})

  return turn


@pytest.mark.asyncio
async def test_a_changed_tex_file_is_proposed_after_master_done_and_reverted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, tex_file: Path) -> None:
  mgr, session_id, _ = await run_turn(tmp_path, monkeypatch, edits_tex(tex_file, "edited by the agent"))

  types = [event["type"] for event in mgr.load_chat_events_sync(session_id)]
  assert types == [ET.MASTER_DONE, ET.TEX_EDIT_PROPOSED]
  assert tex_file.read_text(encoding="utf-8") == "original"
  assert latex.get_pending_proposal() == {"old": "original", "new": "edited by the agent"}


@pytest.mark.asyncio
async def test_an_unchanged_tex_file_appends_nothing_and_clears_the_snapshot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, tex_file: Path) -> None:
  mgr, session_id, _ = await run_turn(tmp_path, monkeypatch, edits_tex(tex_file, "original"))

  assert [event["type"] for event in mgr.load_chat_events_sync(session_id)] == [ET.MASTER_DONE]
  assert latex._tex_snapshot is None
  assert latex.get_pending_proposal() is None


@pytest.mark.asyncio
async def test_latex_after_turn_skips_the_check_without_a_snapshot(monkeypatch: pytest.MonkeyPatch) -> None:
  sessions = SimpleNamespace(persist_and_broadcast=AsyncMock())
  contribution = LatexTurnContribution()
  meta = SessionMetadata(id="no-snapshot", name="no-snapshot")
  monkeypatch.setattr(latex, "has_snapshot", lambda: False)

  with patch("src.features.latex.turn_contribution.asyncio.to_thread", new=AsyncMock()) as to_thread:
    await contribution.after_turn(meta, {"type": ET.MASTER_DONE}, cfg=SimpleNamespace(), sessions=sessions)

  to_thread.assert_not_awaited()
  sessions.persist_and_broadcast.assert_not_awaited()


class _LetGoBackend(ScriptedRelayBackend):
  """A backend whose turn is cancelled after the agent edited the file, as at a graceful restart."""

  def __init__(self, tex_path: Path) -> None:
    super().__init__([], exit_code=0)
    self._tex_path = tex_path

  async def run(self, prompt: str, cwd: str, env: dict, uploaded_files: list[dict] | None = None):
    await self._on_spawn(self.pid)
    self._tex_path.write_text("edited by the agent", encoding="utf-8")
    raise asyncio.CancelledError
    yield {}


@pytest.mark.asyncio
async def test_a_let_go_turn_appends_no_master_done_and_proposes_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, tex_file: Path) -> None:
  """A turn left running in another process ends in CancelledError: the consumer appends no MASTER_DONE,
  so no after_turn runs, and the agent's edit stays on disk for the next boot's re-attach to settle."""
  cfg = build_master_cc_cfg(tmp_path)
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="let-go"))
  tasks: list[asyncio.Task] = []
  patch_instructions_content(monkeypatch)
  install_scripted_backends(monkeypatch, [_LetGoBackend(tex_file)], BUILD_BACKEND_PATCH_TARGET)
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", AsyncMock())
  monkeypatch.setattr("src.runtime.sessions.create_logged_task", make_task_spawner(tasks))

  async with fresh_master_state(session.id):
    queued = asyncio.create_task(
        master_cc_queue.run_message(cfg, session, "edit the paper", mgr.callbacks(), ET.USER, skip_user_event=True))
    for _ in range(1000):
      if session.id in master_cc_state._session_consumers:
        break
      await asyncio.sleep(0)
    consumer = master_cc_state._session_consumers[session.id]
    await asyncio.wait({consumer}, timeout=5)
    queued.cancel()
    await asyncio.gather(queued, return_exceptions=True)

  assert consumer.cancelled()
  assert (await mgr.get_session(session.id)).master_run is not None  # the let-go left the re-attach record
  assert not tasks
  assert ET.MASTER_DONE not in [event["type"] for event in mgr.load_chat_events_sync(session.id)]
  assert tex_file.read_text(encoding="utf-8") == "edited by the agent"
  assert latex.get_pending_proposal() is None

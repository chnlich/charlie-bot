"""Turn contributions: the registry's contract, and the runtime call sites that ask it.

The runtime turn path names no feature. Each feature package registers one TurnContribution, and the path
asks every registered contribution at its step: the instruction build (rule file, segments), the queue
(before a turn), the session funnel (after a MASTER_DONE) and the chat aggregator (event renderers). The
tests drive each call site through the real registrations that conftest makes.
"""

from __future__ import annotations

import asyncio
import pathlib
import subprocess
import sys
import types
from unittest import mock

import conftest
import pytest

from src.features.discord import event_types as discord_event_types
from src.features.discord import metadata as discord_metadata
from src.features.discord import turn_contribution as discord_turn_contribution
from src.features.slack import event_types as slack_event_types
from src.features.slack import metadata as slack_metadata
from src.features.slack import turn_contribution as slack_turn_contribution
from src.infra import event_types as ET
from src.infra import models
from src.runtime import master_cc_state, message_aggregator, streaming, task_prompts
from src.runtime.hooks import turn_contributions

SLACK_ORIGIN = slack_metadata.SlackOrigin(team_id="T1", channel_id="C1", thread_ts="1700000000.000100")
DISCORD_ORIGIN = discord_metadata.DiscordOrigin(guild_id="G1", parent_channel_id="C1", thread_id="1234567890")
SESSION_KINDS = {
    "slack": ("thread_session.md", {
        "slack_origin": SLACK_ORIGIN
    }),
    "discord": ("thread_session.md", {
        "discord_origin": DISCORD_ORIGIN
    }),
    "plain": ("manager_workflows.md", {}),
}


def real_repo_cfg(home: pathlib.Path) -> types.SimpleNamespace:
  """Instruction inputs over this checkout's real prompts tree; the host override and memory store are absent."""
  return types.SimpleNamespace(
      charlie_bot_repo=conftest.ROOT,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      charliebot_home=home,
  )


def prompts_text(filename: str, session_id: str) -> str:
  return (conftest.ROOT / "prompts" / filename).read_text(encoding="utf-8").replace("{{session_id}}", session_id)


# ---------------------------------------------------------------------------
# The workflow rules file: a thread root gets its brief on the task path
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("kind", SESSION_KINDS)
def test_manager_root_task_path_reads_the_rule_file_of_its_kind(kind: str, tmp_path: pathlib.Path) -> None:
  rule_file, origin = SESSION_KINDS[kind]
  meta = models.SessionMetadata(id="root-1", name="root", profile="manager", **origin)

  segments, _ = task_prompts.build_segments(
      real_repo_cfg(tmp_path / "home"), meta, "manager_turn", overlay=None, chain=(), node_ref=None)

  refs = [source.source_ref for segment in segments for source in segment.sources]
  other_file = ({"thread_session.md", "manager_workflows.md"} - {rule_file}).pop()
  assert f"prompts/{rule_file}" in refs
  assert f"prompts/{other_file}" not in refs
  assert refs.index(f"prompts/{rule_file}") == refs.index("prompts/master.md") + 1
  assert "prompts/task_manager.md" in refs
  by_ref = {source.source_ref: segment.text for segment in segments for source in segment.sources}
  assert by_ref[f"prompts/{rule_file}"] == prompts_text(rule_file, meta.id)


# ---------------------------------------------------------------------------
# The thread reply contract: a thread session's manager instructions carry it once
# ---------------------------------------------------------------------------


def _prompt_text(name: str) -> str:
  return (conftest.ROOT / "prompts" / name).read_text(encoding="utf-8").strip()


@pytest.mark.parametrize("kind, summon_line", [("slack", "Slack 线程召唤"), ("discord", "Discord 线程召唤")])
def test_thread_session_manager_instructions_carry_the_reply_contract_once(
    kind: str, summon_line: str, tmp_path: pathlib.Path) -> None:
  """The contract (platform part plus the three rule docs) rides the managed
  instructions once, with each doc's full text present exactly once; the
  summon input itself stays out of the instructions."""
  _, origin = SESSION_KINDS[kind]
  meta = models.SessionMetadata(id="root-1", name="root", profile="manager", **origin)

  segments, _ = task_prompts.build_segments(
      real_repo_cfg(tmp_path / "home"), meta, "manager_turn", overlay=None, chain=(), node_ref=None)
  joined = "\n\n".join(segment.text for segment in segments)

  scope_doc = f"{kind}_reply_scope.md"
  scope, red_line, reply_format = (
      _prompt_text(name) for name in (scope_doc, "thread_reply_redline.md", "thread_reply_format.md"))
  for doc_text in (scope, red_line, reply_format):
    assert joined.count(doc_text) == 1
  # One segment carries the contract; its sources name the three prompt files.
  contract = [segment for segment in segments if scope in segment.text]
  assert len(contract) == 1
  assert [source.source_ref for source in contract[0].sources
         ] == [f"prompts/{scope_doc}", "prompts/thread_reply_redline.md", "prompts/thread_reply_format.md"]
  # The platform part rides the same segment, ahead of the docs.
  assert contract[0].text.index("Platform:") < contract[0].text.index(scope)
  # The contract segment holds rules only: the summon line stays the input's
  # content (thread_session.md legitimately quotes it as an input kind).
  assert summon_line not in contract[0].text


def test_slack_contract_part_carries_the_thread_address(tmp_path: pathlib.Path) -> None:
  """Slack's contract part names this session's thread ids — the summon link's replacement."""
  _, origin = SESSION_KINDS["slack"]
  meta = models.SessionMetadata(id="s", name="s", profile="manager", **origin)

  segments, _ = task_prompts.build_segments(
      real_repo_cfg(tmp_path / "home"), meta, "manager_turn", overlay=None, chain=(), node_ref=None)
  joined = "\n\n".join(segment.text for segment in segments)

  assert "channel `C1`、thread_ts `1700000000.000100`" in joined
  assert "conversations.replies" in joined
  assert "本次召唤是线程里最新一条 @ 机器人的消息" in joined
  assert "`charliebot slack ack --message-id <ts> [...]`" in joined


def test_discord_contract_part_carries_the_summon_mark(tmp_path: pathlib.Path) -> None:
  """Discord's contract part states the server-side read and the ``summon: true`` mark,
  and carries no Slack-only citation boundary."""
  _, origin = SESSION_KINDS["discord"]
  meta = models.SessionMetadata(id="s", name="s", profile="manager", **origin)

  segments, _ = task_prompts.build_segments(
      real_repo_cfg(tmp_path / "home"), meta, "manager_turn", overlay=None, chain=(), node_ref=None)
  joined = "\n\n".join(segment.text for segment in segments)

  assert "`charliebot discord read`" in joined
  assert "`summon: true`" in joined
  assert "已成文的私有内容不引用" not in joined


@pytest.mark.parametrize("kind", ["slack", "discord"])
def test_thread_session_prompt_carries_the_status_and_language_rules(kind: str, tmp_path: pathlib.Path) -> None:
  """The status block lives in the session message (thread_session.md), and the reply
  language rule covers every summoner (thread_reply_format.md): deleting any one
  sentence from either file breaks the assembled instructions."""
  _, origin = SESSION_KINDS[kind]
  meta = models.SessionMetadata(id="root-1", name="root", profile="manager", **origin)

  segments, _ = task_prompts.build_segments(
      real_repo_cfg(tmp_path / "home"), meta, "manager_turn", overlay=None, chain=(), node_ref=None)
  joined = "\n\n".join(segment.text for segment in segments)

  for sentence in (
      # thread_session.md, the status-block item and its carve-out.
      "- In this thread session, open your last session message of each turn with the status block:",
      "- The status block stays in the session message. The thread reply follows `prompts/thread_reply_format.md`.",
      # thread_reply_format.md, the language section's summoner rule.
      "This rule decides the reply language for every summoner, the operator included.",
      "The summon line of the round and the lines of this contract set no reply language.",
  ):
    assert sentence in joined


def test_main_session_instructions_carry_no_reply_contract(tmp_path: pathlib.Path) -> None:
  """A session without a thread origin gets none of the contract."""
  meta = models.SessionMetadata(id="root-1", name="root", profile="manager")

  segments, _ = task_prompts.build_segments(
      real_repo_cfg(tmp_path / "home"), meta, "manager_turn", overlay=None, chain=(), node_ref=None)
  joined = "\n\n".join(segment.text for segment in segments)
  refs = [source.source_ref for segment in segments for source in segment.sources]

  for name in ("slack_reply_scope.md", "discord_reply_scope.md", "thread_reply_redline.md", "thread_reply_format.md"):
    assert _prompt_text(name) not in joined
    assert f"prompts/{name}" not in refs


@pytest.mark.parametrize(
    "contribution, kind", [
        (slack_turn_contribution.SlackTurnContribution(), "slack"),
        (discord_turn_contribution.DiscordTurnContribution(), "discord")
    ])
def test_contract_segment_goes_to_manager_rounds_only(contribution, kind: str) -> None:
  """A worker-kind run of the same thread session gets no contract."""
  _, origin = SESSION_KINDS[kind]
  meta = models.SessionMetadata(id="s", name="s", profile="manager", **origin)

  assert contribution.instruction_segments(meta, "work", real_repo_cfg(pathlib.Path("/tmp"))) == []


# ---------------------------------------------------------------------------
# The registry's contract
# ---------------------------------------------------------------------------


class _Names(turn_contributions.TurnContribution):
  """A contribution that names a workflow rules file and one renderer."""

  def __init__(self, rules_file: str | None, renderers: dict) -> None:
    self._rules_file, self._renderers = rules_file, renderers

  def workflow_rules_file(self, meta: models.SessionMetadata) -> str | None:
    return self._rules_file

  def event_renderers(self) -> dict:
    return self._renderers


def _row(event: dict) -> dict:
  return {"role": "system", "content": "row"}


FIRST = _Names("first.md", {"first_event": _row})
SECOND = _Names("second.md", {"second_event": _row})
SILENT = _Names(None, {})


class _Opens(turn_contributions.TurnContribution):
  """A contribution that opens every user input with one preamble."""

  def __init__(self, preamble: str) -> None:
    self._preamble = preamble

  def input_preamble(self, meta: models.SessionMetadata, input_event: dict) -> str | None:
    return self._preamble


OPENS_A = _Opens("note A")
OPENS_B = _Opens("note B")


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
  meta = models.SessionMetadata(profile="manager", id="s", name="s")
  assert turn_contributions.resolve_workflow_rules_file(meta) == "manager_workflows.md"

  register(silent="SILENT", first="FIRST")

  assert turn_contributions.resolve_workflow_rules_file(meta) == "first.md"


def test_two_named_rule_files_raise(empty_registry: None) -> None:
  meta = models.SessionMetadata(profile="manager", id="s", name="s")
  register(first="FIRST", second="SECOND")

  with pytest.raises(ValueError, match="workflow rules file"):
    turn_contributions.resolve_workflow_rules_file(meta)


def test_one_input_preamble_wins_and_two_raise(empty_registry: None) -> None:
  meta = models.SessionMetadata(profile="manager", id="s", name="s")
  event = {"type": ET.USER, "content": "hello"}
  assert turn_contributions.resolve_input_preamble(meta, event) is None

  register(silent="SILENT", opens_a="OPENS_A")
  assert turn_contributions.resolve_input_preamble(meta, event) == "note A"

  register(opens_b="OPENS_B")
  with pytest.raises(ValueError, match="input preamble"):
    turn_contributions.resolve_input_preamble(meta, event)


def test_two_named_rule_files_fail_the_task_path_build(empty_registry: None, tmp_path: pathlib.Path) -> None:
  register(first="FIRST", second="SECOND")

  with pytest.raises(ValueError, match="workflow rules file"):
    task_prompts.build_segments(
        real_repo_cfg(tmp_path / "home"),
        models.SessionMetadata(profile="manager", id="s", name="s"),
        "manager_turn",
        overlay=None,
        chain=(),
        node_ref=None)


def test_an_event_type_rendered_twice_raises(empty_registry: None) -> None:
  other_first = _Names(None, {"first_event": _row})
  table = {"existing": lambda event: None}

  merged = message_aggregator.merge_event_renderers(table, (FIRST, SECOND))
  assert set(merged) == {"existing", "first_event", "second_event"}
  with pytest.raises(ValueError, match="first_event"):
    message_aggregator.merge_event_renderers(table, (FIRST, other_first))
  with pytest.raises(ValueError, match="master_done"):
    message_aggregator.merge_event_renderers(
        message_aggregator._SIMPLE_HANDLERS, (_Names(None, {ET.MASTER_DONE: _row}),))


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


# ---------------------------------------------------------------------------
# The chat aggregator: reply events render through the platforms' contributions
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "event_type, content, rendered", [
        (slack_event_types.SLACK_REPLY, "hello slack", "Posted to Slack: hello slack"),
        (discord_event_types.DISCORD_REPLY, "hello discord", "Posted to Discord: hello discord"),
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

  async def after_turn(self, meta: models.SessionMetadata, done_event: dict, *, cfg) -> None:
    raise RuntimeError("after_turn failed")


@pytest.mark.asyncio
async def test_one_master_done_calls_each_platforms_deliver_done_once(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.build_master_cc_cfg(tmp_path)
  mgr = conftest.build_session_blocks(cfg)
  meta = await conftest.create_root_session(mgr, models.CreateSessionRequest(name="both"))
  conftest.bind_deps_blocks(monkeypatch, mgr.tree, mgr)
  done = {"type": ET.MASTER_DONE, "exit_code": 0, "still_thinking": False}
  # A failing contribution in front of the real ones must not stop them or the append.
  real = turn_contributions.turn_contributions()
  monkeypatch.setattr(turn_contributions, "turn_contributions", lambda: (_Raises(), *real))
  tasks: list[asyncio.Task] = []

  with (
      mock.patch(conftest.SLACK_LISTENER_DELIVER_DONE_PATCH_TARGET, new=mock.AsyncMock(return_value=True)) as slack,
      mock.patch(conftest.DISCORD_LISTENER_DELIVER_DONE_PATCH_TARGET, new=mock.AsyncMock(return_value=True)) as discord,
      mock.patch(conftest.TASKS_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=conftest.make_task_spawner(tasks)),
      mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()),
  ):
    await mgr.events.persist_and_broadcast(meta.id, done)
    await mgr.events.persist_and_broadcast(meta.id, {"type": ET.SCHEDULED_TRIGGER, "content": "wake"})
    await asyncio.gather(*tasks, return_exceptions=True)

  slack.assert_awaited_once_with(meta.id, done, cfg, mgr.store, mgr.lifecycle, mgr.events, mgr.successor)
  discord.assert_awaited_once_with(meta.id, done, cfg, mgr.store, mgr.lifecycle, mgr.events, mgr.successor)
  # The web contribution's status reminder wakes one more turn, so it adds one task and one event.
  assert len(tasks) == len(real) + 2
  events = mgr.events.load_chat_events_sync(meta.id)
  # The round's event lands before any contribution reacts. The reminder's event and the
  # caller's next event have no promised order between them: the contribution runs in its own
  # task, so it may append before or after that later persist.
  assert [e["type"] for e in events[:2]] == [ET.TASK_CREATED, ET.MASTER_DONE]
  assert sorted(e["type"] for e in events[2:]) == sorted([ET.AGENT_MESSAGE, ET.SCHEDULED_TRIGGER])


# ---------------------------------------------------------------------------
# A turn left running in another process: no MASTER_DONE, so no after_turn runs
# ---------------------------------------------------------------------------


class _RecordingContribution(turn_contributions.TurnContribution):
  """The test's own funnel observer: its after_turn records every call."""

  def __init__(self) -> None:
    self.after_turn_calls: list[dict] = []

  async def after_turn(self, meta: models.SessionMetadata, done_event: dict, *, cfg) -> None:
    self.after_turn_calls.append(done_event)


class _LetGoBackend(conftest.ScriptedRelayBackend):
  """A backend whose turn is cancelled after the agent edited the file, as at a graceful restart.

  The gate holds the CancelledError until the test holds the consumer's task handle. A started
  run() proves the consumer dequeued the item, and the enqueue registered the consumer before
  that (master_cc_queue._enqueue_work_item), so the registry read cannot miss a consumer whose
  turn already started. The before_turn hooks sit between create_task and the registration, and
  their thread hops have no loop-tick budget.
  """

  def __init__(self, path: pathlib.Path) -> None:
    super().__init__([], exit_code=0)
    self._path = path
    self.turn_started = asyncio.Event()
    self.release_cancel = asyncio.Event()

  async def run(self, prompt: str, cwd: str, env: dict, uploaded_files: list[dict] | None = None):
    await self._on_spawn(self.pid)
    self._path.write_text("edited by the agent", encoding="utf-8")
    self.turn_started.set()
    await self.release_cancel.wait()
    raise asyncio.CancelledError
    yield {}


@pytest.mark.asyncio
async def test_a_let_go_turn_appends_no_master_done_and_runs_no_after_turn(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A turn left running in another process ends in CancelledError: the consumer appends no MASTER_DONE,
  so no after_turn runs, and the agent's edit stays on disk for the next boot's re-attach to settle."""
  cfg = conftest.build_master_cc_cfg(tmp_path)
  mgr = conftest.build_session_blocks(cfg)
  session = await conftest.create_root_session(mgr, models.CreateSessionRequest(name="let-go"))
  conftest.bind_deps_blocks(monkeypatch, mgr.tree, mgr)
  tasks: list[asyncio.Task] = []
  edited = tmp_path / "edited.txt"
  let_go_backend = _LetGoBackend(edited)
  conftest.install_scripted_backends(monkeypatch, [let_go_backend], conftest.BUILD_BACKEND_PATCH_TARGET)
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", mock.AsyncMock())
  monkeypatch.setattr(conftest.TASKS_CREATE_LOGGED_TASK_PATCH_TARGET, conftest.make_task_spawner(tasks))
  recorder = _RecordingContribution()
  real = turn_contributions.turn_contributions()
  monkeypatch.setattr(turn_contributions, "turn_contributions", lambda: (*real, recorder))

  async with conftest.fresh_master_state(session.id):
    queued = asyncio.create_task(conftest.run_task_manager_message(cfg, session, "edit the file", mgr.callbacks()))
    try:
      await asyncio.wait_for(let_go_backend.turn_started.wait(), timeout=5)
      consumer = master_cc_state._session_consumers[session.id]
    finally:
      let_go_backend.release_cancel.set()
    await asyncio.wait({consumer}, timeout=5)
    queued.cancel()
    await asyncio.gather(queued, return_exceptions=True)

  assert consumer.cancelled()
  assert not tasks
  assert ET.MASTER_DONE not in [event["type"] for event in mgr.events.load_chat_events_sync(session.id)]
  assert edited.read_text(encoding="utf-8") == "edited by the agent"
  assert recorder.after_turn_calls == []

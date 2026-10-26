"""Live user-message delivery for web manager turns served by Claude Code."""

from __future__ import annotations

import asyncio
import json
import pathlib
from collections.abc import Iterable
from typing import Any, Protocol

from src.infra import config, log_once, metadata_slots, models, ndjson
from src.infra import event_types as ET
from src.runtime import runs, task_input_prompt
from src.runtime.hooks import backend_types

log = log_once.LazyStructlogLogger()
_CLAUDE_BACKEND_TYPES = frozenset({"cc-claude", "cc-anthropic-compatible", "cc-openai-compatible"})


class _Process(Protocol):
  """The subprocess fields the stdin delivery path reads."""

  stdin: asyncio.StreamWriter | None
  returncode: int | None


class _Backend(Protocol):
  """The Claude backend fields the delivery path reads."""

  _midturn_delivery: bool
  _midturn_run_id: str | None
  _proc: _Process | None


class _Dispatcher(Protocol):
  """The dispatcher operation that binds replay evidence to a Run."""

  async def claim_echoed_user_inputs(self, session_id: str, run_id: str, echoed_ids: Iterable[str]) -> list[str]:
    ...


class _WorkItem(Protocol):
  """The running manager item fields used by reply binding."""

  user_event_ids: list[str]
  task_run: Any


class _DeliveryState:
  """The serialized stdin writer and replay evidence for one Run."""

  def __init__(self) -> None:
    self.lock = asyncio.Lock()
    self.initial_written = asyncio.Event()
    self.process: _Process | None = None
    self.stdin: asyncio.StreamWriter | None = None
    self.attempted_ids: set[str] = set()
    self.written_ids: set[str] = set()
    self.closed = False
    self.released = False
    self.write_error: BaseException | None = None


_RUNS: dict[str, _DeliveryState] = {}


def is_gated_turn(meta: models.SessionMetadata, *, turn: bool, run_kind: str) -> bool:
  """Whether this is a web manager turn eligible for live user input."""
  if not turn or run_kind != "manager_turn":
    return False
  return is_web_session(meta)


def is_web_session(meta: models.SessionMetadata) -> bool:
  """Whether a manager session is not owned by a Slack or Discord thread."""
  slack = metadata_slots.fields_of(meta, "slack")
  discord = metadata_slots.fields_of(meta, "discord")
  return meta.profile == "manager" and slack.slack_origin is None and discord.discord_origin is None


def launch_kwargs(meta: models.SessionMetadata, *, turn: bool, run_kind: str, run_id: str | None) -> dict[str, Any]:
  """The Claude lifecycle arguments that enable the stream-json input path."""
  enabled = is_gated_turn(meta, turn=turn, run_kind=run_kind)
  if enabled and run_id is None:
    raise ValueError("a gated web manager turn needs its Run id")
  return {"midturn_delivery": enabled, "midturn_run_id": run_id if enabled else None}


def bind_process(run_id: str | None, backend: _Backend) -> None:
  """Bind a gated Claude process while preserving this Run's replay evidence."""
  if not getattr(backend, "_midturn_delivery", False):
    return
  if run_id is None or backend._midturn_run_id != run_id:
    raise ValueError("the mid-turn backend Run id does not match its task Run")
  state = _RUNS.setdefault(run_id, _DeliveryState())
  state.process = None
  state.stdin = None
  state.closed = False
  state.released = False
  state.write_error = None
  state.initial_written.clear()


def _user_line(run_id: str, content: str) -> bytes:
  """Encode one Claude stream-json user input line."""
  return (
      json.dumps(
          {
              "type": "user",
              "uuid": run_id,
              "message": {
                  "role": "user",
                  "content": content
              },
          },
          ensure_ascii=False,
          separators=(",", ":")) + "\n").encode("utf-8")


async def write_initial_prompt(run_id: str | None, backend: _Backend, prompt: str) -> None:
  """Write the Run prompt as the first JSON message and keep stdin open."""
  if run_id is None:
    raise ValueError("a gated web manager turn needs its Run id")
  state = _RUNS[run_id]
  async with state.lock:
    if state.initial_written.is_set():
      raise RuntimeError(f"Run {run_id} already wrote its initial stream-json prompt")
    process = backend._proc
    if process is None or process.stdin is None:
      raise RuntimeError(f"Run {run_id} has no subprocess stdin for its initial prompt")
    state.process = process
    state.stdin = process.stdin
    try:
      state.stdin.write(_user_line(run_id, prompt))
      await state.stdin.drain()
    except BaseException as error:
      state.write_error = error
      raise
    finally:
      state.initial_written.set()


async def offer(meta: models.SessionMetadata, run: models.RunRecord, pending: list[dict]) -> list[str]:
  """Write each eligible pending web-user event once to the live Run."""
  if not is_gated_turn(meta, turn=run.kind == "manager_turn", run_kind=run.kind):
    return []
  state = _RUNS.get(run.id)
  if state is None:
    return []
  await state.initial_written.wait()
  if state.write_error is not None:
    raise state.write_error
  written: list[str] = []
  async with state.lock:
    if state.released or state.closed:
      return written
    process = state.process
    stdin = state.stdin
    if process is None or stdin is None or process.returncode is not None or stdin.is_closing():
      return written
    for event in pending:
      if event.get("type") != ET.USER:
        continue
      event_id = str(event["id"])
      if event_id in state.attempted_ids:
        continue
      state.attempted_ids.add(event_id)
      content, _uploads = task_input_prompt.compose_input_prompt(meta, [event])
      stdin.write(_user_line(event_id, content))
      state.written_ids.add(event_id)
      await stdin.drain()
      written.append(event_id)
  return written


def translate_replay(run_id: str | None, event: dict) -> list[dict]:
  """Turn a Claude replay of a delivered user message into a non-rendered ack."""
  if event.get("type") != ET.USER or event.get("isReplay") is not True:
    return [event]
  if run_id is None:
    raise ValueError("a replayed Claude user message has no Run id")
  input_id = event.get("uuid")
  if not isinstance(input_id, str) or not input_id:
    raise ValueError("a replayed Claude user message has no uuid")
  if input_id == run_id:
    return []
  state = _RUNS.get(run_id)
  if state is not None and input_id not in state.written_ids:
    raise RuntimeError(f"Run {run_id} replayed user input {input_id} without a matching stdin write")
  return [
      {
          "type": ET.INPUT_DELIVERED,
          ET.INPUT_EVENT_ID: input_id,
          "run_id": run_id,
          "timestamp": models.utc_now_iso(),
      }
  ]


async def on_backend_event(run_id: str | None, event: dict) -> None:
  """Close gated stdin under the write lock at the first result event."""
  if run_id is None or event.get("type") != ET.RESULT:
    return
  state = _RUNS[run_id]
  await state.initial_written.wait()
  async with state.lock:
    if state.closed or state.released:
      return
    if state.stdin is None:
      raise RuntimeError(f"Run {run_id} has no subprocess stdin to close")
    state.closed = True
    if not state.stdin.is_closing():
      state.stdin.close()
      await state.stdin.wait_closed()


async def record_ack(item: _WorkItem | None, event: dict) -> None:
  """Add one replayed user id to the live manager item's web-reply binding."""
  if item is None or event.get("type") != ET.INPUT_DELIVERED:
    return
  run_id = str(event["run_id"])
  if run_id != item.task_run.run_id:
    raise RuntimeError(f"input delivery for Run {run_id} reached manager Run {item.task_run.run_id}")
  input_id = str(event[ET.INPUT_EVENT_ID])
  if input_id not in item.user_event_ids:
    item.user_event_ids.append(input_id)


def _echoed_input_ids(run_dir: pathlib.Path, run_id: str) -> set[str]:
  """Read non-initial replay uuids from every raw transport leg of this Run."""
  echoed: set[str] = set()
  for raw_path in sorted(run_dir.glob(f"{runs.RAW_LOG_NAME}*")):
    with raw_path.open("rb") as raw_file:
      for line in raw_file:
        if not line.endswith(b"\n"):
          break
        event = ndjson.parse_ndjson_line(line[:-1], log_event="raw_line_not_json", log_fields={})
        if event is None or event.get("type") != ET.USER or event.get("isReplay") is not True:
          continue
        input_id = event.get("uuid")
        if isinstance(input_id, str) and input_id and input_id != run_id:
          echoed.add(input_id)
  return echoed


async def claim_finish_inputs(
    meta: models.SessionMetadata,
    dispatcher: _Dispatcher,
    run_id: str,
    transport_dir: pathlib.Path,
    *,
    successful: bool,
) -> list[str]:
  """Claim only pending user events whose uuids the successful Run echoed."""
  if not is_gated_turn(meta, turn=True, run_kind="manager_turn") or not successful:
    return []
  # The scan parses every raw line of the Run, so it rides a thread like the
  # round-notice projection of the same files: on the loop it would freeze
  # every concurrent request and WebSocket at turn end.
  echoed_ids = await asyncio.to_thread(_echoed_input_ids, transport_dir, run_id)
  if not echoed_ids:
    return []
  return await dispatcher.claim_echoed_user_inputs(meta.id, run_id, echoed_ids)


def release_run(run_id: str) -> None:
  """Release process state after the Run's terminal fact is recorded."""
  state = _RUNS.pop(run_id, None)
  if state is not None:
    state.released = True
    state.initial_written.set()


def fresh_translate(
    cfg: config.CharlieBotConfig,
    option: models.BackendOption | None,
    meta: models.SessionMetadata,
    run_id: str,
) -> Any:
  """Build the fresh raw-log translator used to follow or reconcile one Run."""
  if option is None:
    return lambda event: [event]
  launch = {}
  if option.type in _CLAUDE_BACKEND_TYPES and is_gated_turn(meta, turn=True, run_kind="manager_turn"):
    launch = launch_kwargs(meta, turn=True, run_kind="manager_turn", run_id=run_id)
  try:
    return backend_types.build_backend(option, cfg, **launch).translate_event
  except Exception as error:
    log.warning("master_cc_resume_translate_unresolved", backend=option.id, error=str(error))
    return lambda event: [event]

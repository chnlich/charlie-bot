"""Deterministic synthetic home for the parity harness, written through the Python server's own writers.

Usage: python fixture.py --manifest PATH, with CHARLIEBOT_HOME naming a home that holds only the harness's
config.yaml and credentials.yaml. The harness runs this as a process under test (sandboxed and traced).

Every write goes through the server's writer functions: the task tree owner (node creation, archive, restore),
the run store (registration, launch, observation, stop, finish), the input dispatcher (inputs, claims, child
reports), the completion owner, the execution adapter (failure reports, launch snapshots, overlay alerts), the
session lifecycle (star, unread, history recycle), the fork, the plan registry and the trigger store. Chat lines
use the agent-process event builders where one exists. Two runs of this script produce byte-identical homes up
to the home path: uuid4 draws from a seeded generator, the model clock steps from a fixed epoch, and the last
step pins every file and directory time in the home to the clock's final instant (the server derives ETags and
memo revisions from file times).

The home it leaves is a fixed point of the server's startup recovery: no open node holds a pending input or a
launchable queued run, except the node whose stale backend pin makes the dispatch refuse before it writes. The
script runs the real reconcile pass at the end and fails when that pass would write a byte or launch a run.
"""

import argparse
import asyncio
import datetime
import hashlib
import importlib
import json
import os
import pathlib
import random
import sys
import uuid

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
SEED = 20260105
EPOCH = datetime.datetime(2026, 1, 5, 9, 0, tzinfo=datetime.UTC)
CLOCK_STEP = datetime.timedelta(seconds=7)
# Above the kernel's PID_MAX_LIMIT (2**22): no live process ever holds these pids, so every launched fixture
# run reads as ended.
DEAD_PID_BASE = 4_194_400
DEAD_PID_START = "1"
# A backend id no configured option defines: the node pinned to it refuses dispatch before any write.
RETIRED_BACKEND = "claude-retired"
MISSING_SESSION_ID = "00000000-0000-4000-8000-00000000dead"
MISSING_RUN_ID = "00000000-0000-4000-8000-00000000beef"
FAR_FUTURE = datetime.datetime(2099, 1, 1, tzinfo=datetime.UTC)
# Event types that exist only in the worker transcript projection, never in an events file.
_PROJECTION_ONLY_TYPES = frozenset({"run_header", "run_end", "run_error", "run_delivery"})


class _FixtureClock:
  """The model clock: every read advances one step from the fixed epoch."""

  def __init__(self) -> None:
    self._now = EPOCH

  def now(self) -> datetime.datetime:
    self._now += CLOCK_STEP
    return self._now


CLOCK = _FixtureClock()
_RNG = random.Random(SEED)


def _seeded_uuid4() -> uuid.UUID:
  return uuid.UUID(int=_RNG.getrandbits(128), version=4)


def _home_bytes(home: pathlib.Path) -> dict[str, bytes]:
  return {str(path.relative_to(home)): path.read_bytes() for path in sorted(home.rglob("*")) if path.is_file()}


class Builder:
  """Writes the fixture through the server's singletons and records the manifest the scenarios read."""

  def __init__(self) -> None:
    from src.features.artifacts import api as artifacts_api
    from src.infra import config
    from src.runtime import (
        run_token,
        session_events,
        session_fork,
        session_lifecycle,
        session_store,
        task_execution,
        triggers,
    )

    self.cfg = config.get_config()
    self.option = self.cfg.backends.options[0]
    self.tree = task_execution.task_manager()
    # The adapter stays detached from the dispatcher: a dispatch here records its decision and launches nothing.
    self.adapter = self.tree.dispatch.executor
    self.tree.dispatch.executor = None
    self.store = session_store.store()
    self.events = session_events.events()
    self.lifecycle = session_lifecycle.lifecycle()
    self.fork = session_fork.fork()
    self.plans = artifacts_api.plan_manager()
    self.triggers = triggers.TriggerManager(self.cfg, self.tree)
    self.operator = run_token.CallerIdentity("operator")
    self.manifest: dict = {
        "sessions": {},
        "runs": {},
        "inputs": {},
        "missing_session": MISSING_SESSION_ID,
        "missing_run": MISSING_RUN_ID
    }
    self._pids = 0

  def sid(self, key: str) -> str:
    return self.manifest["sessions"][key]

  # -- nodes -------------------------------------------------------------

  async def node(
      self,
      key: str,
      name: str,
      *,
      profile: str = "manager",
      parent: str | None = None,
      task: object = None,
      group: str | None = None,
      backend: str | None = None,
      request_id: str | None = None,
      slot_values: dict | None = None) -> str:
    """Create one task node; its created_at and updated_at come from the model clock."""
    meta = await self.tree.create_task(
        request_id=request_id or f"fixture-{key}",
        task_parent_id=self.sid(parent) if parent else None,
        profile=profile,
        task=task,
        name=name,
        backend=backend or self.option.id,
        group=group,
        slot_values=slot_values,
        caller=self.operator)
    await self._stamp_times(meta.id)
    self.manifest["sessions"][key] = meta.id
    return meta.id

  async def _stamp_times(self, session_id: str) -> None:
    """The model's created_at default binds the real clock; restamp both times from the fixture clock."""
    async with self.store.lock_for(session_id):
      fresh = await self.store.get_session_bypassing_cache(session_id)
      fresh.created_at = fresh.updated_at = CLOCK.now()
      await self.store.save_metadata(fresh, lock_held=True)
    self.tree.invalidate_tree_index()

  async def worker(self, key: str, parent: str, name: str, goal: str, task_type: str) -> tuple[str, str]:
    """A delegated worker node and its first work Run, registered the way the delegate route does it."""
    from src.infra import event_types as ET
    from src.infra import models
    from src.runtime import control_events, task_sessions

    parent_id = self.sid(parent)
    task = models.TaskSpec(goal=goal, task_type=task_type, acceptance=["The report names every source checked."])
    request_id = control_events.derived_delegate_request_id(parent_id, task_type, goal)
    sid = await self.node(key, name, profile="worker", parent=parent, task=task, request_id=request_id)
    run_id = control_events.stable_run_id(sid, f"{request_id}:work")
    record = models.RunRecord(id=run_id, session_id=sid, kind="work", backend=self.option.id, model=self.option.model)
    async with self.tree.control_lock:
      await self.tree.runs.register_run_locked(record, task_spec_text=task_sessions.canonical_task_spec_text(task))
    await self.events.persist_and_broadcast(
        parent_id, {
            "type": ET.TASK_DELEGATED,
            "thread_id": run_id,
            "description": goal,
            "backend": self.option.id,
            "model": self.option.model,
            "child_session_id": sid,
            ET.DELEGATE_INVOCATION:
                {
                    "task_type": task_type,
                    "repo_path": None,
                    "base_branch": None,
                    "task_spec_file": None,
                    "reviewer_context_file": None,
                    "keep_worktree": False,
                    "backend": None,
                },
        })
    return sid, run_id

  # -- chat --------------------------------------------------------------

  async def say(self, session_id: str, *events: dict) -> list[str]:
    """Append chat lines through the events block's one append funnel; returns their ids."""
    ids = []
    for event in events:
      payload = dict(event)
      await self.events.save_chat_event(session_id, payload)
      ids.append(payload["id"])
    return ids

  async def admit(self, key: str, session_id: str, event_type: str, content: str, **fields: object) -> str:
    actor = {"user": "user", "scheduled_trigger": "system"}.get(event_type, "agent")
    event = await self.tree.dispatch.admit_input(
        session_id, event_type=event_type, content=content, actor=actor, **fields)
    self.manifest["inputs"][key] = event["id"]
    return event["id"]

  # -- runs --------------------------------------------------------------

  def _record_run(self, key: str, session_id: str, run_id: str) -> None:
    self.manifest["runs"][key] = {"session": session_id, "run": run_id}

  async def _launch(self, session_id: str, run_id: str) -> None:
    self._pids += 1
    await self.tree.runs.record_launch(session_id, run_id, pid=DEAD_PID_BASE + self._pids, pid_start=DEAD_PID_START)

  def _write_raw_log(self, session_id: str, run_id: str, native: str, events: list[dict]) -> pathlib.Path:
    from src.infra import json_utils
    from src.runtime import runs

    path = self.tree.runs.run_dir(session_id, run_id) / runs.RAW_LOG_NAME
    lines = [{"type": "system", "subtype": "init", "session_id": native}, *events]
    json_utils.atomic_write_text(path, "".join(json.dumps(line) + "\n" for line in lines))
    return path

  async def manager_turn(
      self,
      key: str,
      session_id: str,
      turn_events: list[dict],
      *,
      thinking_seconds: int | None,
      outcome: str = "success") -> str:
    """One manager turn that consumes the node's pending batch, reserved the way the dispatcher reserves it."""
    from src.infra import event_types as ET
    from src.infra import models
    from src.runtime import control_events
    from src.runtime.agent_process import base

    tree = self.tree
    async with tree.control_lock:
      pending = tree.dispatch.pending_inputs(session_id)
      if not pending:
        raise RuntimeError(f"manager turn {key}: session {session_id} has no pending input to consume")
      request_id = "dispatch:" + control_events.sha256_hex("\x00".join(sorted(str(e.get("id")) for e in pending)))
      run_id = control_events.stable_run_id(session_id, request_id)
      record = models.RunRecord(
          id=run_id, session_id=session_id, kind="manager_turn", backend=self.option.id, model=self.option.model)
      await tree.runs.register_run_locked(record, task_spec_text=None)
      claimed = await tree.dispatch.claim_input_batch_locked(session_id, run_id)
    await self._launch(session_id, run_id)
    native = f"native-{key}"
    exit_code = 0 if outcome == "success" else 1
    done = base.make_master_done_event(exit_code, still_thinking=thinking_seconds is None)
    done[ET.INPUT_EVENT_IDS] = claimed
    if thinking_seconds is not None:
      done[ET.THINKING_SECONDS] = thinking_seconds
    await self.say(session_id, {"type": ET.SESSION_ATTACHED, "session_id": native}, *turn_events, done)
    raw = self._write_raw_log(session_id, run_id, native, turn_events)
    await tree.runs.record_observation(
        session_id,
        run_id,
        native_session_id=native,
        model=self.option.model,
        raw_log_ref=str(raw),
        result_ref=str(raw))
    await tree.dispatch.finish_run(session_id, run_id, outcome=outcome, exit_code=exit_code, ended_at=CLOCK.now())
    self._record_run(key, session_id, run_id)
    return run_id

  async def work_run(
      self,
      key: str,
      session_id: str,
      run_id: str,
      run_events: list[dict],
      *,
      outcome: str,
      snapshot: bool = False,
      launch_text: str | None = None) -> None:
    """Launch, observe and finish one registered worker Run, writing its transcript and raw log."""
    from src.infra import event_types as ET
    from src.infra import json_utils, ndjson
    from src.runtime import runs, task_prompts

    tree = self.tree
    run = await tree.runs.get_run(session_id, run_id)
    if snapshot:
      meta = await tree.load_meta(session_id)
      await self.adapter._prepare_launch_snapshot(meta, run, self.option)
    if launch_text is not None:
      json_utils.atomic_write_text(
          tree.runs.run_dir(session_id, run_id) / task_prompts.LAUNCH_TEXT_FILENAME, launch_text)
    await self._launch(session_id, run_id)
    native = f"native-{key}"
    events_log = tree.runs.run_dir(session_id, run_id) / runs.RUN_EVENTS_NAME
    for event in [{"type": ET.SESSION_ATTACHED, "session_id": native}, *run_events]:
      await ndjson.append_ndjson(events_log, {"timestamp": CLOCK.now().isoformat(), **event})
    raw = self._write_raw_log(session_id, run_id, native, run_events)
    await tree.runs.record_observation(
        session_id,
        run_id,
        native_session_id=native,
        model=self.option.model,
        raw_log_ref=str(raw),
        events_ref=str(events_log),
        result_ref=str(raw))
    exit_code = 0 if outcome == "success" else 1
    await tree.dispatch.finish_run(session_id, run_id, outcome=outcome, exit_code=exit_code, ended_at=CLOCK.now())
    self._record_run(key, session_id, run_id)

  async def report_failure(self, session_id: str, run_id: str, outcome: str) -> None:
    """The execution adapter's failure report to the parent, with its archive entry."""
    run = await self.tree.runs.get_run(session_id, run_id)
    await self.adapter._report_failure_to_parent(session_id, run, outcome)


def _usage(prompt: int, output: int, *, cache_read: int = 0, cache_write: int = 0) -> dict:
  return {
      "input_tokens": prompt,
      "output_tokens": output,
      "cache_read_input_tokens": cache_read,
      "cache_creation_input_tokens": cache_write,
  }


def _assistant(*blocks: dict, usage: dict | None = None) -> dict:
  message: dict = {"role": "assistant", "model": "claude-sonnet-5", "content": list(blocks)}
  if usage is not None:
    message["usage"] = usage
  return {"type": "assistant", "message": message}


def _tool_result_echo(tool_use_id: str, text: str, *, is_error: bool = False) -> dict:
  """The Claude CLI's user-typed echo of one tool result (tool output, never a message)."""
  return {
      "type": "user",
      "message":
          {
              "role":
                  "user",
              "content":
                  [
                      {
                          "type": "tool_result",
                          "tool_use_id": tool_use_id,
                          "content": [{
                              "type": "text",
                              "text": text
                          }],
                          "is_error": is_error
                      }
                  ]
          }
  }


async def build_atlas(b: Builder) -> None:
  """The root manager whose chat carries every rendered event type, with a delegated child chain."""
  from src.infra import event_types as ET
  from src.runtime.agent_process import base

  atlas = await b.node("atlas", "Atlas planning", group="Research")
  survey = await b.node("atlas_survey", "Atlas survey", parent="atlas")
  await b.lifecycle.star_session(atlas)

  uploads = b.cfg.charliebot_home / "uploads"
  uploads.mkdir(parents=True, exist_ok=True)
  stations = uploads / "stations.csv"
  stations.write_text("station,lat,lon\nnorth-pier,10.5,20.25\nsouth-pier,10.75,20.5\n", encoding="utf-8")
  await b.admit(
      "atlas_u1",
      atlas,
      ET.USER,
      "Draft a plan for the Atlas survey of tide gauges.",
      uploaded_files=[{
          "filename": "stations.csv",
          "path": str(stations)
      }])
  await b.manager_turn(
      "atlas_t1",
      atlas, [
          _assistant(
              {
                  "type": "thinking",
                  "thinking": "Listing the gauge sources first.",
                  "signature": "sig-parity-1"
              },
              usage=_usage(1200, 40, cache_read=30000, cache_write=800)),
          _assistant(
              {
                  "type": "text",
                  "text": "I will check the station list."
              }, {
                  "type": "tool_use",
                  "id": "toolu_parity_01",
                  "name": "Bash",
                  "input":
                      {
                          "command": "ls stations/",
                          "description": "List every station file in the synthetic survey directory " * 2,
                      }
              },
              usage=_usage(1300, 60, cache_read=31000)),
          _tool_result_echo("toolu_parity_01", "north-pier.csv\nsouth-pier.csv"),
          _assistant(
              {
                  "type": "tool_use",
                  "id": "toolu_parity_02",
                  "name": "Read",
                  "input": {
                      "file_path": "/srv/atlas/notes.md"
                  }
              }),
          _tool_result_echo("toolu_parity_02", "notes.md: no such file", is_error=True),
          _assistant(
              {
                  "type": "tool_use",
                  "id": "toolu_parity_03",
                  "name": "ExitPlanMode",
                  "input": {
                      "plan": "1. Collect the gauge notes.\n2. Probe the mirrors.\n3. Merge the readings."
                  }
              }),
          _assistant(
              {
                  "type": "text",
                  "text": "The plan has three steps; the survey child takes the first two."
              },
              usage=_usage(1500, 120, cache_read=32000)),
          base.make_result_event(1500, 420, cache_read=93000, cache_creation=800, cost=0.0421),
      ],
      thinking_seconds=12)

  await b.say(
      atlas,
      {
          "type": ET.HANDLER_RESULT,
          "task": "nightly-digest",
          "status": ET.HANDLER_STATUS_OK,
          "message": "digest posted"
      },
      {
          "type": ET.HANDLER_RESULT,
          "task": "nightly-digest",
          "status": ET.HANDLER_STATUS_ERROR,
          "message": "digest source unreachable"
      },
      {
          "type": ET.RESUME_CONTEXT_DROPPED,
          "reason": ET.RESUME_REASON_ANCHOR_MISSING
      },
      {
          "type": ET.RESUME_CONTEXT_DROPPED,
          "reason": ET.RESUME_REASON_TRANSCRIPT_MISSING
      },
      {
          "type": ET.BACKEND_SWITCHED,
          "from": "claude-opus",
          "to": "claude-sonnet",
          "previous_native_backend": "claude-opus",
          "previous_native_session_id": "native-atlas_t1"
      },
      {
          "type": ET.BACKEND_OVERLAY_UNDECLARED,
          "backend": "claude-sonnet"
      },
      {
          "type": ET.MODEL_FALLBACK_NOTICE,
          "backend": "claude-sonnet",
          "configured_model": "claude-sonnet-5",
          "served_models": ["claude-haiku-5"]
      },
      {
          "type": ET.WORKER_SUMMARY,
          "content": "Legacy worker summary: the gauge list is complete.",
          "thread_id": "thread-legacy-atlas",
          "origin_session_id": survey
      },
      {
          "type": ET.WEB_REPLY,
          "content": "Posted the outline to the reader.",
          ET.WEB_REPLY: {
              "answers": [b.manifest["inputs"]["atlas_u1"]],
              "chars": 33
          }
      },
      {
          "type": ET.WEB_REPLY_NOTICE,
          "content": "No reply was posted for this message.",
          ET.WEB_REPLY_NOTICE: {
              ET.INPUT_EVENT_ID: b.manifest["inputs"]["atlas_u1"]
          }
      },
      {
          "type": ET.WEB_STATUS,
          "content": "Status: drafting",
          ET.WEB_STATUS: {
              "drafting": "the outline"
          },
          "web_status_changed": ["drafting"]
      },
      {
          "type": "slack_reply",
          "content": "Thread update posted."
      },
      {
          "type": "discord_reply",
          "content": "Channel update posted."
      },
      {"type": "claude_account_login_required"},
      {
          "type": ET.SCHEDULED_RUN_SKIPPED,
          "task": "nightly-digest",
          "skipped_at": "2026-01-05T09:30:00+00:00",
          "reason": "previous run still active"
      },
      {
          "type": ET.RATE_LIMIT_EVENT,
          ET.RATE_LIMIT_INFO: {
              "status": "allowed_warning",
              "resetsAt": 1767700000
          }
      },
      {
          "type": ET.QUOTA_FAILOVER,
          "from": "claude-sonnet",
          "to": "claude-haiku",
          "recovers_at": "2026-01-05T10:00+00:00",
      },
      {
          "type": ET.WEB_ASK_CHECK,
          "content": "Cold read of a Waiting on you item",
          "item_sha256": hashlib.sha256(b"synthetic waiting item").hexdigest(),
          "backend": "claude-sonnet",
          "answer": "The panel should use the compact layout.",
      },
  )
  await b.adapter._alert_overlay_inactive(atlas, b.option, None)
  await b.adapter._alert_overlay_inactive(atlas, b.option, FileNotFoundError(2, "overlay missing"))

  await b.admit(
      "atlas_a1",
      atlas,
      ET.AGENT_MESSAGE,
      "The survey child finished its outline.",
      from_session=survey,
      from_session_name="Atlas survey")
  await b.manager_turn(
      "atlas_t2",
      atlas, [
          {
              "type": ET.THINKING,
              "content": "Weighing two survey orders."
          },
          base.make_tool_use_event("Grep", {
              "pattern": "gauge",
              "path": "/srv/atlas"
          }),
          base.make_tool_result_event(
              "Grep", "".join(f"/srv/atlas/gauge-{i:03d}.csv: gauge reading\n" for i in range(40))),
          base.make_text_event("The outline is in; the readings need a merge pass."),
          {
              "type": ET.SYSTEM,
              "subtype": ET.COMMAND_PROGRESS,
              "content": "Still running: merge-readings (2m)"
          },
          base.make_compact_boundary_event("auto", 150000),
          base.make_context_compacted_event(
              "auto", {
                  "trigger": "auto",
                  ET.COMPACT_PRE_TOKENS: 150000,
                  ET.COMPACT_POST_TOKENS: 32000
              }, "claude-sonnet-5"),
          base.make_context_compact_failed_event("summary exceeded the output limit", None),
          base.make_context_reading_event("claude-sonnet-5", 64000, 200000, 160000),
          base.make_result_event(2100, 300, cache_read=60000, cost=0.0187),
      ],
      thinking_seconds=None)

  await b.admit("atlas_s1", atlas, ET.SCHEDULED_TRIGGER, "Weekly Atlas digest is due.")
  await b.manager_turn(
      "atlas_t3",
      atlas, [
          {
              "type": ET.ASSISTANT_ERROR,
              "content": "Backend returned HTTP 529 (overloaded)"
          },
          base.make_error_event("Rate limited by the synthetic upstream"),
      ],
      thinking_seconds=3,
      outcome="failed")

  ack = await b.admit(
      "atlas_a2",
      atlas,
      ET.AGENT_MESSAGE,
      "A duplicate digest request.",
      from_session=survey,
      from_session_name="Atlas survey")
  await b.tree.completion.acknowledge_inputs(
      atlas, request_id="fixture-ack-atlas", input_ids=[ack], note="handled in the survey thread", caller=b.operator)
  await build_survey(b)


async def build_survey(b: Builder) -> None:
  """The child manager: three delegated workers that end failed-then-retried, interrupted, and stopped."""
  from src.infra import event_types as ET
  from src.runtime.agent_process import base

  survey = b.sid("atlas_survey")
  await b.admit("survey_u1", survey, ET.USER, "Split the survey into notes, probe and scout work.")
  notes, notes_r1 = await b.worker(
      "atlas_notes", "atlas_survey", "Collect survey notes", "Collect survey notes from the three gauge archives.",
      "quick-edit")
  probe, probe_r1 = await b.worker(
      "atlas_probe", "atlas_survey", "Probe gauge mirrors", "Probe the gauge mirrors for stale readings.", "verify")
  scout, scout_r1 = await b.worker(
      "atlas_scout", "atlas_survey", "Scout mirror latency", "Scout mirror latency with the timing script.",
      "script-run")
  await b.manager_turn(
      "survey_t1",
      survey,
      [base.make_text_event("Delegated notes, probe and scout work."),
       base.make_result_event(900, 80, cost=0.006)],
      thinking_seconds=5)

  await b.work_run(
      "notes_r1",
      notes,
      notes_r1, [
          base.make_text_event("Opening the first archive."),
          base.make_error_event("synthetic archive host refused the connection"),
          base.make_result_event(400, 20, cost=0.002) | {
              "subtype": "error_during_execution",
              "is_error": True
          },
      ],
      outcome="failed",
      launch_text="Collect survey notes from the three gauge archives.\n")
  retry = await b.tree.create_retry(notes, "fixture-notes-retry", notes_r1)
  await b.report_failure(notes, notes_r1, "failed")
  await b.work_run(
      "notes_r2",
      notes,
      retry["run_id"], [
          _assistant(
              {
                  "type": "text",
                  "text": "Collected the notes from all three archives."
              },
              usage=_usage(800, 90, cache_read=12000)),
          base.make_tool_use_event("Write", {
              "file_path": "/srv/atlas/notes/summary.md",
              "content": "summary " * 120
          }),
          base.make_tool_result_event("Write", "wrote /srv/atlas/notes/summary.md"),
          base.make_context_reading_event("claude-sonnet-5", 21000, 200000, 160000),
          base.make_result_event(800, 240, cache_read=24000, cost=0.0093),
      ],
      outcome="success",
      snapshot=True)

  await b.work_run(
      "probe_r1",
      probe,
      probe_r1, [
          base.make_text_event("Probing the first mirror."),
          base.make_tool_use_event("Bash", {"command": "curl -sI https://mirror.invalid/"})
      ],
      outcome="interrupted",
      launch_text="Probe the gauge mirrors for stale readings.\n")
  await b.report_failure(probe, probe_r1, "interrupted")

  await b.tree.runs.request_stop(scout, scout_r1, "fixture-scout-stop")
  b._record_run("scout_r1", scout, scout_r1)

  await b.manager_turn(
      "survey_t2",
      survey, [
          base.make_text_event("Notes delivered; the probe was interrupted and archived."),
          base.make_result_event(1100, 140, cost=0.0071)
      ],
      thinking_seconds=9)


async def build_beacon(b: Builder) -> None:
  """An archived root and its archived child; the root stays starred."""
  from src.infra import event_types as ET
  from src.runtime.agent_process import base

  beacon = await b.node("beacon", "Beacon notes", group="Writing")
  await b.admit("beacon_u1", beacon, ET.USER, "Outline the beacon maintenance log.")
  await b.manager_turn(
      "beacon_t1",
      beacon, [base.make_text_event("The log has four sections."),
               base.make_result_event(600, 70, cost=0.004)],
      thinking_seconds=4)
  draft = await b.node("beacon_draft", "Beacon draft outline", parent="beacon")
  await b.admit("beacon_draft_u1", draft, ET.USER, "Draft the first section.")
  await b.manager_turn(
      "beacon_draft_t1",
      draft, [base.make_text_event("Section one drafted."),
              base.make_result_event(500, 60, cost=0.003)],
      thinking_seconds=6)
  await b.lifecycle.star_session(beacon)
  await b.tree.completion.archive_subtree(
      beacon, request_id="fixture-archive-beacon", summary="archived by the user", result_refs=[], run_ids=[],
      caller=b.operator)


async def build_cobalt(b: Builder) -> None:
  """A root pinned to a retired backend: its pending input stays pending because dispatch refuses first."""
  from src.infra import event_types as ET

  cobalt = await b.node("cobalt", "Cobalt ideas", group="Research", backend=RETIRED_BACKEND)
  await b.admit("cobalt_u1", cobalt, ET.USER, "Collect three cobalt pigment suppliers.")
  await b.lifecycle.mark_unread(cobalt)


async def build_delta(b: Builder) -> None:
  """A chat-thread session: its Slack origin moves it from the Workspace list to the Threads view."""
  from src.infra import event_types as ET
  from src.runtime.agent_process import base

  delta = await b.node(
      "delta",
      "Delta thread",
      slot_values={"slack_origin": {
          "team_id": "T0PARITY",
          "channel_id": "C0PARITY",
          "thread_ts": "1767600000.000100"
      }})
  await b.admit("delta_u1", delta, ET.USER, "What changed in the delta release notes?")
  await b.manager_turn(
      "delta_t1",
      delta, [
          base.make_text_event("Two fixes and one new flag."), {
              "type": "slack_reply",
              "content": "Two fixes and one new flag."
          },
          base.make_result_event(700, 50, cost=0.004)
      ],
      thinking_seconds=7)


async def build_ember(b: Builder) -> None:
  """A root with recycled history (archive_offset > 0) that was archived and restored."""
  from src.infra import event_types as ET
  from src.runtime.agent_process import base

  ember = await b.node("ember", "Ember history", group="Writing")
  for turn in range(4):
    await b.admit(f"ember_u{turn}", ember, ET.USER, f"Summarize ember log week {turn + 1}.")
    await b.manager_turn(
        f"ember_t{turn}",
        ember, [
            base.make_text_event(f"Week {turn + 1}: steady readings, no gaps."),
            base.make_result_event(300 + turn, 40, cost=0.002)
        ],
        thinking_seconds=2 + turn)
    if turn == 1:
      b.manifest["ember_cutoff"] = CLOCK.now().isoformat()
  cutoff = datetime.datetime.fromisoformat(b.manifest["ember_cutoff"])
  result = await b.lifecycle.recycle_history_before(ember, cutoff)
  if not result["events_archived"]:
    raise RuntimeError("ember history recycle archived nothing")
  await b.tree.completion.archive_subtree(
      ember, request_id="fixture-archive-ember", summary="archived by the user", result_refs=[], run_ids=[],
      caller=b.operator)
  await b.tree.completion.restore_task(
      ember, request_id="fixture-ember-restore", reason="fixture restore", caller=b.operator)


async def build_fable(b: Builder) -> None:
  """A root with a plan registry, pending and settled triggers, and the unread mark."""
  from src.infra import event_types as ET
  from src.infra import models
  from src.runtime.agent_process import base

  fable = await b.node("fable", "Fable roadmap")
  await b.admit("fable_u1", fable, ET.USER, "Sketch the fable roadmap for next quarter.")
  await b.manager_turn(
      "fable_t1",
      fable, [base.make_text_event("Roadmap sketched as two plans."),
              base.make_result_event(650, 90, cost=0.005)],
      thinking_seconds=8)
  artifacts = b.cfg.sessions_dir / fable / "artifacts"
  artifacts.mkdir()
  for name, title in (("roadmap-v1.html", "Fable roadmap"), ("roadmap-alt.html", "Fable roadmap, alternative")):
    (artifacts / name).write_text(f"<!doctype html><title>{title}</title><h1>{title}</h1>\n", encoding="utf-8")

  def version(file: str) -> dict:
    return {
        "v": 1,
        "file": file,
        "created_at": CLOCK.now().isoformat(),
        "trigger": "initial",
        "base": None,
        "note": None
    }

  # The presenting verb runs the plan assertions through headless Chrome; the registry writes through the same
  # save funnel the verbs use, and the approve and close verbs then run as they do on the server.
  await b.plans._save(
      fable, {
          "plans":
              [
                  {
                      "id": 1,
                      "title": "Fable roadmap",
                      "versions": [version("artifacts/roadmap-v1.html")],
                      "takeoff": None,
                      "closed": None
                  },
                  {
                      "id": 2,
                      "title": "Fable roadmap, alternative",
                      "versions": [version("artifacts/roadmap-alt.html")],
                      "takeoff": None,
                      "closed": None
                  },
              ]
      })
  await b.plans.approve(fable, 1)
  await b.plans.close(fable, 2, "superseded")

  for message, status, fired_at, reason in (
      ("Check the roadmap review again.", models.TriggerStatus.PENDING, None, None),
      ("Remind about the roadmap draft.", models.TriggerStatus.FIRED, CLOCK.now(), "timeout"),
      ("Old reminder that was withdrawn.", models.TriggerStatus.CANCELLED, None, "cancelled by the user"),
  ):
    fire_at = FAR_FUTURE if status is models.TriggerStatus.PENDING else CLOCK.now()
    await b.triggers._save_trigger(
        models.PendingTrigger(
            session_id=fable,
            fire_at=fire_at,
            message=message,
            created_at=CLOCK.now(),
            status=status,
            fired_at=fired_at,
            fire_reason=reason))
  await b.lifecycle.mark_unread(fable)


async def build_garnet(b: Builder) -> None:
  """A fork of the atlas root: its log opens with the copied history and a clone_start marker."""
  meta = await b.fork.fork_session(b.sid("atlas"), event_index=None, backend=b.option.id)
  b.manifest["sessions"]["garnet"] = meta.id
  await b._stamp_times(meta.id)


def check_event_coverage(b: Builder) -> None:
  """Every type the message aggregator renders appears in some fixture chat log or run transcript."""
  from src.infra import event_types as ET
  from src.infra import ndjson
  from src.runtime import message_aggregator

  wanted = set(
      message_aggregator._render_handlers()) | {ET.USER, ET.ASSISTANT, ET.TOOL_USE, ET.TOOL_RESULT, ET.THINKING}
  wanted -= _PROJECTION_ONLY_TYPES
  seen: set[str] = set()
  for path in b.cfg.sessions_dir.rglob("*.jsonl"):
    for event in ndjson.parse_ndjson_file(path):
      seen.add(event.get("type"))
  missing = sorted(wanted - seen)
  if missing:
    raise RuntimeError(f"fixture chat logs miss rendered event types: {missing}")


async def check_recovery_fixed_point(b: Builder) -> None:
  """Run the server's startup reconcile on the finished home: it must write nothing and launch nothing.

  Nodes with an unconfigured backend may reach the executor: backend resolution raises before the reservation writes
  anything.
  """
  from src.runtime import task_recovery

  before = _home_bytes(b.cfg.charliebot_home)
  launches: list[str] = []
  adapter = b.adapter
  refused_node = b.sid("cobalt")

  async def guarded_executor(session_id: str, pending: list[dict], *, launch_run_id: str | None = None) -> str:
    if session_id == refused_node:
      return await adapter(session_id, pending, launch_run_id=launch_run_id)
    launches.append(session_id)
    raise RuntimeError(f"startup recovery would launch a run on {session_id}")

  b.tree.dispatch.executor = guarded_executor
  try:
    await task_recovery.reconcile_task_tree(b.cfg, b.tree, adapter)
  finally:
    b.tree.dispatch.executor = None
  after = _home_bytes(b.cfg.charliebot_home)
  if launches:
    raise RuntimeError(f"startup recovery would launch runs on: {sorted(set(launches))}")
  changed = sorted(path for path in before.keys() | after.keys() if before.get(path) != after.get(path))
  if changed:
    raise RuntimeError(f"startup recovery would change fixture files: {changed}")


def pin_times(home: pathlib.Path, instant: datetime.datetime) -> None:
  """Set the access and modification time of every entry in *home* to *instant*, children before parents."""
  stamp = int(instant.timestamp()) * 1_000_000_000
  for dirpath, dirnames, filenames in os.walk(home, topdown=False):
    for name in [*filenames, *dirnames]:
      os.utime(os.path.join(dirpath, name), ns=(stamp, stamp), follow_symlinks=False)
  os.utime(home, ns=(stamp, stamp))


def pin_times_outside_home(home: pathlib.Path, instant: datetime.datetime) -> None:
  """Pin fixture-created tree entries outside home, leaving harness-owned top-level entries alone."""
  from servers import HARNESS_ENTRIES

  tree_root = home.parent
  stamp = int(instant.timestamp()) * 1_000_000_000
  for entry in tree_root.iterdir():
    if entry.name in HARNESS_ENTRIES or entry == home:
      continue
    for dirpath, dirnames, filenames in os.walk(entry, topdown=False):
      current = pathlib.Path(dirpath)
      for name in [*filenames, *dirnames]:
        os.utime(current / name, ns=(stamp, stamp), follow_symlinks=False)
    os.utime(entry, ns=(stamp, stamp), follow_symlinks=False)


async def build(manifest_path: pathlib.Path, extras: tuple[str, ...]) -> None:
  from src.infra import models

  models.utc_now = CLOCK.now
  b = Builder()
  await build_atlas(b)
  await build_beacon(b)
  await build_cobalt(b)
  await build_delta(b)
  await build_ember(b)
  await build_fable(b)
  await build_garnet(b)
  if extras:
    b.manifest["extras"] = {}
    for name in extras:
      module = importlib.import_module(f"rust.parity.fixture_extras.{name}")
      values = await module.build(b)
      if not isinstance(values, dict):
        raise RuntimeError(f"fixture extra {name} must return a mapping")
      for key, value in values.items():
        if not isinstance(key, str) or not key:
          raise RuntimeError(f"fixture extra {name} returned a non-string key: {key!r}")
        _check_extra_value(name, key, value)
      b.manifest["extras"][name] = values
  check_event_coverage(b)
  await check_recovery_fixed_point(b)
  pin_times(b.cfg.charliebot_home, CLOCK.now())
  if extras:
    pin_times_outside_home(b.cfg.charliebot_home, CLOCK.now())
  manifest_path.write_text(json.dumps(b.manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def _check_extra_value(name: str, key: str, value: object) -> None:
  if isinstance(value, dict):
    for child_key, child in value.items():
      _check_extra_value(name, f"{key}.{child_key}", child)
  elif isinstance(value, list):
    for index, child in enumerate(value):
      _check_extra_value(name, f"{key}[{index}]", child)
  elif isinstance(value, str):
    path = pathlib.PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts:
      raise RuntimeError(f"fixture extra {name} returned non-tree-relative value {key}={value!r}")


def main() -> None:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument("--manifest", required=True, type=pathlib.Path)
  parser.add_argument("--extras", default="", help="comma-separated fixture-extra module names")
  args = parser.parse_args()
  extras = tuple(name for name in args.extras.split(",") if name)
  if len(extras) != len(set(extras)):
    parser.error("--extras must not repeat a module name")
  # Seed uuid4 before any server module draws one.
  uuid.uuid4 = _seeded_uuid4
  sys.path.insert(0, str(REPO_ROOT))
  from src.app import registrations

  registrations.register_all()
  asyncio.run(build(args.manifest, extras))


if __name__ == "__main__":
  main()

"""Exercise the Python manager consumer with scripted backend streams."""

from __future__ import annotations

import asyncio
import datetime
import json
import pathlib
import sys
import tempfile

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tests"))

import conftest
import test_master_cc_consumer as consumer
from src.features.slack import metadata as slack_metadata
from src.infra import models
from src.runtime.agent_process import base


def result_event() -> dict:
  return base.make_result_event()


CASES = [
    {
        "name": "assistant_text_then_result",
        "mode": "fresh",
        "session_kind": "web",
        "events": [base.make_text_event("answer"), result_event()],
        "exit_code": 0,
        "stderr": "",
    },
    {
        "name": "zero_usage_result",
        "mode": "fresh",
        "session_kind": "web",
        "events": [result_event()],
        "exit_code": 0,
        "stderr": "",
    },
    {
        "name": "thinking_text_salvage",
        "mode": "fresh",
        "session_kind": "web",
        "events": [{
            "type": "thinking",
            "content": "considered answer"
        }, result_event()],
        "exit_code": 0,
        "stderr": "",
    },
    {
        "name": "error_event_exit_one_without_result",
        "mode": "fresh",
        "session_kind": "web",
        "events": [base.make_error_event("synthetic backend error")],
        "exit_code": 1,
        "stderr": "",
    },
    {
        "name": "pending_tool_call_before_result",
        "mode": "fresh",
        "session_kind": "web",
        "events":
            [
                {
                    "type": "assistant",
                    "message":
                        {
                            "content":
                                [
                                    {
                                        "type": "tool_use",
                                        "id": "tool-synthetic",
                                        "name": "Read",
                                        "input": {
                                            "path": "/synthetic/file"
                                        }
                                    }
                                ]
                        },
                },
                result_event()
            ],
        "exit_code": 0,
        "stderr": "",
    },
    {
        "name": "nonzero_exit_only_stderr",
        "mode": "fresh",
        "session_kind": "web",
        "events": [],
        "exit_code": 3,
        "stderr": "synthetic stderr failure",
    },
    {
        "name": "manual_compaction_boundary",
        "mode": "fresh",
        "session_kind": "web",
        "events": [base.make_compact_boundary_event("manual", 120),
                   result_event()],
        "exit_code": 0,
        "stderr": "",
    },
    {
        "name": "resume_path",
        "mode": "resume",
        "session_kind": "web",
        "events": [base.make_text_event("resumed answer"), result_event()],
        "exit_code": 0,
        "stderr": "",
    },
    {
        "name": "resume_zero_output_guard",
        "mode": "resume",
        "session_kind": "web",
        "events": [result_event()],
        "exit_code": 0,
        "stderr": "",
    },
    {
        "name": "refused_turn",
        "mode": "refused",
        "session_kind": "web",
        "events": [],
        "exit_code": 0,
        "stderr": "",
    },
    {
        "name": "unknown_backend_pin_refusal",
        "mode": "unknown_pin",
        "session_kind": "web",
        "events": [],
        "exit_code": 0,
        "stderr": "",
    },
    {
        "name": "thread_session_unread",
        "mode": "fresh",
        "session_kind": "thread",
        "events": [base.make_text_event("thread answer"), result_event()],
        "exit_code": 0,
        "stderr": "",
    },
]


def session_meta(session_id: str, session_kind: str) -> models.SessionMetadata:
  meta = consumer._make_meta(session_id)
  if session_kind == "thread":
    meta.slack_origin = slack_metadata.SlackOrigin(
        team_id="synthetic-team", channel_id="synthetic-channel", thread_ts="1700000000.000100")
  return meta


async def run_case(case: dict, index: int) -> dict:
  session_id = f"golden-{index}"
  with tempfile.TemporaryDirectory(prefix="charliebot-python-turn-") as temporary:
    tmp_path = pathlib.Path(temporary)
    monkeypatch = pytest.MonkeyPatch()
    try:
      callbacks = None
      if case["mode"] == "fresh":
        callbacks = await consumer._run_stream_consumer(
            tmp_path,
            monkeypatch,
            session_id,
            case["events"],
            exit_code=case["exit_code"],
            stderr_text=case["stderr"],
            meta=session_meta(session_id, case["session_kind"]))
      elif case["mode"] in ("refused", "unknown_pin"):
        cfg = conftest.build_master_cc_cfg(tmp_path)
        manager = conftest.build_session_blocks(cfg)
        conftest.bind_session_blocks(monkeypatch, manager)
        callbacks = conftest.mock_session_callbacks()
        backend = "deleted-id" if case["mode"] == "unknown_pin" else ""
        meta = models.SessionMetadata(profile="manager", id=session_id, name="refused", backend=backend)
        async with conftest.fresh_master_state(session_id):
          await conftest.run_task_manager_message(cfg, meta, "hi", callbacks)
          await conftest.drain_session_consumer(session_id, timeout=5)
      elif case["mode"] == "resume":
        cfg = conftest.build_master_cc_cfg(tmp_path)
        manager = conftest.build_session_blocks(cfg)
        conftest.bind_session_blocks(monkeypatch, manager)
        conftest.patch_resume_seams(monkeypatch)
        callbacks = conftest.mock_session_callbacks()
        raw_path = tmp_path / "resume" / "agent.raw.ndjson"
        raw_path.parent.mkdir(parents=True)
        raw_path.write_text("".join(json.dumps(event) + "\n" for event in case["events"]), encoding="utf-8")
        record = models.MasterRunRecord(
            pid=None,
            pid_start=None,
            started_at=datetime.datetime.now(datetime.UTC),
            raw_log=str(raw_path),
        )
        await conftest.run_resume_round(
            cfg,
            session_meta(session_id, case["session_kind"]),
            record,
            callbacks,
            is_alive=lambda: False,
        )
      else:
        raise AssertionError(f"unknown mode: {case['mode']}")

      expected = dict(case)
      expected["session_id"] = session_id
      expected["persisted"] = [call.args[1] for call in callbacks.persist_and_broadcast.await_args_list]
      expected["mark_unread"] = [call.args[0] for call in callbacks.mark_unread.await_args_list]
      return expected
    finally:
      monkeypatch.undo()


async def main() -> None:
  results = [await run_case(case, index) for index, case in enumerate(CASES, start=1)]
  print(json.dumps(results, ensure_ascii=False, separators=(",", ":")))


if __name__ == "__main__":
  asyncio.run(main())

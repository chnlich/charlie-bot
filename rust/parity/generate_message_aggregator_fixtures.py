"""Generate Rust aggregator fixtures from the Python baseline and parity home."""

from __future__ import annotations

import json
import pathlib
import sys
import tempfile

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
PARITY_DIR = pathlib.Path(__file__).resolve().parent
OUTPUT = REPO_ROOT / "rust/crates/charliebot-store/tests/fixtures/message_aggregator.json"
SYNTHETIC_HOME = "/home/<user>/parity-home"

SYNTHETIC_SEQUENCES = {
    "assistant_tool_thinking_and_commit":
        {
            "event_index_offset":
                17,
            "events":
                [
                    {
                        "type": "assistant",
                        "id": "synthetic-assistant-1",
                        "timestamp": "2026-01-01T00:00:00Z",
                        "message":
                            {
                                "model":
                                    "claude-sonnet-5",
                                "content":
                                    [
                                        {
                                            "type": "thinking",
                                            "thinking": "Plan the file read.",
                                            "signature": "discard-me"
                                        },
                                        {
                                            "type": "text",
                                            "text": "Reading "
                                        },
                                        {
                                            "type": "tool_use",
                                            "id": "tool-1",
                                            "name": "Read",
                                            "input": {
                                                "file_path": "/work/notes.md"
                                            }
                                        },
                                    ],
                            },
                    },
                    {
                        "type": "user",
                        "id": "synthetic-tool-result-1",
                        "message": {
                            "content": [{
                                "type": "tool_result",
                                "content": "first\nsecond",
                                "is_error": False
                            }]
                        },
                    },
                    {
                        "type": "assistant",
                        "id": "synthetic-assistant-2",
                        "timestamp": "2026-01-01T00:00:02Z",
                        "message": {
                            "content": [{
                                "type": "text",
                                "text": "the notes."
                            }]
                        },
                    },
                    {
                        "type": "master_done",
                        "id": "synthetic-done",
                        "thinking_seconds": 4,
                        "timestamp": "2026-01-01T00:00:03Z"
                    },
                    {
                        "type": "user",
                        "id": "synthetic-user-2",
                        "content": "Next question",
                        "input_mode": "voice"
                    },
                ],
        },
    "bounded_preview_and_empty_draft":
        {
            "event_index_offset":
                0,
            "events":
                [
                    {
                        "type": "assistant",
                        "id": "synthetic-preview",
                        "message":
                            {
                                "content":
                                    [
                                        {
                                            "type": "tool_use",
                                            "id": "tool-preview",
                                            "name": "Bash",
                                            "input": {
                                                "command": "c" * 600,
                                                "description": "d" * 120
                                            },
                                        },
                                        {
                                            "type": "text",
                                            "text": "draft"
                                        },
                                    ]
                            },
                    },
                    {
                        "type": "assistant_error",
                        "content": "stop preview",
                        "timestamp": "2026-01-01T00:01:00Z"
                    },
                ],
        },
    "tool_buffer_flush_and_plan_commit":
        {
            "event_index_offset":
                3,
            "events":
                [
                    {
                        "type": "assistant",
                        "id": "syn-tool-1",
                        "timestamp": "2026-01-02T00:00:00Z",
                        "message":
                            {
                                "content":
                                    [
                                        {
                                            "type": "text",
                                            "text": "Checking "
                                        },
                                        {
                                            "type": "tool_use",
                                            "name": "Bash",
                                            "input": {
                                                "command": "c" * 600,
                                                "description": "d" * 100
                                            }
                                        },
                                    ]
                            },
                    },
                    {
                        "type": "tool_use",
                        "id": "syn-tool-2",
                        "name": "Read",
                        "input": {
                            "file_path": "/work/" + "p" * 520
                        },
                    },
                    {
                        "type": "tool_result",
                        "id": "syn-result-2",
                        "content": "é" * 600,
                        "is_error": True
                    },
                    {
                        "type": "tool_use",
                        "id": "syn-tool-3",
                        "name": "CustomTool",
                        "input": {
                            "alpha": "a" * 70,
                            "beta": "b" * 70
                        },
                    },
                    {
                        "type": "tool_result",
                        "id": "syn-result-3",
                        "content":
                            [
                                {
                                    "type": "text",
                                    "text": "first"
                                },
                                {
                                    "type": "image",
                                    "source": "x"
                                },
                                {
                                    "type": "text",
                                    "text": "second"
                                },
                            ],
                        "is_error": False
                    },
                    {
                        "type": "user",
                        "id": "syn-internal-1",
                        "message":
                            {
                                "content":
                                    [
                                        {
                                            "type": "tool_result",
                                            "content":
                                                [
                                                    {
                                                        "type": "text",
                                                        "text": "line one"
                                                    },
                                                    {
                                                        "type": "text",
                                                        "text": "line two"
                                                    },
                                                    {
                                                        "type": "tool_reference"
                                                    },
                                                ],
                                            "is_error": True
                                        }
                                    ]
                            },
                    },
                    {
                        "type": "assistant",
                        "id": "syn-plan-1",
                        "timestamp": "2026-01-02T00:00:05Z",
                        "message":
                            {
                                "content":
                                    [
                                        {
                                            "type": "tool_use",
                                            "name": "ExitPlanMode",
                                            "input": {
                                                "plan": "Step 1: read. Step 2: write."
                                            },
                                        }
                                    ]
                            },
                    },
                    {
                        "type": "assistant",
                        "id": "syn-text-2",
                        "message": {
                            "content": [{
                                "type": "text",
                                "text": "Buffered answer"
                            }]
                        },
                    },
                    {
                        "type": "assistant",
                        "id": "syn-plan-2",
                        "message": {
                            "content": [{
                                "type": "tool_use",
                                "name": "ExitPlanMode",
                                "input": {
                                    "plan": ""
                                },
                            }]
                        },
                    },
                    {
                        "type": "assistant",
                        "id": "syn-plan-3",
                        "message": {
                            "content": [{
                                "type": "tool_use",
                                "name": "ExitPlanMode",
                                "input": {},
                            }]
                        },
                    },
                    {
                        "type": "tool_use",
                        "id": "syn-tool-4",
                        "name": "Grep",
                        "input": {
                            "pattern": "p" * 80,
                            "path": "/src/" + "d" * 120,
                            "glob": "*.rs" * 30
                        },
                    },
                    {
                        "type": "tool_result",
                        "id": "syn-result-4",
                        "content": "matched",
                        "is_error": False
                    },
                    {
                        "type": "user",
                        "id": "syn-user-3",
                        "content": "follow up",
                        "timestamp": "2026-01-02T00:01:00Z"
                    },
                    {
                        "type": "assistant",
                        "id": "syn-text-3",
                        "timestamp": "2026-01-02T00:01:02Z",
                        "message": {
                            "content": [{
                                "type": "text",
                                "text": "Done "
                            }]
                        },
                    },
                    {
                        "type": "assistant",
                        "id": "syn-text-4",
                        "timestamp": "2026-01-02T00:01:03Z",
                        "message": {
                            "content": [{
                                "type": "text",
                                "text": "reading."
                            }]
                        },
                    },
                    {
                        "type": "master_done",
                        "id": "syn-done-3",
                        "timestamp": "2026-01-02T00:01:04Z"
                    },
                    {
                        "type": "assistant",
                        "id": "syn-open",
                        "message":
                            {
                                "content":
                                    [
                                        {
                                            "type": "text",
                                            "text": "still "
                                        },
                                        {
                                            "type": "tool_use",
                                            "name": "Glob",
                                            "input": {
                                                "pattern": "**/*.py"
                                            }
                                        },
                                    ]
                            },
                    },
                ],
        },
    "thinking_deltas_and_draft_flush":
        {
            "event_index_offset":
                40,
            "events":
                [
                    {
                        "type": "thinking",
                        "id": "syn-think-1",
                        "content": "Plan "
                    },
                    {
                        "type": "thinking",
                        "id": "syn-think-2",
                        "content": 7,
                        "timestamp": "2026-01-03T00:00:00Z"
                    },
                    {
                        "type": "assistant",
                        "id": "syn-asst-1",
                        "timestamp": "2026-01-03T00:00:01Z",
                        "message":
                            {
                                "content":
                                    [
                                        {
                                            "type": "thinking",
                                            "thinking": "Snapshot wins",
                                            "signature": "discard"
                                        },
                                        {
                                            "type": "text",
                                            "text": "Answer "
                                        },
                                    ]
                            },
                    },
                    {
                        "type": "assistant",
                        "id": "syn-asst-2",
                        "timestamp": "2026-01-03T00:00:02Z",
                        "message": {
                            "content": [{
                                "type": "text",
                                "text": "second part"
                            }]
                        },
                    },
                    {
                        "type": "assistant",
                        "id": "syn-asst-3",
                        "message": {
                            "content": []
                        },
                    },
                    {
                        "type": "assistant",
                        "id": "syn-asst-4"
                    },
                    {
                        "type": "assistant",
                        "id": "syn-asst-5",
                        "message": {
                            "content": None
                        },
                    },
                    {
                        "type": "thinking",
                        "id": "syn-think-3"
                    },
                    {
                        "type": "user",
                        "id": "syn-user-1",
                        "content": "interrupt",
                        "input_mode": "text",
                        "timestamp": "2026-01-03T00:00:03Z"
                    },
                    {
                        "type": "thinking",
                        "id": "syn-think-4",
                        "content": "Fresh "
                    },
                    {
                        "type": "master_done",
                        "id": "syn-done-4",
                        "thinking_seconds": 0,
                        "timestamp": "2026-01-03T00:00:04Z"
                    },
                    {
                        "type": "assistant",
                        "id": "syn-asst-6",
                        "message": {
                            "content": [{
                                "type": "text",
                                "text": "tail"
                            }]
                        },
                    },
                    {
                        "type": "user",
                        "id": "syn-internal-2",
                        "message": {
                            "content": [{
                                "type": "tool_result",
                                "content": "no tools",
                                "is_error": True
                            }]
                        },
                    },
                ],
        },
    "compaction_renderings":
        {
            "event_index_offset":
                0,
            "events":
                [
                    {
                        "type": "context_compacted",
                        "id": "syn-c1",
                        "timestamp": "2026-01-04T00:00:00Z",
                        "trigger": "manual",
                        "model": "claude-opus-5",
                        "compact_metadata": {
                            "trigger": "manual",
                            "pre_tokens": 239708,
                            "post_tokens": 12000
                        },
                    },
                    {
                        "type": "context_compacted",
                        "id": "syn-c2",
                        "compact_metadata": {
                            "trigger": "auto",
                            "pre_tokens": 1014500,
                            "post_tokens": 0
                        },
                    },
                    {
                        "type": "context_compacted",
                        "id": "syn-c3",
                        "trigger": "",
                        "model": "claude-sonnet-5",
                        "pre_tokens": 99999
                    },
                    {
                        "type": "context_compacted",
                        "id": "syn-c4",
                        "trigger": "manual",
                        "model": "claude",
                        "compact_metadata": {}
                    },
                    {
                        "type": "context_compacted",
                        "id": "syn-c5",
                        "trigger": "manual",
                        "model": "",
                        "compact_metadata": {
                            "trigger": "manual",
                            "pre_tokens": 0,
                            "post_tokens": 5
                        },
                    },
                    {
                        "type": "context_compacted",
                        "id": "syn-c6",
                        "model": 42
                    },
                    {
                        "type": "context_compacted",
                        "id": "syn-c7",
                        "trigger": "manual",
                        "model": "sonnet-x",
                        "compact_metadata": {
                            "trigger": "manual",
                            "pre_tokens": 15500,
                            "post_tokens": 9000
                        },
                    },
                    {
                        "type": "context_compacted",
                        "id": "syn-c8",
                        "trigger": "auto",
                        "compact_metadata": {
                            "trigger": "auto",
                            "pre_tokens": 4000,
                            "post_tokens": 2000
                        },
                    },
                    {
                        "type": "context_compact_failed",
                        "id": "syn-f1",
                        "model": "claude-haiku-5",
                        "error": "context too large"
                    },
                    {
                        "type": "context_compact_failed",
                        "id": "syn-f2"
                    },
                    {
                        "type": "context_compact_failed",
                        "id": "syn-f3",
                        "model": "",
                        "error": ""
                    },
                    {
                        "type": "context_compact_failed",
                        "id": "syn-f4",
                        "error": {
                            "code": 7
                        }
                    },
                ],
        },
    "errors_notices_and_unknown_events":
        {
            "event_index_offset":
                5,
            "events":
                [
                    {
                        "type": "assistant_error",
                        "id": "syn-e1",
                        "content": "rate limited",
                        "timestamp": "2026-01-05T00:00:00Z"
                    },
                    {
                        "type": "assistant_error",
                        "id": "syn-e2"
                    },
                    {
                        "type": "error",
                        "id": "syn-e3",
                        "content": "",
                        "message": "fallback text"
                    },
                    {
                        "type": "error",
                        "id": "syn-e4",
                        "content": None,
                        "message": None
                    },
                    {
                        "type": "error",
                        "id": "syn-e5",
                        "content": {
                            "detail": ["x"]
                        }
                    },
                    {
                        "type": "message",
                        "id": "syn-e6",
                        "content": "not rendered"
                    },
                    {
                        "type": "stream",
                        "id": "syn-e7",
                        "message": {
                            "content": "not rendered"
                        }
                    },
                    {
                        "type": "system",
                        "id": "syn-s1",
                        "subtype": "command_progress",
                        "content": "running tests"
                    },
                    {
                        "type": "system",
                        "id": "syn-s2",
                        "subtype": "status",
                        "compact_result": "ok"
                    },
                    {
                        "type": "system",
                        "id": "syn-s3",
                        "subtype": "command_progress"
                    },
                    {
                        "type": "mystery_event",
                        "id": "syn-m1",
                        "content": "unknown"
                    },
                    {
                        "type": "tool_result",
                        "id": "syn-r1",
                        "content": "orphan"
                    },
                    {
                        "type": "user",
                        "id": "syn-u-cc",
                        "message": {
                            "content": [{
                                "type": "text",
                                "text": "not a result"
                            }]
                        }
                    },
                    {
                        "type": "user",
                        "id": "syn-u-msg",
                        "message": {
                            "role": "user"
                        }
                    },
                    {
                        "type": "user",
                        "id": "syn-u-legacy",
                        "content": "legacy footer\n\n[Attached files]\n- /work/a.png\n- /work/b.txt",
                        "timestamp": "2026-01-05T00:00:01Z"
                    },
                    {
                        "type": "user",
                        "id": "syn-u-malformed",
                        "content": "bad\n\n[Attached files]\nnot a list"
                    },
                    {
                        "type": "user",
                        "id": "syn-u-structured",
                        "content": "with files\n\n[Attached files]\n- /work/x.png",
                        "uploaded_files": [{
                            "path": "/work/c.png",
                            "filename": "c.png"
                        }],
                    },
                    {
                        "type": "user",
                        "id": "syn-u-string-upload",
                        "content": "string upload",
                        "uploaded_files": ["/work/s.md"]
                    },
                    {
                        "type": "user",
                        "id": "syn-u-empty-upload",
                        "content": "empty uploads",
                        "uploaded_files": []
                    },
                    {
                        "type": "master_done",
                        "id": "syn-d1",
                        "still_thinking": True,
                        "timestamp": "2026-01-05T00:00:02Z"
                    },
                ],
        },
    "task_run_and_control_events":
        {
            "event_index_offset":
                100,
            "events":
                [
                    {
                        "type": "task_delegated",
                        "id": "syn-t1",
                        "thread_id": "th-1",
                        "child_session_id": "child-1",
                        "delegate_invocation": {
                            "task_type": "review"
                        },
                        "backend": "",
                        "resolved_backend": "codex-o3",
                        "model": "",
                        "resolved_model": "o3",
                        "timestamp": "2026-02-01T00:00:00Z"
                    },
                    {
                        "type": "task_delegated",
                        "id": "syn-t2",
                        "thread_id": "th-2",
                        "created_at": "2026-02-01T00:00:01Z"
                    },
                    {
                        "type": "task_delegated",
                        "id": "syn-t3",
                        "thread_id": "th-3"
                    },
                    {
                        "type": "worker_summary",
                        "id": "syn-w1",
                        "content": "done",
                        "thread_id": "th-1",
                        "origin_session_id": "origin-1"
                    },
                    {
                        "type": "worker_summary",
                        "id": "syn-w2",
                        "content": "no origin"
                    },
                    {
                        "type": "worker_summary",
                        "id": "syn-w3",
                        "origin_session_id": None,
                        "thread_id": "th-2"
                    },
                    {
                        "type": "handler_result",
                        "id": "syn-h1",
                        "task": "lint",
                        "status": "ok",
                        "message": "clean"
                    },
                    {
                        "type": "handler_result",
                        "id": "syn-h2",
                        "task": "test",
                        "status": "failed",
                        "message": "2 failed"
                    },
                    {
                        "type": "handler_result",
                        "id": "syn-h3"
                    },
                    {
                        "type": "clone_start",
                        "id": "syn-cl1",
                        "parent_session_name": "Parent",
                        "parent_session_id": "p-1"
                    },
                    {
                        "type": "clone_start",
                        "id": "syn-cl2"
                    },
                    {
                        "type": "scheduled_trigger",
                        "id": "syn-st1",
                        "content": "nightly wake"
                    },
                    {
                        "type": "agent_message",
                        "id": "syn-am1",
                        "content": "hello",
                        "from_session": "s-9",
                        "from_session_name": "Survey"
                    },
                    {
                        "type": "backend_switched",
                        "id": "syn-bs1",
                        "from": "claude",
                        "to": "codex"
                    },
                    {
                        "type": "backend_overlay_inactive",
                        "id": "syn-bo1",
                        "backend": "codex",
                        "reason": "undeclared"
                    },
                    {
                        "type": "backend_overlay_inactive",
                        "id": "syn-bo2",
                        "backend": "claude",
                        "reason": "unreadable",
                        "overlay": "fence.md",
                        "error": "PermissionError"
                    },
                    {
                        "type": "backend_overlay_undeclared",
                        "id": "syn-bo3",
                        "backend": "legacy"
                    },
                    {
                        "type": "model_fallback_notice",
                        "id": "syn-mf1",
                        "configured_model": "claude-opus-5",
                        "served_models": ["claude-sonnet-5", "claude-haiku-5"]
                    },
                    {
                        "type": "model_fallback_notice",
                        "id": "syn-mf2",
                        "configured_model": None,
                        "served_models": []
                    },
                    {
                        "type": "child_report",
                        "id": "syn-cr1",
                        "summary": "built",
                        "child_session_id": "child-1",
                        "outcome": "completed",
                        "result_refs": ["/r/1"]
                    },
                    {
                        "type": "child_report",
                        "id": "syn-cr2"
                    },
                    {
                        "type": "run_header",
                        "id": "syn-rh1",
                        "kind": "implement",
                        "backend_label": "Codex",
                        "backend": "codex-o3",
                        "state": "running",
                        "launched": True,
                        "launch_failed": False,
                        "task_spec_ref": "spec.md",
                        "launch_prompt_ref": "prompt.md",
                        "run_id": "run-1",
                        "error": None
                    },
                    {
                        "type": "run_header",
                        "id": "syn-rh2",
                        "kind": "",
                        "backend": "fake",
                        "state": "failed",
                        "launched": False,
                        "launch_failed": True,
                        "task_spec_ref": None,
                        "launch_prompt_ref": "p2",
                        "error": "spawn failed"
                    },
                    {
                        "type": "run_header",
                        "id": "syn-rh3",
                        "state": "withheld",
                        "withheld": "disk low",
                        "launched": False,
                        "launch_failed": False,
                        "task_spec_ref": "s",
                        "launch_prompt_ref": "p"
                    },
                    {
                        "type": "run_delivery",
                        "id": "syn-rd1",
                        "summary": "shipped",
                        "task_state": "completed",
                        "result_refs": ["/r/2"],
                        "raw_log_ref": "raw.log",
                        "events_ref": "ev.json",
                        "result_ref": "res.json",
                        "repo_path": "/repo",
                        "base_branch": "main",
                        "branch_name": "feat/x",
                        "run_id": "run-2"
                    },
                    {
                        "type": "run_delivery",
                        "id": "syn-rd2",
                        "task_state": "failed"
                    },
                    {
                        "type": "task_closed",
                        "id": "syn-tc1",
                        "outcome": "completed",
                        "summary": "all good"
                    },
                    {
                        "type": "task_closed",
                        "id": "syn-tc2"
                    },
                    {
                        "type": "task_closed",
                        "id": "syn-tc3",
                        "outcome": "abandoned",
                        "summary": None
                    },
                    {
                        "type": "task_reopened",
                        "id": "syn-tr1",
                        "reason": "new input"
                    },
                    {
                        "type": "task_reopened",
                        "id": "syn-tr2"
                    },
                    {
                        "type": "task_input_acknowledged",
                        "id": "syn-ia1",
                        "input_ids": ["syn-u-1", "syn-u-2"],
                        "note": "covered by plan"
                    },
                    {
                        "type": "task_input_acknowledged",
                        "id": "syn-ia2",
                        "input_ids": []
                    },
                    {
                        "type": "resume_context_dropped",
                        "id": "syn-rc1",
                        "reason": "anchor_missing"
                    },
                    {
                        "type": "resume_context_dropped",
                        "id": "syn-rc2",
                        "reason": "transcript_missing"
                    },
                    {
                        "type": "resume_context_dropped",
                        "id": "syn-rc3",
                        "reason": "other"
                    },
                    {
                        "type": "master_done",
                        "id": "syn-d2",
                        "thinking_seconds": 12,
                        "timestamp": "2026-02-01T00:00:10Z"
                    },
                ],
        },
}


def _normalize(value: object, root: pathlib.Path) -> object:
  if isinstance(value, str):
    return value.replace(str(root), SYNTHETIC_HOME)
  if isinstance(value, list):
    return [_normalize(item, root) for item in value]
  if isinstance(value, dict):
    return {key: _normalize(item, root) for key, item in value.items()}
  return value


def _read_events(path: pathlib.Path) -> list[dict]:
  return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _python_frames(events: list[dict], event_index_offset: int) -> list[dict]:
  from src.runtime import message_aggregator

  aggregator = message_aggregator.MessageAggregator(event_index_offset=event_index_offset)
  frames = []
  for event in events:
    frames.extend(aggregator.feed(event))
  return frames


def _session_cases(home: pathlib.Path) -> list[dict]:
  cases = []
  for session_dir in sorted((home / "sessions").iterdir()):
    if not session_dir.is_dir():
      continue
    data_dir = session_dir / "data"
    archives_dir = data_dir / "archives"
    paths = sorted(archives_dir.glob("chat_events.*.jsonl")) if archives_dir.exists() else []
    live_path = data_dir / "chat_events.jsonl"
    if live_path.exists():
      paths.append(live_path)
    events = [event for path in paths for event in _read_events(path)]
    if events:
      cases.append({"session": session_dir.name, "events": events, "frames": _python_frames(events, 0)})
  return cases


def main() -> None:
  sys.path.insert(0, str(REPO_ROOT))
  sys.path.insert(0, str(PARITY_DIR))
  from servers import STRACE_BIN, make_tree, pick_ports, run_traced, write_home_config

  from src.app import registrations

  registrations.register_all()
  parity_out = PARITY_DIR / "out"
  parity_out.mkdir(exist_ok=True)
  with tempfile.TemporaryDirectory(prefix="aggregator-fixture-") as temp_dir:
    temp_root = pathlib.Path(temp_dir)
    tree = make_tree("aggregator", temp_root / "tree")
    with pick_ports(1) as ports:
      write_home_config(tree, ports[0])
    run_traced(
        [sys.executable,
         str(PARITY_DIR / "fixture.py"), "--manifest",
         str(tree.root / "fixture-manifest.json")],
        tree=tree,
        repo_root=REPO_ROOT,
        trace_log=tree.root / "logs/aggregator.fixture.strace",
        log_path=tree.root / "logs/aggregator.fixture.log",
        strace=STRACE_BIN)
    logs = _session_cases(tree.home)
    normalized_logs = _normalize(_normalize(logs, tree.home), tree.root)
    synthetic = []
    for name, sequence in SYNTHETIC_SEQUENCES.items():
      events = sequence["events"]
      offset = sequence["event_index_offset"]
      synthetic.append(
          {
              "name": name,
              "event_index_offset": offset,
              "events": events,
              "frames": _python_frames(events, offset),
          })

  fixture = {"format": 1, "parity_chat_logs": normalized_logs, "synthetic_sequences": synthetic}
  OUTPUT.write_text(json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
  print(f"wrote {OUTPUT} ({len(normalized_logs)} parity chat logs, {len(synthetic)} synthetic sequences)")


if __name__ == "__main__":
  main()

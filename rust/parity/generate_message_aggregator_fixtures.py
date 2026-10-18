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
  from servers import make_tree, pick_ports, run_traced, write_home_config

  from src.app import registrations

  registrations.register_all()
  parity_out = PARITY_DIR / "out"
  parity_out.mkdir(exist_ok=True)
  with tempfile.TemporaryDirectory(prefix="aggregator-fixture-") as temp_dir:
    temp_root = pathlib.Path(temp_dir)
    tree = make_tree("aggregator", temp_root / "tree")
    write_home_config(tree, pick_ports(1)[0])
    run_traced(
        [sys.executable,
         str(PARITY_DIR / "fixture.py"), "--manifest",
         str(tree.root / "fixture-manifest.json")],
        tree=tree,
        repo_root=REPO_ROOT,
        trace_log=tree.root / "logs/aggregator.fixture.strace",
        log_path=tree.root / "logs/aggregator.fixture.log")
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

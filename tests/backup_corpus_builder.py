"""Build the M112 backup collector's scratch home: a synthetic CHARLIEBOT_HOME
whose included corpus (sessions ``data/``, cache, memory, config.d) shapes the
backup archive's real input — gigabyte-scale chat-event and raw-log JSON — with
fresh random session ids and no ``cc_session_id``, so nothing here is live
state. The backup excludes ``sessions/*/threads``, so the corpus carries one
token threads subtree priced out of every build.

Run once per host before the M112 collector (the corpus persists under
/tmp/opencode/m112/home; the builder rebuilds it from scratch each run):

    python tests/backup_corpus_builder.py
"""

import json
import shutil
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

HOME = Path("/tmp/opencode/m112/home")

# Per-session target bytes, chosen to land the whole corpus in the ~5 GB range
# the healthy range's bytes line prices; six sessions keep the file count in the
# walk's realistic shape without stretching the build past a minute.
_CHAT_TARGET = 550 * 1024 * 1024
_ARCHIVE_TARGET = 130 * 1024 * 1024
_RAW_TARGET = 60 * 1024 * 1024

_EVENT_KINDS = (
    lambda i: {
        "type": "assistant",
        "message":
            {
                "content":
                    [
                        {
                            "type": "text",
                            "text":
                                f"The pipeline stage {i % 97} processes the batch and reports the latency numbers "
                                "back to the coordinator for aggregation. " * 6,
                        }
                    ],
                "usage": {
                    "input": 1200 + i % 50,
                    "output": 800 + i % 30,
                    "cache_read": 40000,
                    "cache_write": 900
                },
            },
    },
    lambda i: {
        "type": "user",
        "message":
            {
                "content":
                    [
                        {
                            "type": "text",
                            "text":
                                "Continue the sweep and summarize the deltas against the "
                                f"baseline once the workers drain. {i}"
                        }
                    ]
            },
    },
    lambda i: {
        "type": "stream",
        "message":
            {
                "content":
                    "delta chunk %d carrying the next slice of the streamed draft so "
                    "the re-render memo sees realistic sizes"
            },
    },
)


def _event_line(i: int, ts: str, session_id: str) -> str:
  event = _EVENT_KINDS[i % 3](i)
  event["id"] = f"ev-{i:09d}"
  event["session_id"] = session_id
  event["timestamp"] = ts
  return json.dumps(event, separators=(",", ":"))


def _write_lines(path: Path, target_bytes: int, session_id: str, start: int = 0) -> int:
  base = datetime(2026, 9, 1, 8, 0, 0, tzinfo=timezone.utc)
  size = 0
  i = start
  with open(path, "w", encoding="utf-8") as stream:
    while size < target_bytes:
      lines = []
      for _ in range(500):
        lines.append(_event_line(i, (base + timedelta(seconds=i)).isoformat(), session_id))
        i += 1
      chunk = "\n".join(lines) + "\n"
      stream.write(chunk)
      size += len(chunk.encode())
  return i


def main() -> None:
  if HOME.exists():
    shutil.rmtree(HOME)
  first_sid = None
  for s in range(6):
    sid = str(uuid.uuid4())
    first_sid = first_sid or sid
    session = HOME / "sessions" / sid
    (session / "data" / "archives").mkdir(parents=True)
    (session / "data" / "master_runs" / "2026-09-18T04:30:11.2+00:00").mkdir(parents=True)
    meta = {
        "id": sid,
        "name": f"synthetic-{s}",
        "status": "archived",
        "created_at": "2026-09-01T08:00:00+00:00",
        "updated_at": "2026-09-20T08:00:00+00:00",
        "backend": "cc-claude",
        "archive_offset": 12000
    }
    (session / "metadata.json").write_text(json.dumps(meta), encoding="utf-8")
    _write_lines(session / "data" / "chat_events.jsonl", _CHAT_TARGET, sid)
    _write_lines(session / "data" / "archives" / "chat_events.2026-W38.jsonl", _ARCHIVE_TARGET, sid)
    _write_lines(
        session / "data" / "master_runs" / "2026-09-18T04:30:11.2+00:00" / "agent.raw.ndjson", _RAW_TARGET, sid)

  # The excluded shape: a threads subtree whose bytes the backup never reads.
  threads = HOME / "sessions" / first_sid / "threads" / str(uuid.uuid4()) / "data"
  threads.mkdir(parents=True)
  (threads / "events.jsonl").write_text("{}\n", encoding="utf-8")

  cache = HOME / "cache"
  cache.mkdir()
  rows = [
      {
          "source": "claude",
          "account": "main",
          "model": "claude-opus-4-6",
          "calls": 900 + i % 40,
          "in_fresh": 1000 + i,
          "cache_write": 40000 + i,
          "cache_read": 900000 + i,
          "output": 500 + i % 90
      } for i in range(300000)
  ]
  (cache / "token_tally.json").write_text(json.dumps({"rows": rows}), encoding="utf-8")

  mem = HOME / "memory" / "entries" / "synthetic"
  mem.mkdir(parents=True)
  for i in range(40):
    (mem / f"note-{i}.md").write_text(f"# note {i}\n" + "Synthetic memory body line. " * 40, encoding="utf-8")

  cron = HOME / "config.d" / "cron.d"
  cron.mkdir(parents=True)
  (cron / "hourly.yaml").write_text(
      "type: normal\ncron: '0 * * * *'\n"
      "timezone: America/Los_Angeles\nprompt: hourly probe\n", encoding="utf-8")

  files = [p for p in HOME.rglob("*") if p.is_file()]
  total = sum(p.stat().st_size for p in files)
  print(f"corpus built: {total / 1e9:.2f} GB across {len(files)} files at {HOME}")


if __name__ == "__main__":
  main()

"""Build the M112 backup collector's scratch home: a synthetic CHARLIEBOT_HOME
whose included corpus (sessions ``data/``, cache, memory, config.d) shapes the
backup archive's real input — gigabyte-scale chat-event and raw-log JSON — with
fresh random session ids and no ``cc_session_id``, so nothing here is live
state. The backup excludes ``sessions/*/threads``, so the corpus carries one
token threads subtree priced out of every build.

Run before the M112 collector (the corpus persists under /tmp/opencode/m112/home;
the first run builds it and writes a shape manifest, every later run verifies the
persisted shape against that manifest and rebuilds only on a mismatch):

    python tests/backup_corpus_builder.py
"""

import json
import shutil
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

HOME = Path("/tmp/opencode/m112/home")
# The build's receipt lives outside HOME so the measured corpus stays exactly the
# builder's output: the M112 collector prices the corpus by a plain file walk, and
# a manifest inside HOME would join it and move every reading.
MANIFEST = HOME.with_name("corpus_manifest.json")

# Per-session target bytes, chosen to land the whole corpus in the ~5 GB range
# the healthy range's bytes line prices; six sessions keep the file count in the
# walk's realistic shape without stretching the build past a minute.
_CHAT_TARGET = 550 * 1024 * 1024
_ARCHIVE_TARGET = 130 * 1024 * 1024
_RAW_TARGET = 60 * 1024 * 1024
_TALLY_ROWS = 300_000

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


def _write_lines(path: Path, target_bytes: int, session_id: str) -> int:
  base = datetime(2026, 9, 1, 8, 0, 0, tzinfo=timezone.utc)
  size = 0
  i = 0
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


def _corpus_shape(home: Path) -> tuple[int, int]:
  files = [p for p in home.rglob("*") if p.is_file()]
  return len(files), sum(p.stat().st_size for p in files)


def _read_manifest() -> dict | None:
  try:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
  except (OSError, ValueError):
    return None
  # Only the builder writes the file, but a hand-edited or truncated-then-valid
  # parse must ride the same rebuild path as an unreadable one, so main()'s
  # subscripts never see a manifest without both integer fields.
  if (isinstance(manifest, dict) and isinstance(manifest.get("files"), int) and isinstance(manifest.get("bytes"), int)):
    return manifest
  return None


def main() -> None:
  """Build the corpus once per host; verify the persisted shape on every later run."""
  manifest = _read_manifest()
  if manifest is not None:
    try:
      files, total = _corpus_shape(HOME)
    except OSError as e:
      print(f"corpus shape unreadable ({e}); rebuilding")
    else:
      if (files, total) == (manifest["files"], manifest["bytes"]):
        print(f"corpus verified: {total / 1e9:.2f} GB across {files} files at {HOME}")
        return
      print(
          f"corpus shape drifted from the manifest "
          f"({files} files / {total} bytes vs {manifest['files']} / {manifest['bytes']}); rebuilding")
  else:
    print("no readable corpus manifest; rebuilding")
  _build()


def _build() -> None:
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
      } for i in range(_TALLY_ROWS)
  ]
  (cache / "token_tally.json").write_text(json.dumps({"rows": rows}), encoding="utf-8")

  mem = HOME / "memory" / "entries" / "synthetic"
  mem.mkdir(parents=True)
  for i in range(40):
    (mem / f"note-{i}.md").write_text(f"# note {i}\n" + "Synthetic memory body line. " * 40, encoding="utf-8")

  cron = HOME / "config.d" / "cron.d"
  cron.mkdir(parents=True)
  (cron / "hourly.yaml").write_text(
      "cron: '0 * * * *'\n"
      "timezone: America/Los_Angeles\nprompt: hourly probe\n", encoding="utf-8")

  files, total = _corpus_shape(HOME)
  MANIFEST.write_text(json.dumps({"files": files, "bytes": total}), encoding="utf-8")
  print(f"corpus built: {total / 1e9:.2f} GB across {files} files at {HOME}")


if __name__ == "__main__":
  main()

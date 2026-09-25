"""Fully synthetic, portable homes exercising the eleven migration categories.

Everything under the fixture home is generated at test time (no host
identifiers, no credentials, no private logs): UUID-shaped session ids,
UUID-shaped event/thread ids, one synthetic backend id, and a throwaway git
repo created inside the fixture home for landing checks and cron prompt
resolution. The builder is the single fixture source shared by the
session-tree migration tests.
"""

from __future__ import annotations

import json
import os
import subprocess
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

from src.core.json_utils import atomic_write_text
from src.core.models import (
    PendingTrigger,
    SessionMetadata,
    ThreadMetadata,
)

BASE = datetime(2026, 1, 5, 12, 0, 0, tzinfo=UTC)

_TRANSIENT = {
    "has_running_tasks", "has_pending_trigger", "pending_trigger_count",
    "next_trigger_at", "has_pending_plan_approval", "schedule_cron",
    "schedule_enabled", "schedule_next_run", "schedule_timezone",
    "schedule_project", "schedule_allow_failure", "thinking_since",
}

# Well-known synthetic session ids (UUID-shaped, nothing host-derived).
S_IDENTICAL = "d5d5d555-0000-4000-8000-000000000005"


def iso(offset_minutes: float) -> str:
  return (BASE + timedelta(minutes=offset_minutes)).isoformat()


def _meta_json(meta: SessionMetadata) -> str:
  return meta.model_dump_json(indent=2, exclude=_TRANSIENT)


def ev(event_type: str, offset: float, event_id: str, *, actor: str = "user",
       source_session_id: str | None = None, **extra: Any) -> dict:
  event = {"id": event_id, "type": event_type, "timestamp": iso(offset),
           "actor": actor, "source_session_id": source_session_id}
  event.update(extra)
  return event


class FixtureBuilder:
  """Writes one synthetic home; call helpers then build()."""

  def __init__(self, home: Path) -> None:
    self.home = home
    self.written_metadata: dict[str, SessionMetadata] = {}

  def session(self, meta: SessionMetadata) -> SessionMetadata:
    directory = self.home / "sessions" / meta.id
    (directory / "data").mkdir(parents=True, exist_ok=True)
    (directory / "threads").mkdir(exist_ok=True)
    atomic_write_text(directory / "metadata.json", _meta_json(meta))
    self.written_metadata[meta.id] = meta
    return meta

  def chat_log(self, session_id: str, events: list[dict], *,
               archives: dict[str, list[dict]] | None = None) -> None:
    log = self.home / "sessions" / session_id / "data" / "chat_events.jsonl"
    with open(log, "a", encoding="utf-8") as f:
      for event in events:
        f.write(json.dumps(event) + "\n")
    for name, archive_events in (archives or {}).items():
      archive_dir = self.home / "sessions" / session_id / "data" / "archives"
      archive_dir.mkdir(parents=True, exist_ok=True)
      with open(archive_dir / name, "a", encoding="utf-8") as f:
        for event in archive_events:
          f.write(json.dumps(event) + "\n")

  def thread(self, session_id: str, meta: ThreadMetadata, *,
             events: list[dict] | None = None, raw: str | None = None) -> None:
    directory = self.home / "sessions" / session_id / "threads" / meta.id
    (directory / "data").mkdir(parents=True, exist_ok=True)
    atomic_write_text(directory / "metadata.json", meta.model_dump_json(indent=2))
    if events:
      with open(directory / "data" / "events.jsonl", "a", encoding="utf-8") as f:
        for event in events:
          f.write(json.dumps(event) + "\n")
    if raw is not None:
      (directory / "data" / "agent.raw.ndjson").write_text(raw, encoding="utf-8")

  def master_turn_dir(self, session_id: str, started_at: str, raw: str, *,
                      mtime: str | None = None) -> Path:
    directory = self.home / "sessions" / session_id / "data" / "master_runs" / started_at
    directory.mkdir(parents=True, exist_ok=True)
    raw_path = directory / "agent.raw.ndjson"
    raw_path.write_text(raw, encoding="utf-8")
    if mtime is not None:
      # An offline copy preserves mtimes; the runtime's own completion contract
      # for raw logs reads the last write.
      stamp = datetime.fromisoformat(mtime).timestamp()
      os.utime(raw_path, (stamp, stamp))
    return directory

  def trigger(self, session_id: str, trigger: PendingTrigger) -> None:
    directory = self.home / "sessions" / session_id / "triggers"
    directory.mkdir(parents=True, exist_ok=True)
    atomic_write_text(directory / f"{trigger.id}.json", trigger.model_dump_json(indent=2))

  def loop(self, session_id: str, loop_id: str, files: dict[str, str]) -> Path:
    directory = self.home / "sessions" / session_id / "loops" / loop_id
    directory.mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
      atomic_write_text(directory / name, text)
    return directory

  def cron_config(self, name: str, body_text: str) -> None:
    directory = self.home / "config.d" / "cron.d"
    directory.mkdir(parents=True, exist_ok=True)
    atomic_write_text(directory / f"{name}.yaml", body_text)

  def project(self, group: str, *, common: str, supplement: str | None) -> Path:
    directory = self.home / "projects" / group
    directory.mkdir(parents=True, exist_ok=True)
    body: dict[str, str] = {"prompt_file": "project.md"}
    if supplement is not None:
      body["manager_prompt_file"] = "manager.md"
      atomic_write_text(directory / "manager.md", supplement)
    atomic_write_text(directory / "project.md", common)
    atomic_write_text(
        directory / "project.yaml",
        json.dumps(body, indent=2) + "\n")
    return directory

  def build(self) -> Path:
    (self.home / "sessions").mkdir(parents=True, exist_ok=True)
    (self.home / "config.yaml").write_text(
        "server:\n"
        "  port: 8765\n"
        "backends:\n"
        "  options:\n"
        "    - id: synth\n"
        "      label: Synth\n"
        "      type: cc-claude\n"
        "      model: synth-model\n",
        encoding="utf-8")
    self._build_repo()
    return self.home

  def _build_repo(self) -> None:
    repo = self.home / "repo"
    (repo / "prompts").mkdir(parents=True, exist_ok=True)
    (repo / "prompts" / "nightly.md").write_text("Sweep the nightly checklist.\n", encoding="utf-8")
    env = dict(**__import__("os").environ)
    subprocess.run(
        ["git", "init", "-q", "-b", "main", str(repo)], check=True, env=env)
    git = ["git", "-C", str(repo)]
    subprocess.run([*git, "config", "user.email", "synth@example.invalid"], check=True)
    subprocess.run([*git, "config", "user.name", "Synth Tester"], check=True)
    subprocess.run([*git, "add", "-A"], check=True)
    subprocess.run([*git, "commit", "-qm", "synthetic base"], check=True)


def _round(marker_sid: str, *, input_event: dict | None, done_offset: float,
           done_event_id: str, session_id: str, exit_code: int = 0,
           extra_done: dict | None = None) -> tuple[dict, dict]:
  """One legacy round's chat-log half: run-start marker + its MASTER_DONE."""
  marker = {
      "id": f"m{done_event_id[-11:]}",
      "session_id": marker_sid,
      "timestamp": iso(done_offset - 3),
  }
  done = ev("master_done", done_offset, done_event_id, actor="agent",
            exit_code=exit_code, source_session_id=session_id)
  if input_event is not None:
    done["input_event_id"] = input_event["id"]
  if extra_done:
    done.update(extra_done)
  return marker, done


def _turn_raw(marker_sid: str, *, echo: str | None = None, outcome: str = "success",
              zero_usage: bool = False, with_output: bool = True) -> str:
  """A raw manager log in the claude-family stream shape the producer writes."""
  lines = [json.dumps({"type": "system", "subtype": "init", "session_id": marker_sid})]
  if echo is not None and with_output:
    lines.append(json.dumps({"type": "user", "message": {"role": "user", "content": [
        {"type": "text", "text": echo}]}}))
    lines.append(json.dumps({"type": "assistant", "message": {"content": [
        {"type": "text", "text": "acknowledged"}]}}))
  usage = ({"input_tokens": 0, "output_tokens": 0,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
           if zero_usage else
           {"input_tokens": 100, "output_tokens": 30,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0})
  result: dict = {"type": "result", "is_error": False,
                  "usage": usage}
  if outcome == "success":
    result["subtype"] = "success"
  else:
    result["subtype"] = "error_during_execution"
  lines.append(json.dumps(result))
  return "".join(line + "\n" for line in lines)


def build_identical_requests_home(home: Path) -> Path:
  """Identical input bodies in distinct requests: identity binds each to its own run."""
  builder = FixtureBuilder(home)
  builder.build()
  sid = S_IDENTICAL
  body = "Run the same migration twice"
  u1 = ev("user", 0, f"{sid[:8]}-0000-0000-0000-00000000000a", content=body,
          source_session_id=sid)
  u2 = ev("user", 60, f"{sid[:8]}-0000-0000-0000-00000000000b", content=body,
          source_session_id=sid)
  md1 = ev("master_done", 20, f"{sid[:8]}-0000-0000-0000-00000000000c",
           actor="agent", exit_code=0, input_event_id=u1["id"], source_session_id=sid)
  md2 = ev("master_done", 80, f"{sid[:8]}-0000-0000-0000-00000000000d",
           actor="agent", exit_code=0, input_event_id=u2["id"], source_session_id=sid)
  m1, _ = _round("sid-ident-1", input_event=u1, done_offset=20,
                 done_event_id=md1["id"], session_id=sid)
  m2, _ = _round("sid-ident-2", input_event=u2, done_offset=80,
                 done_event_id=md2["id"], session_id=sid)
  builder.session(SessionMetadata(id=sid, name="Identical requests", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.chat_log(sid, [u1, m1, md1, u2, m2, md2])
  builder.master_turn_dir(sid, iso(10), _turn_raw("sid-ident-1", echo=body),
                          mtime=iso(18))
  builder.master_turn_dir(sid, iso(70), _turn_raw("sid-ident-2", echo=body),
                          mtime=iso(78))
  # A later turn whose stream merely quotes the first request's text: it
  # adopts its own session id and cannot inherit the first round's input.
  quote_raw = (json.dumps({"type": "system", "subtype": "init",
                           "session_id": "sid-ident-quote"}) + "\n"
               + json.dumps({"type": "assistant", "message": {"content": [
                   {"type": "text", "text": "Earlier you asked: " + body}]}}) + "\n"
               + json.dumps({"type": "result", "subtype": "success", "is_error": False,
                             "usage": {"input_tokens": 50, "output_tokens": 10,
                                       "cache_read_input_tokens": 0,
                                       "cache_creation_input_tokens": 0}}) + "\n")
  builder.master_turn_dir(sid, iso(120), quote_raw, mtime=iso(128))
  return home

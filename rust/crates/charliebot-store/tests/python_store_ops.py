"""Python half of the cross-language store wire-format test."""

import asyncio
import json
import os
import pathlib
import shutil
import sys
from datetime import datetime, timezone

from src.app import registrations
from src.infra import config, json_utils, metadata_slots, models
from src.infra import ndjson
from src.runtime import chat_events, session_anchors, session_events, session_lifecycle, session_store

SESSION_ID = "00000000-0000-4000-8000-000000000001"
THREAD_ID = "00000000-0000-4000-8000-000000000002"
FIXED = datetime(2025, 2, 3, 4, 5, 6, 123456, tzinfo=timezone.utc)
OPS = (
    "persist_cc_session_id",
    "persist_native_backend",
    "persist_native_anchor_provenance",
    "clear_cc_session_anchor",
    "persist_account_label",
    "update_thinking_state",
    "mark_unread",
    "rename_session",
    "set_group",
    "switch_backend",
    "archive_session",
    "unarchive_session",
    "update_slot_fields",
)


def _registered() -> None:
  registrations.register_all()
  models.utc_now = lambda: FIXED


def _write_thread(path: pathlib.Path) -> None:
  thread = models.ThreadMetadata(
      id=THREAD_ID,
      session_id=SESSION_ID,
      description="synthetic thread",
      created_at=FIXED,
      claude_session_id="native-thread-id",
  )
  thread.left_by_deleted_package = {"kept": True}
  json_utils.atomic_write_text(path, thread.model_dump_json(indent=2))


async def _seed_one(root: pathlib.Path, operation: str) -> None:
  python_home = root / operation / "python-home"
  rust_home = root / operation / "rust-home"
  py_session = python_home / "sessions" / SESSION_ID
  rust_session = rust_home / "sessions" / SESSION_ID
  py_session.mkdir(parents=True, exist_ok=True)
  rust_session.mkdir(parents=True, exist_ok=True)
  os.environ["CHARLIEBOT_HOME"] = str(python_home)
  cfg = config.load_config()
  store = session_store.SessionStore(cfg)
  status = models.SessionStatus.ARCHIVED if operation == "unarchive_session" else models.SessionStatus.ACTIVE
  meta = models.SessionMetadata(
      id=SESSION_ID,
      name="seed name",
      status=status,
      created_at=FIXED,
      updated_at=FIXED,
      cc_session_id="old-native-id",
      cc_session_started_at=FIXED,
      backend="old-backend",
      native_prompt_hash="old-prompt-hash",
      native_backend="old-backend",
      native_model="old-model",
      group="old-group",
      profile="manager",
  )
  meta.left_by_deleted_package = {"text": "你好", "values": [1, 2.5, 1e16, 1e-5, -0.0]}
  metadata_slots.set_fields(meta, "backend_pool", backend_from_pool=True)
  metadata_slots.set_fields(meta, "cron", last_scheduled_cron="0 0 * * *")
  metadata_slots.set_fields(meta, "slack", slack_watermark_ts="old-slack-watermark")
  metadata_slots.set_fields(meta, "discord", discord_watermark_id="old-discord-watermark")
  meta.claude_account = "old-account"
  await store.save_metadata(meta)

  thread_dir = py_session / "threads" / THREAD_ID
  thread_dir.mkdir(parents=True, exist_ok=True)
  _write_thread(thread_dir / "metadata.json")

  shutil.copy2(py_session / "metadata.json", rust_session / "metadata.json")
  rust_thread = rust_session / "threads" / THREAD_ID
  rust_thread.mkdir(parents=True, exist_ok=True)
  shutil.copy2(thread_dir / "metadata.json", rust_thread / "metadata.json")

  if operation == "event_append":
    event_store = chat_events.ChatEventStore(store.session_dir, store.metadata_path, store.metadata_cache)
    event_store.clear_cache(SESSION_ID)
    await event_store.save_chat_event(
        SESSION_ID,
        {
            "type": "user",
            "id": "synthetic-event-id",
            "timestamp": "2025-02-03T04:05:06.123456+00:00",
            "content": "你好",
        },
    )
    return

  events = session_events.SessionEvents(cfg, store)
  anchors = session_anchors.SessionAnchors(cfg, store, events)
  lifecycle = session_lifecycle.SessionLifecycle(cfg, store, events)
  if operation == "persist_cc_session_id":
    await anchors.persist_cc_session_id(SESSION_ID, "new-native-id", native_backend="new-backend")
  elif operation == "persist_native_backend":
    await anchors.persist_native_backend(SESSION_ID, "new-backend")
  elif operation == "persist_native_anchor_provenance":
    await anchors.persist_native_anchor_provenance(
        SESSION_ID, prompt_hash="new-prompt-hash", native_backend="new-backend", model="new-model")
  elif operation == "clear_cc_session_anchor":
    await anchors.clear_cc_session_anchor(SESSION_ID)
  elif operation == "persist_account_label":
    await anchors.persist_account_label(SESSION_ID, "new-account")
  elif operation == "update_thinking_state":
    await anchors.update_thinking_state(SESSION_ID, FIXED)
  elif operation == "mark_unread":
    await lifecycle.mark_unread(SESSION_ID)
  elif operation == "rename_session":
    await lifecycle.rename_session(SESSION_ID, "renamed session")
  elif operation == "set_group":
    await lifecycle.set_group(SESSION_ID, "new-group")
  elif operation == "switch_backend":
    await lifecycle.switch_backend(SESSION_ID, "new-backend")
  elif operation == "archive_session":
    await lifecycle.archive_session(SESSION_ID)
  elif operation == "unarchive_session":
    await lifecycle.unarchive_session(SESSION_ID)
  elif operation == "update_slot_fields":
    async with store.lock_for(SESSION_ID):
      fresh = await store.get_session_bypassing_cache(SESSION_ID)
      metadata_slots.set_fields(fresh, "slack", slack_watermark_ts="new-slack-watermark")
      await store.save_metadata(fresh, lock_held=True)
  else:
    raise ValueError(f"unknown operation {operation}")


def _prepare(root: pathlib.Path) -> None:
  _registered()
  for operation in (*OPS, "event_append"):
    asyncio.run(_seed_one(root, operation))


def _verify(root: pathlib.Path) -> None:
  _registered()
  for operation in (*OPS, "event_append"):
    rust_home = root / operation / "rust-home"
    session_dir = rust_home / "sessions" / SESSION_ID
    raw = (session_dir / "metadata.json").read_text(encoding="utf-8")
    meta = models.validate_session_metadata(raw, str(session_dir / "metadata.json"))
    assert meta.left_by_deleted_package == {"text": "你好", "values": [1, 2.5, 1e16, 1e-5, -0.0]}
    if operation == "event_append":
      events = ndjson.parse_ndjson_file(session_dir / "data" / "chat_events.jsonl")
      assert events == [
          {
              "type": "user",
              "id": "synthetic-event-id",
              "timestamp": "2025-02-03T04:05:06.123456+00:00",
              "content": "你好",
          }
      ]
    rust_thread = session_dir / "threads" / THREAD_ID / "rust-written.json"
    if rust_thread.exists():
      thread = models.ThreadMetadata.model_validate_json(rust_thread.read_text(encoding="utf-8"))
      assert thread.left_by_deleted_package == {"kept": True}


if __name__ == "__main__":
  mode, root_text = sys.argv[1:]
  root = pathlib.Path(root_text)
  # Package registration reads the environment's home before any operation picks its own, so keep it off the live home.
  os.environ["CHARLIEBOT_HOME"] = str(root / "bootstrap-home")
  if mode == "prepare":
    _prepare(root)
  elif mode == "verify":
    _verify(root)
  else:
    raise ValueError(f"unknown mode {mode}")

"""Python half of the cross-language store wire-format test."""

import asyncio
import json
import os
import pathlib
import shutil
import sys
from collections.abc import Callable
from datetime import datetime, timezone

from src.app import registrations
from src.features.usage import storage_cool
from src.infra import config, json_utils, metadata_slots, models
from src.infra import ndjson
from src.runtime import chat_events, control_events, control_sink, run_token, runs, session_anchors, session_events
from src.runtime import session_fork, session_lifecycle, session_listing, session_sidebar, session_store
from src.runtime import session_successor, takeoff_gate, task_sessions
from src.runtime.hooks import sequence_controllers

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
    "mark_read",
    "rename_session",
    "set_group",
    "rename_group",
    "delete_group",
    "switch_backend",
    "archive_session",
    "unarchive_session",
    "delete_session_permanently",
    "update_slot_fields",
)

RUN_OPS = (
    "register", "retry", "retry_spec", "retry_manager_turn", "launch", "observation", "finish", "repeat_finish", "stop")
RUN_RETRY_OPS = ("retry", "retry_spec", "retry_manager_turn")
RETRY_TASK_SPEC = "synthetic retry task spec"
RUN_SESSION_ID = "00000000-0000-4000-8000-000000000101"
RUN_EVENT_ID = "00000000-0000-4000-8000-000000000199"
RUN_FIXED = datetime(2025, 2, 3, 4, 5, 6, 123456, tzinfo=timezone.utc)
TASK_ROOT_ID = "synthetic-task-root"
TASK_WORKER_ID = "synthetic-task-worker"
TASK_SOURCE_ID = "synthetic-elone-source"
TASK_NEW_ID = "synthetic-elone-new"
TASK_RUN_ID = "synthetic-failed-run"
TASK_COUNTER_ROOT_ID = "synthetic-counter-root"
TASK_ARCHIVE_ROOT_ID = "synthetic-archive-root"
TASK_ARCHIVE_MID_ID = "synthetic-archive-mid"
TASK_ARCHIVE_LEAF_ID = "synthetic-archive-leaf"
TASK_DELEGATE_ROOT_ID = "synthetic-delegate-root"
TASK_DELEGATE_RUN_ID = "synthetic-delegate-run"
FORK_ROOT_ID = "synthetic-fork-root"
FORK_MESSAGES = ("synthetic first message", "synthetic second message", "synthetic third message")
# (case name, operation, event_index): the fixture log holds four lines, so index 3 is its last and 4 is past its end.
FORK_CASES = (
    ("fork-null", "fork", None),
    ("fork-first", "fork", 0),
    ("fork-middle", "fork", 2),
    ("fork-last", "fork", 3),
    ("fork-past-end", "fork", 4),
    ("elone-first", "elone", 0),
    ("elone-middle", "elone", 2),
    ("elone-last", "elone", 3),
    ("elone-past-end", "elone", 4),
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
      has_unread=operation == "mark_read",
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
  elif operation == "mark_read":
    await lifecycle.mark_read(SESSION_ID)
  elif operation == "rename_session":
    await lifecycle.rename_session(SESSION_ID, "renamed session")
  elif operation == "set_group":
    await lifecycle.set_group(SESSION_ID, "new-group")
  elif operation == "rename_group":
    await lifecycle.rename_group("old-group", "new-group")
  elif operation == "delete_group":
    await lifecycle.delete_group("old-group")
  elif operation == "switch_backend":
    await lifecycle.switch_backend(SESSION_ID, "new-backend")
  elif operation == "archive_session":
    await lifecycle.archive_session(SESSION_ID)
  elif operation == "unarchive_session":
    await lifecycle.unarchive_session(SESSION_ID)
  elif operation == "delete_session_permanently":
    await lifecycle.delete_session_permanently(SESSION_ID)
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


async def _seed_old_resource_class(root: pathlib.Path) -> None:
  # A session file of a server before 637df0572: the repair mark is an unknown key that the model keeps.
  python_home = root / "python-home"
  os.environ["CHARLIEBOT_HOME"] = str(python_home)
  store = session_store.SessionStore(config.load_config())
  (python_home / "sessions" / SESSION_ID).mkdir(parents=True, exist_ok=True)
  meta = models.SessionMetadata(
      id=SESSION_ID,
      name="old resource class session",
      created_at=FIXED,
      updated_at=FIXED,
      profile="manager",
  )
  meta.resource_class = "repair"
  await store.save_metadata(meta)


def _prepare_old_resource_class(root: pathlib.Path) -> None:
  _registered()
  asyncio.run(_seed_old_resource_class(root))


def _verify(root: pathlib.Path) -> None:
  _registered()
  for operation in (*OPS, "event_append"):
    rust_home = root / operation / "rust-home"
    session_dir = rust_home / "sessions" / SESSION_ID
    if operation == "delete_session_permanently":
      assert not session_dir.exists()
      continue
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


def _run_record(run_id: str, **fields: object) -> models.RunRecord:
  return models.RunRecord(id=run_id, session_id=RUN_SESSION_ID, **fields)


def _run_seed(root: pathlib.Path, operation: str) -> None:
  home = root / operation / "seed-home"
  session_dir = home / "sessions" / RUN_SESSION_ID
  run_dir = session_dir / "data" / "runs"
  run_dir.mkdir(parents=True, exist_ok=True)
  (home / "config.yaml").write_text("{}\n", encoding="utf-8")
  if operation in RUN_RETRY_OPS:
    manager_turn = operation == "retry_manager_turn"
    previous = _run_record(
        "run-previous",
        kind="manager_turn" if manager_turn else "work",
        input_event_ids=["input-a"] if manager_turn else [],
        backend="backend-a",
        model="model-a",
        repo_path="/repo",
        base_branch="main",
        branch_name="branch-a",
        worktree_path="/repo/worktree",
        sequence_ref={
            "kind": "cron",
            "owner_ref": "sequence-a",
            "position": 2
        },
    )
    (run_dir / previous.id).mkdir()
    (run_dir / previous.id / "metadata.json").write_text(previous.model_dump_json(indent=2), encoding="utf-8")
  elif operation != "register":
    record = _run_record("run-a", input_event_ids=["wrong"] if operation == "repeat_finish" else [])
    (run_dir / record.id).mkdir()
    (run_dir / record.id / "metadata.json").write_text(record.model_dump_json(indent=2), encoding="utf-8")
  if operation in ("finish", "repeat_finish"):
    data_dir = session_dir / "data"
    data_dir.mkdir(parents=True, exist_ok=True)
    chat_path = data_dir / "chat_events.jsonl"
    input_event = {
        "id": "input-a",
        "type": "user",
        "timestamp": "2025-02-03T04:05:06.123456+00:00",
        "content": "synthetic input",
    }
    lines = [json.dumps(input_event, ensure_ascii=False)]
    if operation == "repeat_finish":
      lines.append(
          json.dumps(
              {
                  "id": "00000000-0000-4000-8000-000000000198",
                  "type": "run_finished",
                  "timestamp": "2025-02-03T04:05:06.123456+00:00",
                  "actor": "system",
                  "source_session_id": RUN_SESSION_ID,
                  "run_id": "run-a",
                  "input_event_ids": ["input-a"],
                  "outcome": "failed",
              },
              ensure_ascii=False))
    chat_path.write_text("\n".join(lines) + "\n", encoding="utf-8")


class _RunSink:

  def __init__(self, event_store: chat_events.ChatEventStore) -> None:
    self._event_store = event_store

  async def append(self, session_id: str, event: dict) -> None:
    await self._event_store.save_chat_event(session_id, event)

  async def notify_tree_changed(self, session_id: str, event_type: str | None) -> None:
    return None

  def load_events(self, session_id: str) -> list[dict]:
    return self._event_store.load_chat_events_sync(session_id)


def _python_run_store(home: pathlib.Path) -> runs.RunStore:
  os.environ["CHARLIEBOT_HOME"] = str(home)
  cfg = config.load_config()
  sessions = session_store.SessionStore(cfg)
  event_store = chat_events.ChatEventStore(sessions.session_dir, sessions.metadata_path, sessions.metadata_cache)
  return runs.RunStore(cfg.sessions_dir, asyncio.Lock(), _RunSink(event_store))


def _patch_control_event_ids() -> None:
  original = control_events.build_control_event

  def build(
      event_type: str, *, actor: str, source_session_id: str, event_id: str | None = None, **payload: object) -> dict:
    if event_type in ("run_finished", "run_stop_requested"):
      event_id = RUN_EVENT_ID
    return original(event_type, actor=actor, source_session_id=source_session_id, event_id=event_id, **payload)

  control_events.build_control_event = build


def _prepare_runs(root: pathlib.Path) -> None:
  registrations.register_all()
  for operation in RUN_OPS:
    _run_seed(root, operation)


async def _apply_python_run(home: pathlib.Path, operation: str) -> None:
  store = _python_run_store(home)
  if operation == "register":
    record = _run_record("run-register")
    await store.register_run_locked(record, task_spec_text="synthetic task spec")
    await store.register_run_locked(_run_record("run-register", kind="review"), task_spec_text="ignored replacement")
  elif operation in RUN_RETRY_OPS:
    previous = store.read_run_sync(RUN_SESSION_ID, "run-previous")
    assert previous is not None
    # The same keyword set as TaskTreeManager.create_retry: a manager-round retry binds the original's batch.
    fields = dict(
        kind=previous.kind,
        backend=previous.backend,
        model=previous.model,
        repo_path=previous.repo_path,
        base_branch=previous.base_branch,
        branch_name=previous.branch_name,
        worktree_path=previous.worktree_path,
        sequence_ref=previous.sequence_ref,
        review_of_run_id=previous.review_of_run_id,
    )
    if operation == "retry_manager_turn":
      fields["input_event_ids"] = list(previous.input_event_ids)
    task_spec_text = None if operation == "retry" else RETRY_TASK_SPEC
    async with store._lock:
      # The second call replays the first request and must return the same record.
      for _ in range(2):
        await store.create_retry_run_locked(
            RUN_SESSION_ID, "retry-request", previous.id, task_spec_text=task_spec_text, **fields)
  elif operation == "launch":
    await store.record_launch(RUN_SESSION_ID, "run-a", pid=424242, pid_start="12345")
  elif operation == "observation":
    await store.record_observation(
        RUN_SESSION_ID,
        "run-a",
        native_session_id="native-a",
        model="model-b",
        raw_log_ref="/tmp/run.raw",
    )
  elif operation == "finish":
    await store.record_finish(
        RUN_SESSION_ID,
        "run-a",
        "success",
        input_event_ids=["input-a"],
        exit_code=0,
        ended_at=RUN_FIXED,
    )
  elif operation == "repeat_finish":
    await store.record_finish(
        RUN_SESSION_ID,
        "run-a",
        "success",
        input_event_ids=["ignored-input"],
        exit_code=0,
        ended_at=RUN_FIXED,
    )
  elif operation == "stop":
    await store.request_stop(RUN_SESSION_ID, "run-a", "stop-request")
  else:
    raise ValueError(f"unknown run operation {operation}")


def _apply_runs(root: pathlib.Path) -> None:
  registrations.register_all()
  _patch_control_event_ids()
  for operation in RUN_OPS:
    operation_root = root / operation
    shared_home = operation_root / "shared-home"
    if shared_home.exists():
      shutil.rmtree(shared_home)
    shutil.copytree(operation_root / "seed-home", shared_home)
    asyncio.run(_apply_python_run(shared_home, operation))
    shutil.copytree(shared_home, operation_root / "python-result")


def _verify_runs(root: pathlib.Path) -> None:
  registrations.register_all()
  for operation in RUN_OPS:
    home = root / operation / "shared-home"
    if operation == "register":
      run_id = "run-register"
    elif operation in RUN_RETRY_OPS:
      run_id = control_events.stable_run_id(RUN_SESSION_ID, "retry-request")
    else:
      run_id = "run-a"
    record = _python_run_store(home).read_run_sync(RUN_SESSION_ID, run_id)
    assert record is not None
    assert record.id == run_id
    if operation == "register":
      assert pathlib.Path(record.task_spec_ref).read_text(encoding="utf-8") == "synthetic task spec"
    if operation in RUN_RETRY_OPS:
      assert record.retry_of_run_id == "run-previous"
      assert record.backend == "backend-a"
    if operation == "retry_spec":
      assert pathlib.Path(record.task_spec_ref).read_text(encoding="utf-8") == RETRY_TASK_SPEC
    if operation == "retry_manager_turn":
      assert record.kind == "manager_turn" and record.input_event_ids == ["input-a"]
    if operation == "launch":
      assert record.pid == 424242 and record.pid_start == "12345"
    if operation == "observation":
      assert record.native_session_id == "native-a" and record.model == "model-b"
      assert record.raw_log_ref == "/tmp/run.raw"
    if operation == "finish":
      assert record.input_event_ids == ["input-a"] and record.exit_code == 0
    if operation == "repeat_finish":
      assert record.input_event_ids == ["input-a"] and record.exit_code == 0 and record.ended_at == RUN_FIXED
    if operation == "stop":
      facts = ndjson.parse_ndjson_file(home / "sessions" / RUN_SESSION_ID / "data" / "chat_events.jsonl")
      assert len(facts) == 1 and facts[0]["request_id"] == "stop-request"


# ---------------------------------------------------------------------------
# Cross-implementation read-parity cases: each computes the Python answer for a
# fixture home the Rust test builds and copies, as JSON for the Rust side.
# ---------------------------------------------------------------------------


def _events_store(home: pathlib.Path) -> chat_events.ChatEventStore:

  def session_dir(session_id: str) -> pathlib.Path:
    return home / "sessions" / session_id

  def metadata_path(session_id: str) -> pathlib.Path:
    return session_dir(session_id) / "metadata.json"

  return chat_events.ChatEventStore(session_dir, metadata_path, {})


def _fact_history(store: chat_events.ChatEventStore, session_id: str) -> list[dict]:
  """task_sessions.fact_history over the event store, with only session_events set on the manager."""
  manager = task_sessions.TaskTreeManager.__new__(task_sessions.TaskTreeManager)
  manager.session_events = store
  return manager.fact_history(session_id)


def _range_read(root: pathlib.Path, home: pathlib.Path, session_id: str) -> None:
  store = _events_store(home)
  windows = [(6, 7), (7, 9), (8, 10), (6, 10), (9, 12), (11, 15), (7, 7), (0, 3), (3, 5)]
  result = []
  for start, end in windows:
    events, has_more = store.load_chat_events_range(session_id, start, end)
    result.append({"start": start, "end": end, "has_more": has_more, "events": events})
  (root / "expected.json").write_text(json.dumps(result), encoding="utf-8")


def _listing_raw_scan(root: pathlib.Path, home: pathlib.Path, session_id: str) -> None:
  os.environ["CHARLIEBOT_HOME"] = str(home)
  cfg = config.load_config()
  store = session_store.SessionStore(cfg)
  metas = asyncio.run(store.load_session_metas())
  listed = {meta.id: meta.model_dump(mode="json") for meta in metas}
  # The storage sweep's own scan: a missing or malformed metadata file drops out after load_json_meta logs it.
  raw = sorted(storage_cool._scan_sessions(cfg, datetime.now(timezone.utc), 0))
  (root / "expected.json").write_text(json.dumps({"listed": listed, "raw": raw}), encoding="utf-8")


def _facts(root: pathlib.Path, home: pathlib.Path, session_id: str) -> None:
  store = _events_store(home)
  events = _fact_history(store, session_id)
  (root / "expected.json").write_text(json.dumps(events), encoding="utf-8")


def _archive_cases(root: pathlib.Path, cases: list[str]) -> None:
  """Run archive_old_chat_events_sync on each case's Python home; a failed call archives nothing."""
  cutoff = datetime(2025, 2, 3, tzinfo=timezone.utc)
  result = {}
  for case in cases:
    store = _events_store(root / case / "python-home")
    try:
      archived = store.archive_old_chat_events_sync(SESSION_ID, cutoff)
    except (UnicodeDecodeError, AttributeError, TypeError) as error:
      result[case] = {"failed": True, "error": type(error).__name__, "events_archived": 0}
    else:
      result[case] = {"failed": False, "error": None, "events_archived": archived["events_archived"]}
  (root / "archive-cases.json").write_text(json.dumps(result), encoding="utf-8")


CASES = {
    "range_read": _range_read,
    "listing_raw_scan": _listing_raw_scan,
    "facts": _facts,
}


def _parity_case(case: str, root: pathlib.Path, home: pathlib.Path, session_id: str) -> None:
  if case not in CASES:
    raise ValueError(f"unknown parity case {case}")
  CASES[case](root, home, session_id)


def _page_json(page: runs.RunPageSlice) -> dict[str, object]:
  return {"ids": [run.id for run in page.items], "next_cursor": page.next_cursor}


def _run_cursor(payload: dict[str, object]) -> str:
  return run_token.b64url_encode(json.dumps(payload, separators=(",", ":")).encode("utf-8"))


def _refusal(read: Callable[[], object]) -> str:
  try:
    read()
  except ValueError as error:
    return str(error)
  raise AssertionError("the cursor was accepted under the other order")


def _read_runs(root: pathlib.Path, case: str) -> dict[str, object]:
  """Run the Python side of one read case on the case's Python copy and return its JSON-shaped result."""
  registrations.register_all()
  store = _python_run_store(root / case / "python-home")
  if case == "pages":
    ascending = store.list_runs_page_sync(RUN_SESSION_ID, limit=2, cursor=None)
    descending = store.list_runs_page_sync(RUN_SESSION_ID, limit=2, cursor=None, descending=True)
    descending_next = store.list_runs_page_sync(RUN_SESSION_ID, limit=2, cursor=descending.next_cursor, descending=True)
    empty_started_at = store.list_runs_page_sync(
        RUN_SESSION_ID, limit=2, cursor=_run_cursor({
            "s": "",
            "i": "queued-0"
        }))
    truthy_order_flag = store.list_runs_page_sync(
        RUN_SESSION_ID,
        limit=2,
        cursor=_run_cursor({
            "s": "2026-01-01T00:04:00+00:00",
            "i": "run-4",
            "d": "false"
        }),
        descending=True)
    return {
        "ascending":
            _page_json(ascending),
        "descending":
            _page_json(descending),
        "descending_next":
            _page_json(descending_next),
        "empty_started_at":
            _page_json(empty_started_at),
        "truthy_order_flag":
            _page_json(truthy_order_flag),
        "refused_ascending_cursor":
            _refusal(
                lambda: store.list_runs_page_sync(
                    RUN_SESSION_ID, limit=2, cursor=ascending.next_cursor, descending=True)),
        "refused_descending_cursor":
            _refusal(lambda: store.list_runs_page_sync(RUN_SESSION_ID, limit=2, cursor=descending.next_cursor)),
    }
  if case == "blockers":
    # The same collection as TaskTreeManager._structural_blockers: every run's blocker, in run order.
    events = store.load_events_sync(RUN_SESSION_ID)
    host_boot = runs.read_host_boot_time()
    blockers = []
    for run in store.list_run_records_sync(RUN_SESSION_ID):
      blocker = store.run_blocker(run, events, host_boot)
      if blocker is not None:
        blockers.append(blocker)
    return {"blockers": blockers}
  if case == "last_ended":
    return {"last_ended_run_id": store.last_ended_run_id(RUN_SESSION_ID)}
  if case == "terminal_outcome":
    events = store.load_events_sync(RUN_SESSION_ID)
    return {"terminal_outcome": store.terminal_outcome(events, "run-two-finishes")}
  raise ValueError(f"unknown read case {case}")


def _task_tree(home: pathlib.Path) -> task_sessions.TaskTreeManager:
  return _task_tree_and_fork(home)[0]


def _task_tree_and_fork(home: pathlib.Path) -> tuple[task_sessions.TaskTreeManager, session_fork.SessionFork]:
  os.environ["CHARLIEBOT_HOME"] = str(home)
  cfg = config.load_config()
  store = session_store.SessionStore(cfg)
  sidebar = session_sidebar.SessionSidebar(cfg, store)
  events = session_events.SessionEvents(cfg, store)
  listing = session_listing.SessionListing(cfg, store, sidebar)
  lifecycle = session_lifecycle.SessionLifecycle(cfg, store, events)
  fork = session_fork.SessionFork(cfg, store, events)
  anchors = session_anchors.SessionAnchors(cfg, store, events)
  successor = session_successor.SessionSuccessor(cfg, store, events)
  tree = task_sessions.TaskTreeManager(cfg, store, events, sidebar, listing, lifecycle, fork, anchors, successor)
  return tree, fork


class _TestSequenceController:

  def binding(self, _session_id: str) -> None:
    return None

  async def move_bindings(self, _source_id: str, _target_id: str) -> bool:
    return True


def _install_test_sequence_controller() -> None:
  sequence_controllers.sequence_controllers = lambda: (_TestSequenceController(),)


async def _create_task_node(
    tree: task_sessions.TaskTreeManager,
    *,
    session_id: str,
    parent_id: str | None,
    request_id: str,
    profile: str = "manager",
    slots: dict[str, object] | None = None,
) -> models.SessionMetadata:
  return await tree.create_task(
      request_id=request_id,
      task_parent_id=parent_id,
      profile=profile,
      task=None,
      name=request_id,
      backend="synthetic",
      session_id=session_id,
      slot_values=slots,
      caller="system",
  )


async def _prepare_tasks(root: pathlib.Path) -> None:
  _registered()
  home = root / "task-base"
  tree = _task_tree(home)
  manager = await _create_task_node(tree, session_id=TASK_ROOT_ID, parent_id=None, request_id="root-request")
  worker = await _create_task_node(
      tree, session_id=TASK_WORKER_ID, parent_id=manager.id, request_id="worker-request", profile="worker")
  source = await _create_task_node(
      tree,
      session_id=TASK_SOURCE_ID,
      parent_id=manager.id,
      request_id="source-request",
      slots={
          "last_scheduled_run": FIXED.isoformat(),
          "last_scheduled_cron": "0 0 * * *",
          "last_run_status": "success",
      },
  )
  await _create_task_node(tree, session_id=TASK_NEW_ID, parent_id=manager.id, request_id="new-request")

  record = models.RunRecord(
      id=TASK_RUN_ID,
      session_id=worker.id,
      ended_at=FIXED,
      exit_code=1,
  )
  run_path = home / "sessions" / worker.id / "data" / "runs" / TASK_RUN_ID / "metadata.json"
  run_path.parent.mkdir(parents=True, exist_ok=True)
  json_utils.atomic_write_text(run_path, record.model_dump_json(indent=2))
  event = control_events.build_control_event(
      "run_finished",
      actor="system",
      source_session_id=worker.id,
      event_id="synthetic-failed-run-fact",
      timestamp=FIXED.isoformat(),
      run_id=TASK_RUN_ID,
      input_event_ids=[],
      outcome="failed",
  )
  async with tree.control_lock:
    await tree.events.append(worker.id, event)


async def _mutate_tasks(home: pathlib.Path) -> None:
  tree = _task_tree(home)
  _install_test_sequence_controller()
  child = await tree.create_task(
      request_id="replayed-child-request",
      task_parent_id=TASK_ROOT_ID,
      profile="manager",
      task=None,
      name="replayed child",
      backend="synthetic",
      caller="system",
  )
  replay = await tree.create_task(
      request_id="replayed-child-request",
      task_parent_id=TASK_ROOT_ID,
      profile="manager",
      task=None,
      name="ignored replay name",
      backend="synthetic",
      caller="system",
  )
  assert replay.id == child.id
  await tree.update_slot_fields(TASK_WORKER_ID, "slack", slack_watermark_ts="updated-watermark")
  assert await tree.completion.archive_unsuccessful_worker(TASK_WORKER_ID, run_id=TASK_RUN_ID, outcome="failed")
  assert not await tree.completion.archive_unsuccessful_worker(TASK_WORKER_ID, run_id=TASK_RUN_ID, outcome="failed")
  source = await tree.load_task_meta(TASK_SOURCE_ID)
  new = await tree.load_task_meta(TASK_NEW_ID)
  await tree._rebind_scheduled_tasks(source, new)


async def _verify_tasks(home: pathlib.Path) -> None:
  tree = _task_tree(home)
  meta = await tree.load_meta(TASK_ROOT_ID)
  assert meta is not None and meta.profile == "manager"
  index = await tree._get_index()
  children = tree._children_of(index, TASK_ROOT_ID)
  assert len(children) == 4
  worker = await tree.load_task_meta(TASK_WORKER_ID)
  assert metadata_slots.fields_of(worker, "slack").slack_watermark_ts == "updated-watermark"
  assert tree.task_state(TASK_WORKER_ID) == "archived"
  target = await tree.load_task_meta(TASK_NEW_ID)
  assert metadata_slots.fields_of(target, "cron").last_scheduled_cron == "0 0 * * *"
  assert metadata_slots.fields_of(target, "cron").last_run_status == "success"
  matches = []
  for session_id in children:
    child = await tree.load_meta(session_id)
    if child is not None and child.name == "replayed child":
      matches.append(session_id)
  assert len(matches) == 1


async def _verify_rust_created_tree(home: pathlib.Path) -> None:
  tree = _task_tree(home)
  root = await tree.load_task_meta("rust-created-root")
  child = await tree.load_task_meta("rust-created-child")
  assert root.profile == "manager" and child.task_parent_id == root.id
  assert tree._children_of(await tree._get_index(), root.id) == [child.id]


def _prepare_tasks_mode(root: pathlib.Path) -> None:
  asyncio.run(_prepare_tasks(root))


def _mutate_tasks_mode(root: pathlib.Path, home_name: str) -> None:
  _registered()
  asyncio.run(_mutate_tasks(root / home_name))


def _verify_tasks_mode(root: pathlib.Path, home_name: str) -> None:
  _registered()
  asyncio.run(_verify_tasks(root / home_name))


def _verify_rust_created_mode(root: pathlib.Path) -> None:
  _registered()
  asyncio.run(_verify_rust_created_tree(root / "rust-created-home"))


async def _prepare_counter(root: pathlib.Path) -> None:
  _registered()
  home = root / "counter-base"
  tree = _task_tree(home)
  await _create_task_node(tree, session_id=TASK_COUNTER_ROOT_ID, parent_id=None, request_id="counter-root")
  await _create_task_node(
      tree, session_id="synthetic-counter-child", parent_id=TASK_COUNTER_ROOT_ID, request_id="counter-child")
  (home / "sessions" / ".counter").write_text("not-a-number", encoding="utf-8")


def _prepare_counter_mode(root: pathlib.Path) -> None:
  asyncio.run(_prepare_counter(root))


async def _create_unnamed_node(home: pathlib.Path) -> None:
  _registered()
  tree = _task_tree(home)
  node = await tree.create_task(
      request_id="counter-request",
      task_parent_id=TASK_COUNTER_ROOT_ID,
      profile="manager",
      task=None,
      name=None,
      backend="synthetic",
      caller="system",
  )
  assert node.name == "Session 2", node.name


def _create_unnamed_node_mode(root: pathlib.Path, home_name: str) -> None:
  asyncio.run(_create_unnamed_node(root / home_name))


async def _prepare_archive(root: pathlib.Path) -> None:
  _registered()
  tree = _task_tree(root / "archive-base")
  await _create_task_node(tree, session_id=TASK_ARCHIVE_ROOT_ID, parent_id=None, request_id="archive-root")
  await _create_task_node(
      tree, session_id=TASK_ARCHIVE_MID_ID, parent_id=TASK_ARCHIVE_ROOT_ID, request_id="archive-mid")
  await _create_task_node(
      tree,
      session_id=TASK_ARCHIVE_LEAF_ID,
      parent_id=TASK_ARCHIVE_MID_ID,
      request_id="archive-leaf",
      profile="worker",
  )


async def _archive_subtree(home: pathlib.Path) -> None:
  _registered()
  tree = _task_tree(home)
  _install_test_sequence_controller()
  archived = await tree.completion.archive_subtree(
      TASK_ARCHIVE_ROOT_ID,
      request_id="synthetic-archive-request",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=run_token.CallerIdentity(kind="operator"))
  assert archived == [TASK_ARCHIVE_ROOT_ID, TASK_ARCHIVE_MID_ID, TASK_ARCHIVE_LEAF_ID], archived


def _prepare_archive_mode(root: pathlib.Path) -> None:
  asyncio.run(_prepare_archive(root))


def _archive_subtree_mode(root: pathlib.Path, home_name: str) -> None:
  asyncio.run(_archive_subtree(root / home_name))


async def _prepare_fork(root: pathlib.Path) -> None:
  _registered()
  home = root / "fork-base"
  tree = _task_tree(home)
  await _create_task_node(tree, session_id=FORK_ROOT_ID, parent_id=None, request_id="fork-root")
  with (home / "sessions" / FORK_ROOT_ID / "data" / "chat_events.jsonl").open("a", encoding="utf-8") as out:
    for content in FORK_MESSAGES:
      event = {"type": "user", "actor": "user", "source_session_id": FORK_ROOT_ID, "content": content}
      out.write(json.dumps(event, ensure_ascii=False) + "\n")


async def _fork_op(home: pathlib.Path, operation: str, event_index: int | None) -> dict[str, str]:
  _, fork = _task_tree_and_fork(home)
  _install_test_sequence_controller()
  try:
    if operation == "fork":
      await fork.fork_session(FORK_ROOT_ID, event_index, backend="synthetic")
    else:
      await fork.elone_session(FORK_ROOT_ID, event_index, backend="synthetic")
  except ValueError as error:
    return {"outcome": "refused", "error": str(error)}
  return {"outcome": "created"}


def _fork_cases_mode(root: pathlib.Path) -> None:
  _registered()
  outcomes = {}
  for name, operation, event_index in FORK_CASES:
    home = root / f"python-{name}"
    shutil.copytree(root / "fork-base", home)
    outcomes[name] = asyncio.run(_fork_op(home, operation, event_index))
  # The session log writes its spawn records to stdout, so the outcomes go to a file the test reads.
  (root / "fork-outcomes.json").write_text(json.dumps(outcomes, sort_keys=True) + "\n", encoding="utf-8")


async def _prepare_delegate(root: pathlib.Path) -> None:
  _registered()
  tree = _task_tree(root / "delegate-base")
  await _create_task_node(tree, session_id=TASK_DELEGATE_ROOT_ID, parent_id=None, request_id="delegate-root")
  request = control_events.build_control_event(
      "user", actor="user", source_session_id=TASK_DELEGATE_ROOT_ID, content="Ship the synthetic change.")
  async with tree.control_lock:
    await tree.events.append(TASK_DELEGATE_ROOT_ID, request)


async def _delegate_task(home: pathlib.Path) -> None:
  _registered()
  tree = _task_tree(home)
  spec = models.TaskSpec(
      goal="## Goal\n\nShip the synthetic change.\n\nKeep the public API.",
      acceptance=["the synthetic tests pass"],
      context_refs=["docs/synthetic.md"],
      repo_path="/home/synthetic/repo",
      base_branch="main",
      task_type=models.TaskType.IMPLEMENT,
      keep_worktree=True,
  )
  # The internal delegate route's shape: an agent caller, a worker child, then the work run registered with the spec.
  caller = run_token.CallerIdentity(
      kind="agent", claims=run_token.RunTokenClaims(TASK_DELEGATE_ROOT_ID, "synthetic-manager-run", "manager"))
  child = await tree.create_task(
      request_id="delegate-request",
      task_parent_id=TASK_DELEGATE_ROOT_ID,
      profile="worker",
      task=spec,
      name=None,
      backend="synthetic",
      caller=caller,
  )
  assert child.name == "Ship the synthetic change.", child.name
  record = models.RunRecord(
      id=TASK_DELEGATE_RUN_ID,
      session_id=child.id,
      kind="work",
      backend="synthetic",
      repo_path=spec.repo_path,
      base_branch=spec.base_branch,
  )
  async with tree.control_lock:
    await tree.runs.register_run_locked(record, task_spec_text=task_sessions.canonical_task_spec_text(spec))


def _prepare_delegate_mode(root: pathlib.Path) -> None:
  asyncio.run(_prepare_delegate(root))


def _delegate_task_mode(root: pathlib.Path, home_name: str) -> None:
  asyncio.run(_delegate_task(root / home_name))


def _gate_parity(root: pathlib.Path) -> None:
  cases = json.loads((root / "gate-cases.json").read_text(encoding="utf-8"))["cases"]
  results = {}
  for case in cases:
    try:
      user_node = takeoff_gate.check_takeoff_gate_for_task(
          case["start"],
          load_events=lambda session_id: case["events"].get(session_id, []),
          task_meta_of=lambda session_id: (case["parents"].get(session_id), case["profiles"].get(session_id)),
          task_state_of=lambda session_id: case["states"].get(session_id, "open"),
      )
    except takeoff_gate.DelegationBlockedError as error:
      results[case["name"]] = {"authorized": False, "user_node": None, "reason": str(error)}
    else:
      results[case["name"]] = {"authorized": True, "user_node": user_node, "reason": None}
  (root / "gate-results.json").write_text(json.dumps(results, sort_keys=True) + "\n", encoding="utf-8")


def _prepare_parity_gate(root: pathlib.Path) -> None:
  from rust.parity import fixture
  from rust.parity.servers import Tree, write_home_config
  from src.runtime import task_execution

  fixture_root = root / "parity-fixture"
  home = fixture_root / "home"
  workspace = fixture_root / "workspace"
  worktrees = fixture_root / "worktrees"
  for path in (home, workspace, worktrees, fixture_root / "tmp", fixture_root / "logs", fixture_root / "fake-agent"):
    path.mkdir(parents=True, exist_ok=True)
  tree = Tree("fixture", fixture_root)
  write_home_config(tree, 18642)
  os.environ["CHARLIEBOT_HOME"] = str(home)
  manifest_path = root / "parity-fixture-manifest.json"
  previous_argv = sys.argv
  sys.argv = ["fixture.py", "--manifest", str(manifest_path)]
  try:
    fixture.main()
  finally:
    sys.argv = previous_argv
  task_tree = task_execution.task_manager()
  manifest = json.loads(manifest_path.read_text(encoding="utf-8"))

  async def collect() -> dict[str, dict[str, object]]:
    results = {}
    for session_id in sorted(manifest["sessions"].values()):
      try:
        user_node = await task_tree.check_task_authorization(session_id)
      except takeoff_gate.DelegationBlockedError as error:
        results[session_id] = {"authorized": False, "user_node": None, "reason": str(error)}
      else:
        results[session_id] = {"authorized": True, "user_node": user_node, "reason": None}
    return results

  results = asyncio.run(collect())
  (root / "parity-gate-results.json").write_text(json.dumps(results, sort_keys=True) + "\n", encoding="utf-8")
  (root / "parity-fixture-sessions.txt").write_text(
      "\n".join(sorted(manifest["sessions"].values())) + "\n", encoding="utf-8")


if __name__ == "__main__":
  mode, root_text, *extra = sys.argv[1:]
  root = pathlib.Path(root_text)
  # Package registration reads the environment's home before any operation picks its own, so keep it off the live home.
  os.environ["CHARLIEBOT_HOME"] = str(root / "bootstrap-home")
  if mode == "prepare":
    _prepare(root)
  elif mode == "prepare_old_resource_class":
    _prepare_old_resource_class(root)
  elif mode == "verify":
    _verify(root)
  elif mode == "prepare_runs":
    _prepare_runs(root)
  elif mode == "apply_runs":
    _apply_runs(root)
  elif mode == "verify_runs":
    _verify_runs(root)
  elif mode.startswith("parity:"):
    _case, case = mode.split(":", 1)
    _parity_case(case, root, root / "python-home", extra[0])
  elif mode == "read_runs":
    print(json.dumps(_read_runs(root, *extra), ensure_ascii=False))
  elif mode == "prepare_tasks":
    _prepare_tasks_mode(root)
  elif mode == "mutate_tasks":
    _mutate_tasks_mode(root, extra[0])
  elif mode == "verify_tasks":
    _verify_tasks_mode(root, extra[0])
  elif mode == "verify_rust_created":
    _verify_rust_created_mode(root)
  elif mode == "prepare_counter":
    _prepare_counter_mode(root)
  elif mode == "create_unnamed_node":
    _create_unnamed_node_mode(root, extra[0])
  elif mode == "prepare_archive":
    _prepare_archive_mode(root)
  elif mode == "archive_subtree":
    _archive_subtree_mode(root, extra[0])
  elif mode == "prepare_delegate":
    _prepare_delegate_mode(root)
  elif mode == "delegate_task":
    _delegate_task_mode(root, extra[0])
  elif mode == "prepare_fork":
    asyncio.run(_prepare_fork(root))
  elif mode == "fork_cases":
    _fork_cases_mode(root)
  elif mode == "gate_parity":
    _gate_parity(root)
  elif mode == "prepare_parity_gate":
    _prepare_parity_gate(root)
  elif mode == "archive_cases":
    _archive_cases(root, extra)
  else:
    raise ValueError(f"unknown mode {mode}")

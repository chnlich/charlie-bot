"""The Threads view's route: one task's Runs as thread rows."""

import asyncio
import hashlib
from datetime import UTC, datetime

from fastapi import APIRouter, Query, Request, Response

from src.infra.memo import BoundedMemo
from src.infra.models import RunRecord
from src.infra.responses import fast_json_bytes, gzip_body_response
from src.runtime.api.deps import get_task_manager
from src.runtime.runs import read_host_boot_time, stop_requested_in_events, terminal_outcome_in_events

router = APIRouter()
_LIST_DESCRIPTION_CAP = 100
_RUN_LIST_GZIP_MEMO: BoundedMemo[bytes, bytes] = BoundedMemo(8)


def _run_status(run: RunRecord, events: list[dict], host_boot: datetime) -> str:
  """The thread-row status string of a task Run."""
  from src.runtime.runs import is_run_alive

  outcome = terminal_outcome_in_events(events, run.id)
  if outcome == "success":
    return "completed"
  if outcome is not None:
    return "failed"
  if run.pid is None:
    return "cancelled" if stop_requested_in_events(events, run.id) else "idle"
  return "running" if is_run_alive(run.pid, run.pid_start, run.started_at, host_boot) else "failed"


def _run_list_item(
    run: RunRecord,
    events: list[dict],
    host_boot: datetime,
    *,
    created_at: datetime,
    description: str,
) -> dict:
  """Render one thread row from the Run record and facts."""
  item = {
      "type": "thread",
      "id": run.id,
      "description": description[:_LIST_DESCRIPTION_CAP],
      "status": _run_status(run, events, host_boot),
      "created_at": int(created_at.timestamp() * 1000),
      "completed_at": int(run.ended_at.timestamp() * 1000) if run.ended_at else None,
      "backend": run.backend,
      "session_id": run.session_id,
  }
  if len(description) > _LIST_DESCRIPTION_CAP:
    item["description_full_len"] = len(description)
  if run.pid is not None:
    item["pid"] = run.pid
  if run.branch_name:
    item["branch_name"] = run.branch_name
  if run.worktree_path:
    item["worktree_path"] = run.worktree_path
  return item


@router.get("/{session_id}/list")
async def list_threads(
    request: Request,
    session_id: str,
    etag: str | None = Query(default=None),
) -> Response:
  """List task Runs as thread rows, newest first."""
  tree = await get_task_manager()
  meta = await tree.load_meta(session_id)
  description = (meta.task.goal if meta is not None and meta.task is not None else "") or ""
  events = tree.runs.load_events_sync(session_id)
  host_boot = await asyncio.to_thread(read_host_boot_time)
  rows = []
  for run in await asyncio.to_thread(tree.runs.list_run_records_sync, session_id):
    created_at = run.started_at or run.ended_at or datetime.now(UTC)
    rows.append(_run_list_item(run, events, host_boot, created_at=created_at, description=description))
  rows.sort(key=lambda row: row["created_at"], reverse=True)
  body = fast_json_bytes(rows)
  etag_value = '"' + hashlib.sha1(body).hexdigest() + '"'
  if etag == etag_value:
    return Response(status_code=204, headers={"ETag": etag_value, "Cache-Control": "no-store"})
  return await gzip_body_response(request, body, {"ETag": etag_value, "Cache-Control": "no-store"}, _RUN_LIST_GZIP_MEMO)

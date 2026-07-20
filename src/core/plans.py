"""Plan registry — per-session plan lineage state and verify-outcome mapping.

This module is the single authority for the verify-result trailer regex, the
final-report reader, the verify-outcome mapping function, the derived-state
function, and the per-session plan registry mutations. It must NOT import
``src.core.spawner``; the dependency direction is spawner → plans.
"""

import asyncio
import json
import os
import re
from datetime import datetime, timezone
from enum import IntEnum
from pathlib import Path
from typing import Optional

import structlog

from src.api.message_utils import extract_text_from_message
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import TaskType, ThreadMetadata, ThreadStatus, utc_now
from src.core.ndjson import parse_ndjson_file
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager
from src.core.threads import ThreadManager

log = structlog.get_logger()

# ---------------------------------------------------------------------------
# Verify-result trailer — single authority
# ---------------------------------------------------------------------------

VERIFY_RESULT_TRAILER_RE = re.compile(r"RESULT: (?:clean|[1-9][0-9]* mismatch(?:es)? \([0-9]+ approval\))")
VERIFY_RESULT_TRAILER_EXPECTED = f"`{VERIFY_RESULT_TRAILER_RE.pattern}`"


async def read_verify_final_report(session_id: str, thread_id: str, thread_mgr: ThreadManager) -> str:
  """Read the verifier's complete final result, falling back to its last assistant text."""
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  events = await asyncio.to_thread(parse_ndjson_file, events_path)

  for ev in reversed(events):
    if ev.get("type") != ET.RESULT:
      continue
    result = ev.get("result")
    if isinstance(result, str) and result.strip():
      return result
    break

  for ev in reversed(events):
    if ev.get("type") != ET.ASSISTANT:
      continue
    message = ev.get("message") if isinstance(ev.get("message"), dict) else None
    text = extract_text_from_message(message)
    if text.strip():
      return text
  return ""


def verify_result_trailer_error(report: str) -> str:
  """Return an explicit verifier completion error, or an empty string for a valid trailer."""
  expected = VERIFY_RESULT_TRAILER_EXPECTED
  if not report.strip():
    return f"Verifier final report is empty; expected a final {expected} line."
  final_line = report.splitlines()[-1]
  if VERIFY_RESULT_TRAILER_RE.fullmatch(final_line):
    return ""
  return f"Verifier final report has a missing or malformed `RESULT:` trailer; expected a final {expected} line."


# ---------------------------------------------------------------------------
# VerifyState — internal enum (0 = UNKNOWN reserved); stored/API use strings
# ---------------------------------------------------------------------------


class _VerifyState(IntEnum):
  UNKNOWN = 0
  PENDING = 1
  CLEAN = 2
  MISMATCH = 3
  FAILED = 4


_VERIFY_STATE_STR: dict[_VerifyState, str] = {
    _VerifyState.PENDING: "pending",
    _VerifyState.CLEAN: "clean",
    _VerifyState.MISMATCH: "mismatch",
    _VerifyState.FAILED: "failed",
}

_STR_TO_VERIFY_STATE: dict[str, _VerifyState] = {v: k for k, v in _VERIFY_STATE_STR.items()}

# ---------------------------------------------------------------------------
# DerivedState — internal enum (0 = UNKNOWN reserved); API use strings
# ---------------------------------------------------------------------------


class _DerivedState(IntEnum):
  UNKNOWN = 0
  IN_FLIGHT = 1
  AWAITING_APPROVAL = 2
  NEEDS_AMENDMENT = 3
  VERIFY_FAILED = 4
  APPROVED = 5
  APPROVED_AWAITING_CLEAN = 6
  APPROVED_VERIFY_FAILED = 7
  SUPERSEDED = 8
  ABANDONED = 9


_DERIVED_STATE_STR: dict[_DerivedState, str] = {
    _DerivedState.IN_FLIGHT: "in flight",
    _DerivedState.AWAITING_APPROVAL: "awaiting approval",
    _DerivedState.NEEDS_AMENDMENT: "needs amendment",
    _DerivedState.VERIFY_FAILED: "verify failed",
    _DerivedState.APPROVED: "approved",
    _DerivedState.APPROVED_AWAITING_CLEAN: "approved · awaiting clean verify",
    _DerivedState.APPROVED_VERIFY_FAILED: "approved · verify failed",
    _DerivedState.SUPERSEDED: "superseded",
    _DerivedState.ABANDONED: "abandoned",
}


def _derive_state(
    closed: Optional[dict],
    takeoff: Optional[dict],
    latest_verify_state: str,
) -> _DerivedState:
  """Pure function of (closed, takeoff, latest verify_state) → _DerivedState. Fail-loud."""
  if closed is not None:
    close_as = closed.get("as")
    if close_as == "superseded":
      return _DerivedState.SUPERSEDED
    if close_as == "abandoned":
      return _DerivedState.ABANDONED
    raise ValueError(f"unknown closed.as: {close_as!r}")
  if takeoff is None:
    if latest_verify_state == "pending":
      return _DerivedState.IN_FLIGHT
    if latest_verify_state == "clean":
      return _DerivedState.AWAITING_APPROVAL
    if latest_verify_state == "mismatch":
      return _DerivedState.NEEDS_AMENDMENT
    if latest_verify_state == "failed":
      return _DerivedState.VERIFY_FAILED
    raise ValueError(f"unknown verify_state: {latest_verify_state!r}")
  if latest_verify_state == "clean":
    return _DerivedState.APPROVED
  if latest_verify_state == "pending":
    return _DerivedState.APPROVED_AWAITING_CLEAN
  if latest_verify_state == "failed":
    return _DerivedState.APPROVED_VERIFY_FAILED
  if latest_verify_state == "mismatch":
    raise RuntimeError("unreachable: takeoff + mismatch; mismatch flip clears takeoff and approve rejects mismatch")
  raise ValueError(f"unknown verify_state: {latest_verify_state!r}")


def derive_state_str(plan: dict) -> str:
  """Return the derived-state display string for a plan dict (registry shape)."""
  versions = plan.get("versions") or []
  if not versions:
    raise ValueError(f"plan {plan.get('id')} has no versions")
  latest = versions[-1]
  return _DERIVED_STATE_STR[_derive_state(plan.get("closed"), plan.get("takeoff"), latest["verify_state"])]


def map_verify_outcome(thread_status: ThreadStatus, report_text: str) -> _VerifyState:
  """Pure function mapping (thread status, final report text) → _VerifyState. Fail-loud."""
  if thread_status == ThreadStatus.COMPLETED:
    if not report_text.strip():
      raise RuntimeError("completed verify thread has empty final report")
    final_line = report_text.splitlines()[-1]
    if not VERIFY_RESULT_TRAILER_RE.fullmatch(final_line):
      raise RuntimeError(f"completed verify thread has malformed trailer: {final_line!r}")
    if final_line == "RESULT: clean":
      return _VerifyState.CLEAN
    return _VerifyState.MISMATCH
  if thread_status in (ThreadStatus.FAILED, ThreadStatus.CANCELLED):
    return _VerifyState.FAILED
  raise ValueError(f"unexpected thread status for verify outcome mapping: {thread_status!r}")


async def _resolve_verify_state(
    session_id: str,
    thread_id: str,
    thread_mgr: ThreadManager,
) -> _VerifyState:
  """Read thread status; return PENDING for idle/running, mapped state for terminal."""
  thread = await thread_mgr.get_thread(session_id, thread_id)
  if thread is None:
    raise ValueError(f"thread {thread_id!r} not found in session {session_id!r}")
  if thread.status in (ThreadStatus.IDLE, ThreadStatus.RUNNING):
    return _VerifyState.PENDING
  report = await read_verify_final_report(session_id, thread_id, thread_mgr)
  return map_verify_outcome(thread.status, report)


def _utc_now_iso() -> str:
  return utc_now().isoformat()


# ---------------------------------------------------------------------------
# Plan registry manager
# ---------------------------------------------------------------------------


class PlanRegistryManager:
  """Per-session plan registry: lineage state, version mutations, verify-outcome flips."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager, thread_mgr: ThreadManager):
    self._cfg = cfg
    self._session_mgr = session_mgr
    self._thread_mgr = thread_mgr
    self._locks: dict[str, asyncio.Lock] = {}

  # -- locking ------------------------------------------------------------

  def _lock_for(self, session_id: str) -> asyncio.Lock:
    lock = self._locks.get(session_id)
    if lock is None:
      lock = asyncio.Lock()
      self._locks[session_id] = lock
    return lock

  # -- persistence --------------------------------------------------------

  def _plans_path(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id / "plans.json"

  async def _load(self, session_id: str) -> dict:
    path = self._plans_path(session_id)
    if not path.exists():
      return {"plans": []}
    raw = await asyncio.to_thread(path.read_text, "utf-8")
    return json.loads(raw)

  async def _save(self, session_id: str, data: dict) -> None:
    path = self._plans_path(session_id)
    content = json.dumps(data, indent=2)
    await asyncio.to_thread(self._atomic_write, path, content)

  @staticmethod
  def _atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(content, encoding="utf-8")
    os.replace(tmp, path)

  async def _broadcast(self, session_id: str, plan_id: int) -> None:
    await self._session_mgr.broadcast_only(
        session_id, {
            "type": "plan_updated",
            "session_id": session_id,
            "plan_id": plan_id,
        })

  # -- helpers ------------------------------------------------------------

  def _validate_file_in_session_dir(self, session_id: str, file: str) -> None:
    session_dir = (self._cfg.sessions_dir / session_id).resolve()
    candidate = (session_dir / file).resolve()
    try:
      candidate.relative_to(session_dir)
    except ValueError as e:
      raise ValueError(f"file {file!r} resolves outside the session directory") from e
    if not candidate.exists():
      raise ValueError(f"file {file!r} not found inside the session directory")

  def _find_binding_by_file(self, data: dict, file: str) -> Optional[tuple[int, int]]:
    for plan in data["plans"]:
      for ver in plan["versions"]:
        if ver["file"] == file:
          return plan["id"], ver["v"]
    return None

  def _find_binding_by_thread(self, data: dict, verify_thread: str) -> Optional[tuple[int, int]]:
    for plan in data["plans"]:
      for ver in plan["versions"]:
        if ver["verify_thread"] == verify_thread:
          return plan["id"], ver["v"]
    return None

  async def _assert_thread_is_verify(self, session_id: str, verify_thread: str) -> ThreadMetadata:
    thread = await self._thread_mgr.get_thread(session_id, verify_thread)
    if thread is None:
      raise ValueError(f"verify thread {verify_thread!r} not found in session {session_id!r}")
    if thread.task_type != TaskType.VERIFY:
      raise ValueError(f"thread {verify_thread!r} is not a verify thread (task_type={thread.task_type!r})")
    return thread

  def _get_plan(self, data: dict, plan_id: int) -> Optional[dict]:
    for plan in data["plans"]:
      if plan["id"] == plan_id:
        return plan
    return None

  # -- verbs --------------------------------------------------------------

  async def present(
      self,
      session_id: str,
      file: str,
      verify_thread: str,
      title: str,
      base: Optional[dict] = None,
  ) -> dict:
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      await self._assert_thread_is_verify(session_id, verify_thread)
      self._validate_file_in_session_dir(session_id, file)
      existing_file = self._find_binding_by_file(data, file)
      if existing_file is not None:
        raise ValueError(f"file {file!r} already bound to plan {existing_file[0]} v{existing_file[1]}")
      existing_thread = self._find_binding_by_thread(data, verify_thread)
      if existing_thread is not None:
        raise ValueError(
            f"verify thread {verify_thread!r} already bound to plan {existing_thread[0]} v{existing_thread[1]}")
      next_id = max((p["id"] for p in data["plans"]), default=0) + 1
      verify_state = await _resolve_verify_state(session_id, verify_thread, self._thread_mgr)
      plan = {
          "id": next_id,
          "title": title,
          "versions":
              [
                  {
                      "v": 1,
                      "file": file,
                      "created_at": _utc_now_iso(),
                      "trigger": "initial",
                      "verify_thread": verify_thread,
                      "verify_state": _VERIFY_STATE_STR[verify_state],
                      "base": base,
                  }
              ],
          "takeoff": None,
          "closed": None,
      }
      data["plans"].append(plan)
      await self._save(session_id, data)
    await self._broadcast(session_id, next_id)
    return {"plan": next_id, "v": 1, "state": derive_state_str(plan)}

  async def amend(
      self,
      session_id: str,
      file: str,
      verify_thread: str,
      plan_id: Optional[int] = None,
      trigger: str = "feedback",
      base: Optional[dict] = None,
  ) -> dict:
    if trigger not in ("initial", "auto_amend", "feedback"):
      raise ValueError(f"trigger must be one of initial|auto_amend|feedback, got {trigger!r}")
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      await self._assert_thread_is_verify(session_id, verify_thread)
      self._validate_file_in_session_dir(session_id, file)
      existing_file = self._find_binding_by_file(data, file)
      if existing_file is not None:
        raise ValueError(f"file {file!r} already bound to plan {existing_file[0]} v{existing_file[1]}")
      existing_thread = self._find_binding_by_thread(data, verify_thread)
      if existing_thread is not None:
        raise ValueError(
            f"verify thread {verify_thread!r} already bound to plan {existing_thread[0]} v{existing_thread[1]}")
      plan = self._resolve_target_plan_for_amend(data, plan_id)
      new_v = max(ver["v"] for ver in plan["versions"]) + 1
      verify_state = await _resolve_verify_state(session_id, verify_thread, self._thread_mgr)
      plan["versions"].append(
          {
              "v": new_v,
              "file": file,
              "created_at": _utc_now_iso(),
              "trigger": trigger,
              "verify_thread": verify_thread,
              "verify_state": _VERIFY_STATE_STR[verify_state],
              "base": base,
          })
      plan["takeoff"] = None
      await self._save(session_id, data)
    await self._broadcast(session_id, plan["id"])
    return {"plan": plan["id"], "v": new_v, "state": derive_state_str(plan)}

  def _resolve_target_plan_for_amend(self, data: dict, plan_id: Optional[int]) -> dict:
    if plan_id is not None:
      plan = self._get_plan(data, plan_id)
      if plan is None:
        raise ValueError(f"plan {plan_id} not found in session")
      if plan.get("closed") is not None:
        raise ValueError(f"plan {plan_id} is closed ({plan['closed']['as']!r})")
      return plan
    open_candidates = [p for p in data["plans"] if p.get("closed") is None and p.get("takeoff") is None]
    if not open_candidates:
      raise ValueError("no open lineage to amend; pass --plan to amend an approved one")
    if len(open_candidates) > 1:
      ids = ", ".join(str(p["id"]) for p in open_candidates)
      raise ValueError(f"amend requires --plan (multiple open lineages: {ids})")
    return open_candidates[0]

  async def approve(self, session_id: str, plan_id: Optional[int] = None) -> dict:
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      plan = self._resolve_target_plan_for_approve(data, plan_id)
      latest = plan["versions"][-1]
      verify_state = latest["verify_state"]
      if verify_state == "mismatch":
        raise ValueError(f"plan {plan['id']} latest version has mismatch verify_state; amend first")
      if verify_state == "failed":
        raise ValueError(f"plan {plan['id']} latest version has failed verify_state; reverify first")
      plan["takeoff"] = {"v": latest["v"], "at": _utc_now_iso()}
      await self._save(session_id, data)
    await self._broadcast(session_id, plan["id"])
    return {"plan": plan["id"], "v": latest["v"], "state": derive_state_str(plan)}

  def _resolve_target_plan_for_approve(self, data: dict, plan_id: Optional[int]) -> dict:
    if plan_id is not None:
      plan = self._get_plan(data, plan_id)
      if plan is None:
        raise ValueError(f"plan {plan_id} not found in session")
      if plan.get("closed") is not None:
        raise ValueError(f"plan {plan_id} is closed ({plan['closed']['as']!r})")
      if plan.get("takeoff") is not None:
        raise ValueError(f"plan {plan_id} is already approved")
      return plan
    candidates = [p for p in data["plans"] if p.get("closed") is None and p.get("takeoff") is None]
    if not candidates:
      raise ValueError("no plan available for approval")
    if len(candidates) > 1:
      ids = ", ".join(str(p["id"]) for p in candidates)
      raise ValueError(f"approve requires --plan (multiple candidates: {ids})")
    return candidates[0]

  async def reverify(
      self,
      session_id: str,
      verify_thread: str,
      plan_id: Optional[int] = None,
  ) -> dict:
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      await self._assert_thread_is_verify(session_id, verify_thread)
      existing_thread = self._find_binding_by_thread(data, verify_thread)
      if existing_thread is not None:
        raise ValueError(
            f"verify thread {verify_thread!r} already bound to plan {existing_thread[0]} v{existing_thread[1]}")
      plan = self._resolve_target_plan_for_reverify(data, plan_id)
      latest = plan["versions"][-1]
      if latest["verify_state"] != "failed":
        raise ValueError(
            f"plan {plan['id']} latest version verify_state is {latest['verify_state']!r}; reverify only from failed")
      verify_state = await _resolve_verify_state(session_id, verify_thread, self._thread_mgr)
      latest["verify_thread"] = verify_thread
      latest["verify_state"] = _VERIFY_STATE_STR[verify_state]
      if verify_state == _VerifyState.MISMATCH and plan.get("takeoff") is not None:
        plan["takeoff"] = None
      await self._save(session_id, data)
    await self._broadcast(session_id, plan["id"])
    return {"plan": plan["id"], "v": latest["v"], "state": derive_state_str(plan)}

  def _resolve_target_plan_for_reverify(self, data: dict, plan_id: Optional[int]) -> dict:
    if plan_id is not None:
      plan = self._get_plan(data, plan_id)
      if plan is None:
        raise ValueError(f"plan {plan_id} not found in session")
      if plan.get("closed") is not None:
        raise ValueError(f"plan {plan_id} is closed ({plan['closed']['as']!r})")
      return plan
    open_candidates = [p for p in data["plans"] if p.get("closed") is None and p.get("takeoff") is None]
    if not open_candidates:
      raise ValueError("no open lineage to reverify; pass --plan to target an approved one")
    if len(open_candidates) > 1:
      ids = ", ".join(str(p["id"]) for p in open_candidates)
      raise ValueError(f"reverify requires --plan (multiple open lineages: {ids})")
    return open_candidates[0]

  async def close(self, session_id: str, plan_id: int, close_as: str) -> dict:
    if close_as not in ("superseded", "abandoned"):
      raise ValueError(f"--as must be superseded|abandoned, got {close_as!r}")
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      plan = self._get_plan(data, plan_id)
      if plan is None:
        raise ValueError(f"plan {plan_id} not found in session")
      if plan.get("closed") is not None:
        raise ValueError(f"plan {plan_id} is already closed ({plan['closed']['as']!r})")
      plan["closed"] = {"as": close_as, "at": _utc_now_iso()}
      await self._save(session_id, data)
    await self._broadcast(session_id, plan_id)
    return {"plan": plan_id, "state": derive_state_str(plan)}

  async def list_plans(self, session_id: str) -> dict:
    async with self._lock_for(session_id):
      data = await self._load(session_id)
    return self._with_derived_states(data)

  @staticmethod
  def _with_derived_states(data: dict) -> dict:
    plans = []
    for plan in data.get("plans", []):
      enriched = {**plan, "state": derive_state_str(plan)}
      plans.append(enriched)
    return {"plans": plans}

  # -- verify completion hook ---------------------------------------------

  async def flip_on_verify_completion(self, session_id: str, thread_id: str) -> None:
    """Look up the registered version by thread id and write the mapped verify_state.

    A completing verify thread with no registered version is a no-op.
    """
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      binding = self._find_binding_by_thread(data, thread_id)
      if binding is None:
        return
      plan_id, v = binding
      plan = self._get_plan(data, plan_id)
      if plan is None:
        return
      version = next((ver for ver in plan["versions"] if ver["v"] == v), None)
      if version is None:
        return
      thread = await self._thread_mgr.get_thread(session_id, thread_id)
      if thread is None:
        return
      if thread.status not in (ThreadStatus.COMPLETED, ThreadStatus.FAILED, ThreadStatus.CANCELLED):
        return
      report = await read_verify_final_report(session_id, thread_id, self._thread_mgr)
      new_state = map_verify_outcome(thread.status, report)
      version["verify_state"] = _VERIFY_STATE_STR[new_state]
      if new_state == _VerifyState.MISMATCH and plan.get("takeoff") is not None:
        plan["takeoff"] = None
      await self._save(session_id, data)
    await self._broadcast(session_id, plan_id)

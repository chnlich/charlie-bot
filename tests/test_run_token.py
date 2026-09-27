"""Run-token identity: signing, fail-closed middleware rules, and CLI token priority."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta

import pytest
from conftest import (
    _ok_asgi_downstream,
    asgi_downstream_called,
    run_through_asgi_middleware,
    stub_credentials,
)

from src.api.auth import AuthMiddleware
from src.core.run_token import (
    RunTokenClaims,
    RunTokenError,
    sign_run_token,
    verify_run_token,
)
from src.core.takeoff_gate import check_takeoff_gate_for_task


def _scope(headers: dict[str, str] | None = None, cookies: dict[str, str] | None = None) -> dict:
  raw_headers: list[tuple[bytes, bytes]] = []
  for name, value in (headers or {}).items():
    raw_headers.append((name.lower().encode(), value.encode()))
  if cookies:
    raw_headers.append((b"cookie", "; ".join(f"{k}={v}" for k, v in cookies.items()).encode()))
  return {"type": "http", "method": "GET", "path": "/api/sessions", "headers": raw_headers, "query_string": b""}


def _status(sent: list[dict]) -> int:
  return next(m for m in sent if m["type"] == "http.response.start")["status"]


CLAIMS = RunTokenClaims(session_id="s-1", run_id="r-1", agent="worker-alpha")

# ---------------------------------------------------------------------------
# Token mechanics
# ---------------------------------------------------------------------------


def test_sign_and_verify_round_trip() -> None:
  token = sign_run_token(CLAIMS, "op-secret")
  assert verify_run_token(token, "op-secret") == CLAIMS
  # Every claim is bound: another session/run/agent cannot reuse it.
  with pytest.raises(RunTokenError):
    verify_run_token(token, "other-key")
  payload_b64, sig = token.split(".", 1)
  import base64
  payload = json.loads(base64.urlsafe_b64decode(payload_b64 + "=" * (-len(payload_b64) % 4)))
  payload["run_id"] = "r-2"
  forged_payload = base64.urlsafe_b64encode(json.dumps(payload).encode()).rstrip(b"=").decode()
  with pytest.raises(RunTokenError):
    verify_run_token(f"{forged_payload}.{sig}", "op-secret")


def test_missing_signing_key_is_an_explicit_error() -> None:
  with pytest.raises(RunTokenError, match="without a configured"):
    sign_run_token(CLAIMS, "")
  with pytest.raises(RunTokenError, match="no signing key"):
    verify_run_token("a.b", "")


# ---------------------------------------------------------------------------
# Middleware: a presented bearer is verified or rejected, never cookie-fallback
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_invalid_run_bearer_never_falls_back_to_operator_cookie() -> None:
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  mw = AuthMiddleware(app=_ok_asgi_downstream)
  sent = await run_through_asgi_middleware(
      mw, _scope(headers={"Authorization": "Bearer forged"}, cookies={"charliebot_access_key": "op-secret"}))
  assert _status(sent) == 401
  assert not asgi_downstream_called()


# ---------------------------------------------------------------------------
# CLI: a run token in the environment is the only credential sent

# ---------------------------------------------------------------------------
# The v2 task gate: nearest real user instruction, unchanged time rules
# ---------------------------------------------------------------------------


def _gate_env(
    events_by_session: dict[str, list[dict]], parents: dict[str, str | None], profiles: dict[str, str],
    states: dict[str, str]):

  def load_events(session_id: str) -> list[dict]:
    return events_by_session.get(session_id, [])

  def meta_of(session_id: str) -> tuple[str | None, str | None]:
    return parents.get(session_id), profiles.get(session_id)

  def state_of(session_id: str) -> str:
    return states.get(session_id, "open")

  return load_events, meta_of, state_of


def _user(content: str, at: datetime) -> dict:
  return {"id": content, "type": "user", "timestamp": at.isoformat(), "actor": "user", "content": content}


NOW = datetime.now(UTC)


def test_task_gate_borrows_from_the_nearest_user_ancestor() -> None:
  # root holds a valid take off; child has nothing; grandchild only agent chatter.
  events = {
      "root": [_user("plan the thing", NOW - timedelta(hours=1)),
               _user("take off", NOW - timedelta(minutes=5))],
      "child": [{
          "type": "agent_message",
          "content": "progress",
          "timestamp": NOW.isoformat()
      }],
  }
  parents = {"root": None, "child": "root", "grandchild": "child"}
  profiles = {"root": "manager", "child": "manager", "grandchild": "manager"}
  load_events, meta_of, state_of = _gate_env(events, parents, profiles, {})
  assert check_takeoff_gate_for_task(
      "grandchild", load_events=load_events, task_meta_of=meta_of, task_state_of=state_of) == "root"

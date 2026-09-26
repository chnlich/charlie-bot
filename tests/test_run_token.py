"""Run-token identity: signing, fail-closed middleware rules, and CLI token priority."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from starlette.requests import Request
from conftest import (
    _ok_asgi_downstream,
    asgi_downstream_called,
    patched_cli_post,
    run_through_asgi_middleware,
    stub_credentials,
)

from src.api.auth import AuthMiddleware
from src.api.deps import require_caller
from src.cli.common import internal_api_auth_headers
from src.cli.session import main as session_cli_main
from src.core.constants import CALLER_SESSION_HEADER, SESSION_ID_ENV_VAR
from src.core.models import RunRecord
from src.core.run_token import (
    RUN_TOKEN_ENV,
    CallerIdentity,
    RunTokenClaims,
    RunTokenError,
    load_run_token,
    sign_run_token,
    verify_run_token,
)
from src.core.takeoff_gate import DelegationBlockedError, check_takeoff_gate_for_task


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


@pytest.mark.asyncio
async def test_valid_run_bearer_passes_the_middleware() -> None:
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  token = sign_run_token(CLAIMS, "op-secret")
  mw = AuthMiddleware(app=_ok_asgi_downstream)
  sent = await run_through_asgi_middleware(mw, _scope(headers={"Authorization": f"Bearer {token}"}))
  assert _status(sent) == 200
  assert asgi_downstream_called()


@pytest.mark.asyncio
async def test_operator_bearer_and_cookie_rules_are_unchanged() -> None:
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  mw = AuthMiddleware(app=_ok_asgi_downstream)
  sent = await run_through_asgi_middleware(mw, _scope(headers={"Authorization": "Bearer op-secret"}))
  assert _status(sent) == 200
  sent = await run_through_asgi_middleware(mw, _scope(cookies={"charliebot_access_key": "op-secret"}))
  assert _status(sent) == 200
  sent = await run_through_asgi_middleware(mw, _scope())
  assert _status(sent) == 401


# ---------------------------------------------------------------------------
# CLI: a run token in the environment is the only credential sent
# ---------------------------------------------------------------------------


def test_cli_run_token_priority(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv(RUN_TOKEN_ENV, "run-token-xyz")
  assert internal_api_auth_headers() == {"Authorization": "Bearer run-token-xyz"}
  # The CLI session verbs ride the same header function: no operator fallback.
  from conftest import make_json_response
  cfg = type("Cfg", (), {})()
  cfg.server_base_url = "http://localhost:9"
  with patched_cli_post(cfg, ["session", "retry", "s-1", "--run", "r-1", "--request-id", "rr"],
                        return_value=make_json_response({"session_id": "s-1", "run_id": "r-1"})) as post_mock:
    session_cli_main()
  assert post_mock.call_args[1]["headers"] == {"Authorization": "Bearer run-token-xyz"}


def test_cli_without_run_token_uses_the_operator_key(monkeypatch: pytest.MonkeyPatch) -> None:
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.delenv(RUN_TOKEN_ENV, raising=False)
  assert internal_api_auth_headers() == {"Authorization": "Bearer op-secret"}
  monkeypatch.setenv(RUN_TOKEN_ENV, "")
  assert internal_api_auth_headers() == {"Authorization": "Bearer op-secret"}
  monkeypatch.delenv(RUN_TOKEN_ENV, raising=False)
  stub_credentials({"charliebot": {"access_key": ""}})
  assert internal_api_auth_headers() == {}


def test_internal_api_auth_headers_carries_the_session_header(monkeypatch: pytest.MonkeyPatch) -> None:
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv(RUN_TOKEN_ENV, "run-token-xyz")
  monkeypatch.setenv(SESSION_ID_ENV_VAR, "sess-1")
  # Run-token branch: the bearer dict plus the caller-session header.
  assert internal_api_auth_headers() == {
      "Authorization": "Bearer run-token-xyz",
      CALLER_SESSION_HEADER: "sess-1",
  }
  # Operator-key branch: the same header rides the access-key bearer.
  monkeypatch.delenv(RUN_TOKEN_ENV, raising=False)
  assert internal_api_auth_headers() == {
      "Authorization": "Bearer op-secret",
      CALLER_SESSION_HEADER: "sess-1",
  }
  # Empty or absent variable: no such header.
  monkeypatch.setenv(SESSION_ID_ENV_VAR, "")
  assert internal_api_auth_headers() == {"Authorization": "Bearer op-secret"}
  monkeypatch.delenv(SESSION_ID_ENV_VAR, raising=False)
  assert internal_api_auth_headers() == {"Authorization": "Bearer op-secret"}


def test_caller_identity_equality_covers_session_id() -> None:
  assert CallerIdentity(kind="operator") == CallerIdentity(kind="operator")
  assert CallerIdentity(kind="operator", session_id="a") != CallerIdentity(kind="operator", session_id="b")
  assert CallerIdentity(kind="operator", session_id="a") != CallerIdentity(kind="operator")
  assert CallerIdentity(kind="agent", claims=CLAIMS) == CallerIdentity(kind="agent", claims=CLAIMS)
  # An agent's session always comes from its verified token claims.
  assert CallerIdentity(kind="agent", claims=CLAIMS).session_id == CLAIMS.session_id


class _FakeRunStore:
  """Just the two RunStore members require_caller reads, answering from one active run."""

  def __init__(self, run: RunRecord) -> None:
    self._run = run

  async def get_run(self, session_id: str, run_id: str) -> RunRecord | None:
    return self._run

  def load_events_sync(self, session_id: str) -> list[dict]:
    return []


@pytest.mark.asyncio
async def test_require_caller_carries_the_session_on_the_caller_identity() -> None:
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  token = sign_run_token(CLAIMS, "op-secret")
  run_store = _FakeRunStore(RunRecord(id="r-1", session_id="s-1", pid=11, pid_start="22"))

  # Operator bearer + header: the header value becomes the identity's session.
  caller = await require_caller(
      Request(_scope(headers={"Authorization": "Bearer op-secret", CALLER_SESSION_HEADER: "sess-1"})),
      run_store=run_store)
  assert caller.is_operator and caller.session_id == "sess-1"

  # A valid run token ignores the header: the session is the token's, never the header's.
  caller = await require_caller(
      Request(_scope(headers={"Authorization": f"Bearer {token}", CALLER_SESSION_HEADER: "spoofed"})),
      run_store=run_store)
  assert not caller.is_operator and caller.session_id == "s-1"

  # No header: the operator identity's session is None.
  caller = await require_caller(
      Request(_scope(headers={"Authorization": "Bearer op-secret"})),
      run_store=run_store)
  assert caller.is_operator and caller.session_id is None


def test_load_run_token_reads_env(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.delenv(RUN_TOKEN_ENV, raising=False)
  assert load_run_token() is None
  monkeypatch.setenv(RUN_TOKEN_ENV, " tok ")
  assert load_run_token() == "tok"


def test_caller_identity_shape() -> None:
  operator = CallerIdentity(kind="operator")
  assert operator.is_operator
  agent = CallerIdentity(kind="agent", claims=CLAIMS)
  assert not agent.is_operator and agent.agent_session_id == "s-1"
  with pytest.raises(RuntimeError):
    _ = operator.agent_session_id


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


def test_task_gate_local_instruction_blocks_higher_borrow() -> None:
  # A more recent local real user message without take off stops the walk.
  events = {
      "root": [_user("take off", NOW - timedelta(minutes=5))],
      "child": [_user("hold on", NOW - timedelta(minutes=1))],
  }
  parents = {"root": None, "child": "root", "grandchild": "child"}
  profiles = {"root": "manager", "child": "manager", "grandchild": "manager"}
  load_events, meta_of, state_of = _gate_env(events, parents, profiles, {})
  with pytest.raises(DelegationBlockedError, match="task child"):
    check_takeoff_gate_for_task("grandchild", load_events=load_events, task_meta_of=meta_of, task_state_of=state_of)


def test_task_gate_requires_open_ancestors_and_manager_caller() -> None:
  events = {"root": [_user("take off", NOW)]}
  parents = {"root": None, "child": "root"}
  profiles = {"root": "manager", "child": "manager", "worker": "worker"}
  load_events, meta_of, state_of = _gate_env(events, parents, profiles, {"root": "open", "child": "completed"})
  with pytest.raises(DelegationBlockedError, match="task child is completed"):
    check_takeoff_gate_for_task("child", load_events=load_events, task_meta_of=meta_of, task_state_of=state_of)
  with pytest.raises(DelegationBlockedError, match="not a manager"):
    check_takeoff_gate_for_task("worker", load_events=load_events, task_meta_of=meta_of, task_state_of=state_of)


def test_task_gate_pre_takeoff_window_matches_the_legacy_12_hours() -> None:
  issued = NOW - timedelta(hours=11)
  events = {"root": [_user("pre take off", issued), _user("normal follow-up", issued + timedelta(minutes=1))]}
  load_events, meta_of, state_of = _gate_env(events, {"root": None}, {"root": "manager"}, {})
  assert check_takeoff_gate_for_task(
      "root", load_events=load_events, task_meta_of=meta_of, task_state_of=state_of, now=NOW) == "root"
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate_for_task(
        "root",
        load_events=load_events,
        task_meta_of=meta_of,
        task_state_of=state_of,
        now=issued + timedelta(hours=12, seconds=1))


def test_task_gate_never_borrows_past_the_root() -> None:
  load_events, meta_of, state_of = _gate_env({}, {"root": None}, {"root": "manager"}, {})
  with pytest.raises(DelegationBlockedError, match="ancestor chain"):
    check_takeoff_gate_for_task("root", load_events=load_events, task_meta_of=meta_of, task_state_of=state_of)

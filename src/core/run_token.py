"""Signed run tokens: the agent caller identity for task-tree structural APIs.

The server signs a CHARLIEBOT_RUN_TOKEN for one Run with the operator access
key (``credentials.yaml`` ``charliebot.access_key``). The token's payload binds
``session_id``, ``run_id`` and the agent identity; validity is the HMAC
signature AND the referenced Run being active (registered and not yet
terminal), checked together at the caller-identity dependency.

Hard rules this module makes enforceable:

- A presented run bearer is verified against the signing key; it never falls
  back to the operator access key or the browser cookie, and an invalid run
  bearer fails closed even when a valid operator cookie rides the same request.
- Run-token use with a missing signing key is an explicit error, never a
  silent pass.
- The operator access key keeps working exactly as before for operator
  callers (browser and operator CLI).
"""

import base64
import hashlib
import hmac
import json
import os
from dataclasses import dataclass
from typing import Callable, Literal

RUN_TOKEN_ENV = "CHARLIEBOT_RUN_TOKEN"

# Credentials lookup: kept as an injectable seam so tests can stub credentials
# without the env-file round-trip. Live callers resolve through
# src.core.config.get_credentials (lazy import — the CLI import floor rule).
_credentials_getter: Callable[[], dict] | None = None


def _credentials() -> dict:
  if _credentials_getter is not None:
    return _credentials_getter()
  from src.core.config import get_credentials
  return get_credentials()


def set_credentials_getter(getter: Callable[[], dict] | None) -> None:
  """Swap the credentials source (tests); None restores the live reader."""
  global _credentials_getter
  _credentials_getter = getter


def operator_signing_key() -> str:
  """The configured operator access key — the only run-token signing key."""
  return str(_credentials().get("charliebot", "access_key") or "")


class RunTokenError(Exception):
  """Any run-token verification failure (bad shape, bad signature, bad payload)."""


@dataclass(frozen=True)
class RunTokenClaims:
  """The identity one run token binds: session, run, and the agent's name."""
  session_id: str
  run_id: str
  agent: str


@dataclass(frozen=True)
class CallerIdentity:
  """The verified caller of a structural request: operator, or an agent bound to one Run."""
  kind: Literal["operator", "agent"]
  # Present only for kind == "agent".
  claims: RunTokenClaims | None = None

  @property
  def is_operator(self) -> bool:
    return self.kind == "operator"

  @property
  def agent_session_id(self) -> str:
    if self.claims is None:
      raise RuntimeError("agent_session_id is only defined for agent callers")
    return self.claims.session_id


def _b64url_encode(raw: bytes) -> str:
  return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def _b64url_decode(text: str) -> bytes:
  padding = "=" * (-len(text) % 4)
  return base64.urlsafe_b64decode(text + padding)


def sign_run_token(claims: RunTokenClaims, signing_key: str) -> str:
  """Sign *claims* with the operator access key; the wire form is ``payload.signature``."""
  if not signing_key:
    raise RunTokenError("cannot sign a run token without a configured charliebot.access_key")
  payload = {
      "session_id": claims.session_id,
      "run_id": claims.run_id,
      "agent": claims.agent,
  }
  payload_b64 = _b64url_encode(json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8"))
  signature = hmac.new(signing_key.encode("utf-8"), payload_b64.encode("ascii"), hashlib.sha256).digest()
  return f"{payload_b64}.{_b64url_encode(signature)}"


def verify_run_token(token: str, signing_key: str) -> RunTokenClaims:
  """Verify signature and shape; returns the claims or raises :class:`RunTokenError`."""
  if not signing_key:
    raise RunTokenError("run token presented but no signing key is configured")
  try:
    payload_b64, signature_b64 = token.split(".", 1)
  except ValueError as e:
    raise RunTokenError("malformed run token") from e
  expected = hmac.new(signing_key.encode("utf-8"), payload_b64.encode("ascii"), hashlib.sha256).digest()
  try:
    presented = _b64url_decode(signature_b64)
  except (ValueError, TypeError) as e:
    raise RunTokenError("malformed run token signature") from e
  if not hmac.compare_digest(presented, expected):
    raise RunTokenError("run token signature mismatch")
  try:
    payload = json.loads(_b64url_decode(payload_b64))
  except (ValueError, TypeError) as e:
    raise RunTokenError("malformed run token payload") from e
  if not isinstance(payload, dict):
    raise RunTokenError("run token payload must be an object")
  session_id = payload.get("session_id")
  run_id = payload.get("run_id")
  agent = payload.get("agent")
  if not (isinstance(session_id, str) and session_id and isinstance(run_id, str) and run_id and
          isinstance(agent, str) and agent):
    raise RunTokenError("run token payload must bind session_id, run_id and agent")
  return RunTokenClaims(session_id=session_id, run_id=run_id, agent=agent)


def bearer_from_authorization(header_value: str | None) -> str:
  """The bearer credential of an ``Authorization`` header value, or ""."""
  if header_value and header_value.startswith("Bearer "):
    return header_value[7:]
  return ""


def load_run_token() -> str | None:
  """The process environment's run token, or None when absent."""
  value = os.environ.get(RUN_TOKEN_ENV, "").strip()
  return value or None

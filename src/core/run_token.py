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

``b64url_encode``/``b64url_decode`` are the single home of the unpadded-base64url
wire codec; the page cursors in ``src/core/runs.py`` and ``src/core/task_sessions.py``
share it.
"""

import base64
import hashlib
import hmac
import json
import os
from typing import Literal

RUN_TOKEN_ENV = "CHARLIEBOT_RUN_TOKEN"


class RunTokenError(Exception):
  """Any run-token verification failure (bad shape, bad signature, bad payload)."""


class RunTokenClaims:
  """The identity one run token binds: session, run, and the agent's name.

  A plain class, not a dataclass: the CLI verbs resolve the run token in fresh
  processes, and the ``dataclasses`` import pulls ``inspect`` (~9-11 ms of the
  M92/M97/M102 verb walls) for machinery no consumer calls.
  """

  __slots__ = ("agent", "run_id", "session_id")

  def __init__(self, session_id: str, run_id: str, agent: str) -> None:
    self.session_id = session_id
    self.run_id = run_id
    self.agent = agent

  def __eq__(self, other: object) -> bool:
    if not isinstance(other, RunTokenClaims):
      return NotImplemented
    return (self.session_id, self.run_id, self.agent) == (other.session_id, other.run_id, other.agent)

  def __hash__(self) -> int:
    return hash((self.session_id, self.run_id, self.agent))


class CallerIdentity:
  """The verified caller of a structural request: operator, or an agent bound to one Run."""

  __slots__ = ("claims", "kind", "session_id")

  def __init__(
      self,
      kind: Literal["operator", "agent"],
      claims: RunTokenClaims | None = None,
      session_id: str | None = None) -> None:
    self.kind = kind
    # Present only for kind == "agent".
    self.claims = claims
    # The calling session id: token-verified for kind == "agent", the
    # caller-session header for kind == "operator" (None when absent).
    self.session_id = session_id
    if kind == "agent" and claims is not None:
      self.session_id = claims.session_id

  def __eq__(self, other: object) -> bool:
    if not isinstance(other, CallerIdentity):
      return NotImplemented
    return (self.kind == other.kind and self.claims == other.claims and self.session_id == other.session_id)

  def __hash__(self) -> int:
    return hash((self.kind, self.claims, self.session_id))

  @property
  def is_operator(self) -> bool:
    return self.kind == "operator"

  @property
  def agent_session_id(self) -> str:
    if self.claims is None:
      raise RuntimeError("agent_session_id is only defined for agent callers")
    return self.claims.session_id


def b64url_encode(raw: bytes) -> str:
  """Single home of the unpadded-base64url wire form: this module's token and the page cursors ride it."""
  return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def b64url_decode(text: str) -> bytes:
  """The decode side: re-pads the unpadded form to the length base64 requires."""
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
  payload_b64 = b64url_encode(json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8"))
  signature = hmac.new(signing_key.encode("utf-8"), payload_b64.encode("ascii"), hashlib.sha256).digest()
  return f"{payload_b64}.{b64url_encode(signature)}"


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
    presented = b64url_decode(signature_b64)
  except (ValueError, TypeError) as e:
    raise RunTokenError("malformed run token signature") from e
  if not hmac.compare_digest(presented, expected):
    raise RunTokenError("run token signature mismatch")
  try:
    payload = json.loads(b64url_decode(payload_b64))
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

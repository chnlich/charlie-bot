"""Account relay mechanics shared by the master turn and delegated workers.

A Claude Code process runs on one pool account (src/core/claude_accounts.py).
Both the master turn (src/agents/master_cc_relay.py) and a delegated worker
(src/agents/worker.py) need the same answers while that process runs: which
events mean the account is running out, where the process can stop safely, and
how the run continues on the next account. This module owns those answers so
the two callers differ only in how they persist events and rebuild the process.

During a run every ``rate_limit_event`` updates the account's reading. A
rejection means the process ends on its own and the run relays; a warning at
or above the warning line with the reset more than 45 minutes away arms a
relay at the next safe point, the first tool result after the warning, where
the process is terminated and the transcript ends on a completed tool call. An
exit that names an authentication failure marks the account unhealthy and
relays too.

Relay: the transcript is copied to the account with the most headroom among the
others and Claude Code resumes the same session id with one fixed continuation
prompt. The user sees no account names; those stay in the server log, the
metadata and the usage panel.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

from src.core import claude_accounts
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, ClaudeAccount

# One run relays at most this many times: four accounts rejecting in turn must
# end in a loud error, never a loop.
MAX_RELAYS_PER_TURN = 3

# A warning whose window resets sooner than this is left alone: waiting for the
# reset costs less than a cold start on another account.
RELAY_MIN_RESET_AHEAD = timedelta(minutes=45)

# The prompt the resumed process receives after a mid-run relay. The transcript
# already holds the task and every completed tool call, so re-sending the task
# would make the model redo work.
CONTINUATION_PROMPT = (
    "The previous process was interrupted right after a tool call completed. Continue from "
    "where it stopped; do not redo the steps that are already done.")

AUTH_FAILURE_MARKER = "Failed to authenticate"

# RelayWatch.decision values.
RELAY_REJECTED = "rejected"
RELAY_WARNING = "warning"
LOGIN_FAILED = "login_failed"

_now = claude_accounts._now


def _is_tool_result_event(event: dict) -> bool:
  """A Claude Code ``user`` event whose content carries a tool_result block: the relay safe point."""
  if event.get("type") != ET.USER:
    return False
  message = event.get("message")
  content = message.get("content") if isinstance(message, dict) else None
  return isinstance(content, list) and any(
      isinstance(block, dict) and block.get("type") == ET.TOOL_RESULT for block in content)


def _assistant_texts(event: dict) -> list[str]:
  message = event.get("message")
  content = message.get("content") if isinstance(message, dict) else None
  if not isinstance(content, list):
    return []
  return [block["text"] for block in content if isinstance(block, dict) and isinstance(block.get("text"), str)]


def _reset_far_enough(info: dict, now: datetime) -> bool:
  resets_at = info.get("resetsAt")
  if not isinstance(resets_at, (int, float)) or isinstance(resets_at, bool):
    return True
  return datetime.fromtimestamp(float(resets_at), UTC) - now > RELAY_MIN_RESET_AHEAD


class RelayWatch:
  """Folds one run's events into a relay decision for its account."""

  def __init__(self, label: str, model: str | None, now: datetime | None = None) -> None:
    self.label = label
    self.model = model
    self._now = now
    self.reason: str | None = None
    self.armed = False
    self.auth_failed = False

  def observe(self, event: dict) -> bool:
    """Record the event; True exactly when the run must be terminated now (armed safe point)."""
    event_type = event.get("type")
    if event_type == ET.RATE_LIMIT_EVENT:
      info = event.get("rate_limit_info")
      if not isinstance(info, dict):
        return False
      moment = _now(self._now)
      reading = claude_accounts.observe_rate_limit(self.label, info, moment)
      status = info.get("status")
      if status == "rejected":
        self.reason = RELAY_REJECTED
        self.armed = False
      elif (status == "allowed_warning" and reading is not None and
            reading.utilization >= claude_accounts.WARNING_UTILIZATION and _reset_far_enough(info, moment)):
        self.armed = True
      return False
    if event_type == ET.ASSISTANT and any(AUTH_FAILURE_MARKER in text for text in _assistant_texts(event)):
      self.auth_failed = True
      return False
    if self.armed and self.reason is None and _is_tool_result_event(event):
      self.reason = RELAY_WARNING
      return True
    return False

  def decision(self, exit_code: int, stderr_text: str) -> str | None:
    """What the finished run asks for: a relay reason, a login failure, or None."""
    if exit_code != 0 and (self.auth_failed or AUTH_FAILURE_MARKER in (stderr_text or "")):
      return LOGIN_FAILED
    return self.reason


def pool_exhausted_message(cfg: CharlieBotConfig, now: datetime | None = None) -> str:
  reset = claude_accounts.earliest_reset(cfg, now)
  when = f"earliest reset {reset.astimezone(UTC).strftime('%H:%M')} UTC" if reset else "no reset time known"
  return f"Claude account pool has no available account ({when}); this run did not complete."


def relay_limit_message() -> str:
  return (
      f"Claude account relay limit reached ({MAX_RELAYS_PER_TURN} relays in one run); "
      "this run did not complete.")


def login_required_event(account: ClaudeAccount, reason: str) -> dict:
  """The operator notice for an account that needs a new login; chat renders it account-free."""
  return {
      "type": ET.CLAUDE_ACCOUNT_LOGIN_REQUIRED,
      "account": account.label,
      "config_dir": account.config_dir,
      "reason": reason,
  }


def move_to_next_account(
    cfg: CharlieBotConfig,
    model: str | None,
    current: ClaudeAccount,
    cc_session_id: str | None,
    now: datetime | None = None,
) -> tuple[ClaudeAccount | None, str | None]:
  """Choose the account with the most headroom besides *current* and copy the transcript there.

  Returns ``(account, None)``, or ``(None, error)`` when the pool is exhausted,
  the transcript has no id yet, or the copy failed; the caller ends the run loudly.
  """
  if not cc_session_id:
    return None, "Claude account relay impossible: the run produced no session id to resume."
  nxt = claude_accounts.select(cfg, model, current=None, exclude={current.label}, now=now)
  if nxt is None:
    return None, pool_exhausted_message(cfg, now)
  try:
    claude_accounts.move_transcript(cc_session_id, current.config_dir, nxt.config_dir)
  except claude_accounts.TranscriptMoveError as exc:
    return None, f"Claude account relay failed: {exc}"
  return nxt, None


class PoolExhaustedError(Exception):
  """No pool account can take the run; callers report it as quota exhaustion with the reset time."""


def pin_pool_account(cfg: CharlieBotConfig, option: BackendOption) -> tuple[BackendOption, ClaudeAccount | None]:
  """Pin a pooled entry to the account with the most headroom; any other option passes through unchanged."""
  if not claude_accounts.is_pooled(option, cfg):
    return option, None
  account = claude_accounts.select(cfg, option.model, current=None)
  if account is None:
    raise PoolExhaustedError(pool_exhausted_message(cfg))
  return option.model_copy(update={"claude_config_dir": account.config_dir}), account

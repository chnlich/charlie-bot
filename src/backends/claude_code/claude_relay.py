"""Account relay mechanics shared by the master turn and delegated workers.

A Claude Code process runs on one pool account (src/backends/claude_code/claude_accounts.py).
Both the master turn (src/backends/claude_code/master_cc_relay.py) and a delegated worker
(src/runtime/worker.py) need the same answers while that process runs: which
events mean the account is running out, where the process can stop safely, and
how the run continues on the next account. This module owns those answers so
the callers differ only in how they persist events and rebuild the process.

During a run every ``rate_limit_event`` updates the account's reading. A
rejection means the process ends on its own and the run relays; a warning at
or above the warning line with the reset more than 45 minutes away arms a
relay at the next safe point, the first tool result after the warning. The
safe point terminates the process only when the pool holds an account strictly
below the warning line; with none, the run stays armed and keeps going, and
the next tool result recomputes. An exit that names an authentication failure
marks the account unhealthy and relays too.

Relay: the transcript is copied to the destination account and Claude Code
resumes the same session id with one fixed continuation prompt. A warning
relay lands on an account strictly below the warning line, falling back to the
most headroom when that account vanishes between the gate and the move; a
rejection lands on the account with the most headroom among the others. The
user sees no account names; those stay in the server log, the metadata and the
usage panel.
"""

from __future__ import annotations

import datetime
from typing import NamedTuple

from src.backends.claude_code import claude_accounts, claude_config, event_types
from src.infra import config
from src.infra import event_types as ET

# One run relays at most this many times: four accounts rejecting in turn must
# end in a loud error, never a loop.
MAX_RELAYS_PER_TURN = 3

# Operator notice from the account pool (claude_accounts.py): one login lost its credentials or
# failed to authenticate and needs an interactive `claude /login` in ``config_dir``. Fields:
# account, config_dir, reason ("auth_failed" | "empty_credentials"). The event type is shared
# with the account-free chat renderer through event_types.py.

# A warning whose window resets sooner than this is left alone: waiting for the
# reset costs less than a cold start on another account.
RELAY_MIN_RESET_AHEAD = datetime.timedelta(minutes=45)

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

# The login-required reason values: event_types.py pins the closed pair, and
# each emit site labels both its server-log row and the chat event with the
# same value, so a split spelling would fork the operator's grep.
LOGIN_REASON_AUTH_FAILED = "auth_failed"
LOGIN_REASON_EMPTY_CREDENTIALS = "empty_credentials"

# The one pool-exhausted phrase: ``pool_exhausted_message`` renders it, and the
# phrase-pin tests (test_task_sequences, test_task_execution) hold the prefix
# stable. Improve's quota classification reads the refusal event's
# QUOTA_EXHAUSTED flag, never this text.
POOL_EXHAUSTED_PHRASE = "Claude account pool has no available account"


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


def _reset_far_enough(info: dict, now: datetime.datetime) -> bool:
  resets_at = info.get("resetsAt")
  if not isinstance(resets_at, (int, float)) or isinstance(resets_at, bool):
    return True
  return datetime.datetime.fromtimestamp(float(resets_at), datetime.UTC) - now > RELAY_MIN_RESET_AHEAD


class RelayWatch:
  """Folds one run's events into a relay decision for its account.

  The armed safe point terminates only when ``cool_destination`` names an account
  strictly below the warning line; with none the run stays armed and the next safe
  point recomputes.
  """

  def __init__(
      self,
      label: str,
      model: str | None,
      cfg: config.CharlieBotConfig,
      account_pool: str | None,
      now: datetime.datetime | None = None,
  ) -> None:
    self.label = label
    self.model = model
    self.cfg = cfg
    self.account_pool = account_pool
    self._now = now
    self.reason: str | None = None
    self.armed = False
    self.auth_failed = False
    # The decision ``wants_next`` last judged; ``next_launch`` reads it to tell a login failure
    # from a quota relay.
    self.decided: str | None = None

  def observe(self, event: dict) -> bool:
    """Record the event; True exactly when the run must be terminated now (an armed safe
    point with an account strictly below the warning line to move to)."""
    event_type = event.get("type")
    if event_type == ET.RATE_LIMIT_EVENT:
      info = event.get(ET.RATE_LIMIT_INFO)
      if not isinstance(info, dict):
        return False
      moment = claude_accounts.now_or(self._now)
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
      moment = claude_accounts.now_or(self._now)
      destination = claude_accounts.cool_destination(self.cfg, self.model, account_pool=self.account_pool, now=moment)
      if destination is None or destination.label == self.label:
        # Nothing strictly below the warning line: stay armed and keep running; the
        # next safe point recomputes.
        return False
      self.reason = RELAY_WARNING
      return True
    return False

  def decision(self, exit_code: int, stderr_text: str) -> str | None:
    """What the finished run asks for: a relay reason, a login failure, or None."""
    if exit_code != 0 and (self.auth_failed or AUTH_FAILURE_MARKER in (stderr_text or "")):
      return LOGIN_FAILED
    return self.reason

  def wants_next(self, exit_code: int, stderr: str) -> bool:
    """True when the finished run asks for a relay; the reason stays in ``decided``."""
    self.decided = self.decision(exit_code, stderr)
    return self.decided is not None


def _pool_detail(cfg: config.CharlieBotConfig, now: datetime.datetime | None, account_pool: str | None) -> str:
  """The pool state both relay error messages render: that pool's earliest reset or, with
  no rejection reset ahead, each account's unavailability."""
  reset = claude_accounts.earliest_reset(cfg, now, account_pool)
  if reset is not None:
    when = f"earliest reset {reset.astimezone(datetime.UTC).strftime('%H:%M')} UTC"
  else:
    when = claude_accounts.unavailable_detail(cfg, now, account_pool) or "no unavailability recorded"
  return f"pool '{account_pool}'; {when}" if account_pool else when


def pool_exhausted_message(
    cfg: config.CharlieBotConfig, now: datetime.datetime | None = None, account_pool: str | None = None) -> str:
  """The pool-exhausted error: it names the pool and that pool's earliest reset.

  A named pool renders ``... no available account (pool 'alpha'; earliest reset
  03:00 UTC); ...``; with no rejection reset ahead the reset slot instead lists
  each account's unavailability (``... no available account (pool 'alpha';
  main: missing credentials); ...``). ``POOL_EXHAUSTED_PHRASE`` itself stays
  byte-identical: the phrase-pin tests hold the prefix stable.
  """
  return f"{POOL_EXHAUSTED_PHRASE} ({_pool_detail(cfg, now, account_pool)}); this run did not complete."


def relay_limit_message(cfg: config.CharlieBotConfig, account_pool: str | None = None) -> str:
  """The relay-cap error; the pool detail is ``pool_exhausted_message``'s rendering."""
  return (
      f"Claude account relay limit reached ({MAX_RELAYS_PER_TURN} relays in one run; "
      f"{_pool_detail(cfg, None, account_pool)}); this run did not complete.")


def login_required_event(account: claude_config.ClaudeAccount, reason: str) -> dict:
  """The operator notice for an account that needs a new login; chat renders it account-free."""
  return {
      "type": event_types.CLAUDE_ACCOUNT_LOGIN_REQUIRED,
      "account": account.label,
      "config_dir": account.config_dir,
      "reason": reason,
  }


class RelayMove(NamedTuple):
  """The outcome of one relay move.

  ``account`` is the login the run continues on; None when the move failed, and then ``error``
  says why. ``refused_holder`` is the destination login that holds a newer transcript, when the
  newer-transcript guard refused the copy. ``pool_exhausted`` is True when no pool login could
  take the run.
  """
  account: claude_config.ClaudeAccount | None
  error: str | None
  refused_holder: claude_config.ClaudeAccount | None
  pool_exhausted: bool


def relay_move(
    cfg: config.CharlieBotConfig,
    model: str | None,
    current: claude_config.ClaudeAccount,
    cc_session_id: str | None,
    now: datetime.datetime | None = None,
    account_pool: str | None = None,
    min_headroom: float = 0.0,
) -> RelayMove:
  """Choose the account with the most headroom besides *current* and copy the transcript there.

  The next account comes from pool *account_pool* alone; the relay never falls
  back to another pool. A *min_headroom* above zero (the warning relay's
  strictly-below-the-line bar) falls back to the regular select when nothing
  qualifies: the gate's reading may have been a transient, and a fallback
  landing may sit at or above the line and self-corrects at the next safe
  point. The pool exhausted and the transcript-missing cases leave
  *refused_holder* None, and a copy the newer-transcript guard refused carries
  the destination account holding the newer transcript. This module never
  redirects a refused move -- adopting *refused_holder* and continuing the run
  from it is the consumer's decision; a consumer that has none ends the run
  loudly on *error*.
  """
  if not cc_session_id:
    return RelayMove(
        None, "Claude account relay impossible: the run produced no session id to resume.", None, pool_exhausted=False)
  nxt = claude_accounts.select(
      cfg, model, exclude={current.label}, now=now, account_pool=account_pool, min_headroom=min_headroom)
  if nxt is None and min_headroom > 0.0:
    nxt = claude_accounts.select(cfg, model, exclude={current.label}, now=now, account_pool=account_pool)
  if nxt is None:
    return RelayMove(None, pool_exhausted_message(cfg, now, account_pool), None, pool_exhausted=True)
  try:
    claude_accounts.move_transcript(cc_session_id, current.config_dir, nxt.config_dir)
  except claude_accounts.TranscriptMoveError as exc:
    refused = claude_accounts.is_newer_transcript_refusal(exc)
    return RelayMove(None, f"Claude account relay failed: {exc}", nxt if refused else None, pool_exhausted=False)
  return RelayMove(nxt, None, None, pool_exhausted=False)

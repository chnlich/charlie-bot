"""The Claude Code package's account-free chat renderer."""

from __future__ import annotations

from collections.abc import Callable

from src.backends.claude_code.event_types import (
    CLAUDE_ACCOUNT_LOGIN_REQUIRED as _CLAUDE_ACCOUNT_LOGIN_REQUIRED,
)
from src.runtime.hooks import turn_contributions


def _claude_account_login_required_msg(ev: dict) -> dict:
  """Account-free by design: the login directory is on the usage panel, never in chat."""
  del ev
  return {
      'role': 'system',
      'kind': _CLAUDE_ACCOUNT_LOGIN_REQUIRED,
      'content': 'One account in the Claude pool needs a new login; see the usage panel.',
  }


class ClaudeCodeTurnContribution(turn_contributions.TurnContribution):
  """Render Claude Code account notices in chat."""

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {_CLAUDE_ACCOUNT_LOGIN_REQUIRED: _claude_account_login_required_msg}


CONTRIBUTION = ClaudeCodeTurnContribution()

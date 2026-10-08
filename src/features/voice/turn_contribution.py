"""The voice package's turn contribution: a dictated message opens with a note that it may hold recognition errors."""

from __future__ import annotations

from src.infra.models import SessionMetadata
from src.runtime.hooks import turn_contributions

# The ``input_mode`` value that the web client sends and the user event stores for a dictated message.
VOICE_INPUT_MODE = "voice"

VOICE_NOTE = (
    "[Voice input: this message was dictated via speech transcription and may "
    "contain recognition errors. Interpret unclear words from context; ask only "
    "when the intent is genuinely ambiguous.]")


class VoiceTurnContribution(turn_contributions.TurnContribution):
  """Opens each dictated user input with ``VOICE_NOTE``; the chat view shows the message verbatim."""

  def input_preamble(self, meta: SessionMetadata, input_event: dict) -> str | None:
    return VOICE_NOTE if input_event.get("input_mode") == VOICE_INPUT_MODE else None


CONTRIBUTION = VoiceTurnContribution()

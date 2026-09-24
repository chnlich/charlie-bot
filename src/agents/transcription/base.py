"""The transcription backend contract: one abstract class every speech-to-text engine implements.

Voice input selects a backend by id through the registry
(src/agents/transcription/registry.py); the preview relay, the page's backend
menu, and the replay script all see only this base class. Mirrors the LLM
backend layout (src/agents/backends/base.py plus that package's registry).
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from collections.abc import AsyncIterator, Sequence
from dataclasses import dataclass
from typing import ClassVar, Literal


@dataclass(frozen=True)
class TranscriptEvent:
  """One transcription output event.

  ``text`` is the cumulative text of the whole recording so far: every event
  replaces the previous one, and the ``final`` event carries the text to keep.
  """

  kind: Literal["partial", "final"]
  text: str


# The plan fixes this name; the Error suffix would read as a transport fault, and a
# rejection is a session-level decision, not a broken connection.
class TranscriptionRejected(Exception):  # noqa: N818
  """The backend refused the session before any transcript: bad credential, setup
  reply missing, or an error close. A mid-stream transport failure is not a
  rejection — it propagates as the transport's own error."""


class TranscriptionBackend(ABC):
  """One speech-to-text engine the voice input can select by id.

  The whole backend session — connect, handshake, audio turns, close — lives
  inside one ``transcribe`` call, so callers hold no per-session state and no
  call order to follow.
  """

  # Registry key ("local", "gemini", "muse") and the dropdown text shown beside
  # the microphone. A backend whose label depends on config overrides ``label``
  # with a property.
  id: ClassVar[str]
  label: ClassVar[str]
  # True when text arrives while the user is still speaking.
  live_partials: ClassVar[bool]

  def unavailable_reason(self) -> str | None:
    """Why this backend cannot run now, e.g. ``"needs gemini.api_key"``; None when ready.

    Building a backend never requires its credential: availability is answered
    here, and ``transcribe`` raises :class:`TranscriptionRejected` when the
    credential is absent anyway.
    """
    return None

  @abstractmethod
  def transcribe(
      self,
      audio: AsyncIterator[bytes],
      *,
      vocabulary: Sequence[str],
      languages: Sequence[str],
  ) -> AsyncIterator[TranscriptEvent]:
    """Transcribe one recording: 16 kHz mono PCM16 chunks in, events out.

    Exhaustion of ``audio`` is the end of the audio. Emits zero or more
    ``partial`` events, then exactly one ``final``. ``vocabulary`` biases
    proper nouns for the backends that support it. ``languages`` are BCP-47
    base codes (``zh``, ``en``); each backend maps them to its own wire format
    and ignores codes it cannot map. Raises :class:`TranscriptionRejected`
    when the backend refuses the session. Closing the returned iterator early
    ends the backend session and releases its connection.
    """

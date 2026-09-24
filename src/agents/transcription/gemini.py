"""Gemini 3.5 Transcribe Live backend: the public Live API over client-delimited turns.

The server's automatic segmentation drops speech, and nothing arrives after
``audioStreamEnd`` (measured in the 2026-09-24 live probe): so this backend
disables automatic activity detection, opens the turn itself with
``activityStart`` before the first chunk, and closes it with ``activityEnd``
after the last. The whole recording then comes back as one final, with
cumulative interim transcriptions while the audio is still arriving.
"""

from __future__ import annotations

import asyncio
import base64
import json
from collections.abc import AsyncIterator, Sequence

from src.agents.transcription.base import TranscriptEvent, TranscriptionBackend, TranscriptionRejected
from src.core.config import CharlieBotConfig
from src.core.credentials import get_credentials

MODEL = "models/gemini-3.5-transcribe-live"
SAMPLE_RATE = 16_000
DEFAULT_ENDPOINT_URL = (
    "wss://generativelanguage.googleapis.com/ws/"
    "google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent")
SETUP_TIMEOUT_S = 15.0

# BCP-47 base codes -> the Live API's languageCodes. Unmapped codes are dropped;
# an empty list leaves the model auto-detecting.
LANGUAGE_CODES = {"zh": "cmn-Hans-CN", "en": "en-US"}

# Interim transcriptions carry spaces between CJK characters that the final
# drops (probe interims read "我 记 得 在 这 句"); strips exactly those, keeping
# every space next to Latin text.
_CJK_RANGES = (
    (0x3000, 0x303F),  # CJK symbols and punctuation
    (0x3040, 0x30FF),  # kana
    (0x3400, 0x4DBF),  # CJK extension A
    (0x4E00, 0x9FFF),  # CJK unified ideographs
    (0xF900, 0xFAFF),  # CJK compatibility ideographs
    (0xFF00, 0xFFEF),  # fullwidth forms
    (0x20000, 0x2FA1F),  # CJK extensions B-F and the compatibility supplement
)


def _is_cjk(char: str) -> bool:
  code = ord(char)
  return any(low <= code <= high for low, high in _CJK_RANGES)


def _strip_cjk_spaces(text: str) -> str:
  """Drop spaces whose two neighbours are both CJK; keep every other space."""
  kept: list[str] = []
  for index, char in enumerate(text):
    if (char == " " and kept and _is_cjk(kept[-1]) and index + 1 < len(text) and _is_cjk(text[index + 1])):
      continue
    kept.append(char)
  return "".join(kept)


class GeminiTranscriptionBackend(TranscriptionBackend):
  id = "gemini"
  label = "Gemini 3.5 Transcribe Live"
  live_partials = True

  def __init__(self, cfg: CharlieBotConfig, endpoint_url: str = DEFAULT_ENDPOINT_URL) -> None:
    self._cfg = cfg
    # Constructor argument, not config: tests point it at a loopback fake server.
    self._endpoint_url = endpoint_url

  def unavailable_reason(self) -> str | None:
    if not get_credentials().get("gemini", "api_key"):
      return "needs gemini.api_key"
    return None

  async def transcribe(
      self,
      audio: AsyncIterator[bytes],
      *,
      vocabulary: Sequence[str],
      languages: Sequence[str],
  ) -> AsyncIterator[TranscriptEvent]:
    api_key = str(get_credentials().get("gemini", "api_key") or "")
    if not api_key:
      raise TranscriptionRejected("gemini.api_key is not set in credentials.yaml")
    from websockets.asyncio.client import connect

    # The key rides the query string like the public endpoint expects; it is
    # never logged and never reaches an event.
    async with connect(
        f"{self._endpoint_url}?key={api_key}", max_size=None, open_timeout=SETUP_TIMEOUT_S) as socket:
      await socket.send(json.dumps(self._setup_frame(vocabulary, languages)))
      reply = json.loads(await asyncio.wait_for(socket.recv(), SETUP_TIMEOUT_S))
      if "setupComplete" not in reply:
        raise TranscriptionRejected(f"setup reply was not setupComplete: {sorted(reply)}")

      async def send_audio() -> None:
        await socket.send(json.dumps({"realtimeInput": {"activityStart": {}}}))
        async for chunk in audio:
          audio_frame = {"data": base64.b64encode(chunk).decode("ascii"), "mimeType": f"audio/pcm;rate={SAMPLE_RATE}"}
          await socket.send(json.dumps({"realtimeInput": {"audio": audio_frame}}))
        await socket.send(json.dumps({"realtimeInput": {"activityEnd": {}}}))

      # Audio is sent as fast as it arrives, concurrently with receiving: the
      # API accepts a burst, and pacing here would only add stop-to-final lag.
      def end_receive_when_sender_dies(sender_task: asyncio.Task) -> None:
        """A sender that died must close the session now: the receive loop would
        otherwise wait forever for messages that will never come."""
        if not sender_task.cancelled() and sender_task.exception() is not None:
          asyncio.create_task(socket.close())

      sender = asyncio.create_task(send_audio())
      sender.add_done_callback(end_receive_when_sender_dies)
      try:
        async for raw in socket:
          content = json.loads(raw).get("serverContent", {})
          interim = content.get("interimInputTranscription", {}).get("text")
          if interim is not None:
            yield TranscriptEvent(kind="partial", text=_strip_cjk_spaces(interim))
            continue
          final_text = content.get("inputTranscription", {}).get("text")
          if final_text is not None:
            # The generationComplete arriving with it says no text follows.
            yield TranscriptEvent(kind="final", text=final_text)
            return
      finally:
        sender.cancel()
        await asyncio.gather(sender, return_exceptions=True)
      # A clean close without a final is a session that produced nothing.
      raise RuntimeError("connection closed before a final transcription")

  def _setup_frame(self, vocabulary: Sequence[str], languages: Sequence[str]) -> dict:
    """The first frame on the socket. VERBATIM keeps the user's exact words."""
    transcription = {
        "languageCodes": [LANGUAGE_CODES[code] for code in languages if code in LANGUAGE_CODES],
        "customVocabulary": list(vocabulary),
        "mode": "VERBATIM",
    }
    return {
        "setup":
            {
                "model": MODEL,
                "generationConfig": {
                    "responseModalities": ["TEXT"]
                },
                "inputAudioTranscription": transcription,
                "realtimeInputConfig": {
                    "automaticActivityDetection": {
                        "disabled": True
                    }
                },
            }
    }

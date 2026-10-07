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
import re
from collections.abc import AsyncIterator, Sequence

from src.features.voice.transcription import base
from src.infra import config, credentials, timeouts

MODEL = "models/gemini-3.5-transcribe-live"
DEFAULT_ENDPOINT_URL = (
    "wss://generativelanguage.googleapis.com/ws/"
    "google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent")
SETUP_TIMEOUT_S = 15.0

# BCP-47 base codes -> the Live API's languageCodes. Unmapped codes are dropped;
# an empty list leaves the model auto-detecting.
LANGUAGE_CODES = {"zh": "cmn-Hans-CN", "en": "en-US"}

# The Live API's text arrives in two shapes this backend flattens: interims
# always arrive character-split with spaces (probe interims read
# "我 记 得 在 这 句"), and finals occasionally arrive in that same form, there
# with ASCII punctuation around the marks. One normalizer owns the transcript
# shape and both yield sites call it; every space next to Latin text survives.
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


# The clause marks a spaced-out transcript puts spaces around: the ASCII set
# plus the fullwidth forms the space can still sit beside (the mark preceded by
# non-CJK text). Rule 4 converts only the ASCII ones, so the dot stays a dot.
_CLAUSE_MARKS = ",?!;:.、。，？！；："
_FULLWIDTH_OF = {",": "，", "?": "？", "!": "！", ";": "；", ":": "："}

# A run of spaces before a mark that itself closes a clause — the mark is
# followed by whitespace, the text's end, or non-ASCII text — goes; "cd ./dir"
# keeps its space because its dot is followed by a slash.
_SPACE_BEFORE_CLAUSE_MARK = re.compile(r" +(?=[" + _CLAUSE_MARKS + r"](?:\s|$|[^\x00-\x7f]))")
# One space between an ASCII mark and non-ASCII text goes.
_SPACE_AFTER_ASCII_MARK = re.compile(r"(?<=[,?!;:.]) (?=[^\x00-\x7f])")


def _normalize_transcript(text: str) -> str:
  """The one transcript shape both yield sites pass through.

  Rules, in order: spaces between two CJK characters go; the spaces before a
  clause mark go when the mark itself ends a clause; one space between an ASCII
  mark and non-ASCII text goes; an ASCII mark directly after CJK text becomes
  fullwidth. Spaces between Latin and CJK text stay ("cron job 的 session").
  """
  out = _strip_cjk_spaces(text)
  out = _SPACE_BEFORE_CLAUSE_MARK.sub("", out)
  out = _SPACE_AFTER_ASCII_MARK.sub("", out)
  chars = list(out)
  for index, char in enumerate(chars):
    if char in _FULLWIDTH_OF and index > 0 and _is_cjk(chars[index - 1]):
      chars[index] = _FULLWIDTH_OF[char]
  return "".join(chars)


class GeminiTranscriptionBackend(base.TranscriptionBackend):
  id = "gemini"
  label = "Gemini 3.5 Transcribe Live"
  live_partials = True

  def __init__(self, cfg: config.CharlieBotConfig, endpoint_url: str = DEFAULT_ENDPOINT_URL) -> None:
    self._cfg = cfg
    # Constructor argument, not config: tests point it at a loopback fake server.
    self._endpoint_url = endpoint_url

  def unavailable_reason(self) -> str | None:
    if not credentials.get_credentials().get("gemini", "api_key"):
      return "needs gemini.api_key"
    return None

  async def transcribe(
      self,
      audio: AsyncIterator[bytes],
      *,
      vocabulary: Sequence[str],
      languages: Sequence[str],
  ) -> AsyncIterator[base.TranscriptEvent]:
    api_key = str(credentials.get_credentials().get("gemini", "api_key") or "")
    if not api_key:
      raise base.TranscriptionRejected("gemini.api_key is not set in credentials.yaml")
    from websockets.asyncio import client

    # The key rides the query string like the public endpoint expects; it is
    # never logged and never reaches an event.
    async with client.connect(
        f"{self._endpoint_url}?key={api_key}",
        max_size=None,
        open_timeout=SETUP_TIMEOUT_S,
        close_timeout=timeouts.WS_CLIENT_CLOSE_TIMEOUT,
    ) as socket:
      await socket.send(json.dumps(self._setup_frame(vocabulary, languages)))
      reply = json.loads(await asyncio.wait_for(socket.recv(), SETUP_TIMEOUT_S))
      if "setupComplete" not in reply:
        raise base.TranscriptionRejected(f"setup reply was not setupComplete: {sorted(reply)}")

      async def send_audio() -> None:
        await socket.send(json.dumps({"realtimeInput": {"activityStart": {}}}))
        async for chunk in audio:
          audio_frame = {
              "data": base64.b64encode(chunk).decode("ascii"),
              "mimeType": f"audio/pcm;rate={base.SAMPLE_RATE}"
          }
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
            yield base.TranscriptEvent(kind="partial", text=_normalize_transcript(interim))
            continue
          final_text = content.get("inputTranscription", {}).get("text")
          if final_text is not None:
            # The generationComplete arriving with it says no text follows.
            yield base.TranscriptEvent(kind="final", text=_normalize_transcript(final_text))
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

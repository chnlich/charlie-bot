"""Gemini 3.5 Transcribe through the aigw gateway: the whole recording in one HTTP request.

The gateway's /gemini pass-through forwards the request to Google unchanged and
swaps only the key, so the spend lands on the gateway's key (``aigw.api_key``)
instead of a separate Google one. The backend is not live: the upload endpoint
hands it the whole sealed recording, it wraps the PCM as a 16 kHz mono WAV,
POSTs once, and returns the transcription as one final — the browser shows no
text while the user is still speaking. The reply's text sits in
``candidates[0].content.parts[i].audioTranscription.text``; reading the Live
API's ``parts[i].text`` returns nothing (the 2026-09 batch's empty outputs).
"""

from __future__ import annotations

import base64
import io
import wave
from collections.abc import AsyncIterator, Sequence

from src.agents.transcription.base import (
    SAMPLE_RATE,
    TranscriptEvent,
    TranscriptionBackend,
    TranscriptionRejected,
)
from src.agents.transcription.gemini import LANGUAGE_CODES
from src.core.config import CharlieBotConfig
from src.core.credentials import get_credentials

MODEL = "gemini-3.5-transcribe"
# The browser's upload budget (web/static/js/voice-input.js
# VOICE_UPLOAD_TIMEOUT_MS): past it the client has already aborted into its
# retry state, so a longer server wait answers nobody.
TRANSCRIBE_TIMEOUT_S = 60.0
# How much of a failed reply's body an error message carries.
ERROR_BODY_EXCERPT_CHARS = 500


def _pcm_as_wav(pcm_bytes: bytes) -> bytes:
  """Wrap mono PCM16 at SAMPLE_RATE in the WAV container the API takes inline."""
  buffer = io.BytesIO()
  with wave.open(buffer, "wb") as wav:
    wav.setnchannels(1)
    wav.setsampwidth(2)
    wav.setframerate(SAMPLE_RATE)
    wav.writeframes(pcm_bytes)
  return buffer.getvalue()


def _request_body(pcm_bytes: bytes, vocabulary: Sequence[str], languages: Sequence[str]) -> dict:
  """The one generateContent body: the inline WAV plus the transcription config.

  The transcription config mirrors the Live setup frame's inputAudioTranscription
  (same field names, same LANGUAGE_CODES mapping): VERBATIM keeps the user's
  exact words, unmapped language codes drop, and empty lists send no field so
  the model auto-detects.
  """
  transcription: dict = {"mode": "VERBATIM"}
  language_codes = [LANGUAGE_CODES[code] for code in languages if code in LANGUAGE_CODES]
  if language_codes:
    transcription["languageCodes"] = language_codes
  if vocabulary:
    transcription["customVocabulary"] = list(vocabulary)
  return {
      "contents":
          [
              {
                  "parts":
                      [
                          {
                              "inlineData":
                                  {
                                      "mimeType": "audio/wav",
                                      "data": base64.b64encode(_pcm_as_wav(pcm_bytes)).decode("ascii"),
                                  }
                          }
                      ]
              }
          ],
      "generationConfig": {
          "audioTranscriptionConfig": transcription
      },
  }


def _body_excerpt(body: str) -> str:
  return body[:ERROR_BODY_EXCERPT_CHARS]


def _final_text(reply: dict, reply_body: str) -> str:
  """Join every part's ``audioTranscription.text`` — the reply's only text location.

  A reply without candidates is a transport-level break and raises. Candidates
  carrying no parts is the model saying nothing only when the candidate closed
  with ``finishReason: STOP`` — measured on three recordings without speech
  (two digital silences and one noise clip, the same three the Live API closed
  with 1008 on) — and joins zero parts into an empty text. Any other
  finishReason, or a missing one, raises instead of passing as silence: an
  empty text has the browser report no speech and drop the recording, hiding a
  refusal such as SAFETY. A part that exists but lacks the transcription
  raises too: that shape is the 2026-09 batch's parse bug, wrong-location
  reading dressed up as model output.
  """
  excerpt = _body_excerpt(reply_body)
  candidates = reply.get("candidates")
  if not isinstance(candidates, list) or not candidates:
    raise RuntimeError(f"transcription reply carries no candidates: {excerpt}")
  candidate = candidates[0]
  parts = candidate.get("content", {}).get("parts") or []
  if not parts:
    # Only STOP makes the empty parts a proven silence; any other close, or
    # none, is a refusal the browser would read as no speech and drop.
    finish_reason = candidate.get("finishReason")
    if finish_reason != "STOP":
      if finish_reason is None:
        raise RuntimeError(f"transcription reply carries no parts and no finishReason: {excerpt}")
      raise RuntimeError(f"transcription reply carries no parts with finishReason {finish_reason!r}: {excerpt}")
  texts = []
  for part in parts:
    transcription = part.get("audioTranscription")
    if not isinstance(transcription, dict) or "text" not in transcription:
      raise RuntimeError(f"transcription reply part carries no audioTranscription.text: {excerpt}")
    texts.append(transcription["text"])
  return "".join(texts)


class GeminiAigwTranscriptionBackend(TranscriptionBackend):
  id = "gemini-aigw"
  label = "Gemini 3.5 Transcribe · aigw"
  live_partials = False

  def __init__(self, cfg: CharlieBotConfig) -> None:
    self._cfg = cfg

  def unavailable_reason(self) -> str | None:
    if not self._cfg.voice.aigw_base_url:
      return "needs voice.aigw_base_url"
    if not get_credentials().get("aigw", "api_key"):
      return "needs aigw.api_key"
    return None

  async def transcribe(
      self,
      audio: AsyncIterator[bytes],
      *,
      vocabulary: Sequence[str],
      languages: Sequence[str],
  ) -> AsyncIterator[TranscriptEvent]:
    """Drain ``audio``, POST the whole recording once, yield the joined text as one final."""
    api_key = get_credentials().get("aigw", "api_key")
    if not api_key:
      raise TranscriptionRejected("needs aigw.api_key")
    pcm = bytearray()
    async for chunk in audio:
      pcm.extend(chunk)
    # One request per recording: a per-call client carries the timeout as its
    # default and keeps this backend off the shared client singleton. Deferred
    # import: pages.py builds every backend per page load, and httpx's import
    # chain must stay off that path. The key rides the header and never reaches
    # an exception message, a log line, or an event; httpx errors carry the
    # URL, not the request headers.
    import httpx

    async with httpx.AsyncClient(timeout=TRANSCRIBE_TIMEOUT_S) as client:
      reply = await client.post(
          f"{self._cfg.voice.aigw_base_url.rstrip('/')}/gemini/v1beta/models/{MODEL}:generateContent",
          headers={"x-goog-api-key": str(api_key)},
          json=_request_body(bytes(pcm), vocabulary, languages),
      )
    # The non-streaming post() has read the whole body, so reply.text is in memory.
    if reply.status_code in (401, 403):
      raise TranscriptionRejected(
          f"transcription request rejected with {reply.status_code}: {_body_excerpt(reply.text)}")
    if not reply.is_success:
      raise RuntimeError(f"transcription request failed with {reply.status_code}: {_body_excerpt(reply.text)}")
    try:
      payload = reply.json()
    except ValueError as exc:
      raise RuntimeError(f"transcription reply was not JSON: {_body_excerpt(reply.text)}") from exc
    yield TranscriptEvent(kind="final", text=_final_text(payload, reply.text))

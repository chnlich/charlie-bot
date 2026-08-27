"""Voice WebSocket handling and persistence."""

from __future__ import annotations

import asyncio
import json
import wave
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

import structlog
from fastapi import WebSocket, WebSocketDisconnect

from src.agents.transcriber import (
  SAMPLE_RATE,
  SimulatedStreamingTranscriptionSession,
  SpeechModelsNotReady,
  create_transcription_session,
)
from src.core.config import CharlieBotConfig, get_config

log = structlog.get_logger()

_active_voice_sockets: dict[str, WebSocket] = {}
_active_voice_lock = asyncio.Lock()


async def handle_voice_websocket(websocket: WebSocket, session_id: str) -> None:
  """Run one local streaming transcription WebSocket."""
  cfg = get_config()
  try:
    session = await asyncio.to_thread(create_transcription_session, cfg)
  except SpeechModelsNotReady as exc:
    await _send_error_and_close(websocket, str(exc))
    return
  except Exception as exc:
    log.exception("voice_session_create_failed", session_id=session_id)
    await _send_error_and_close(websocket, f"speech inference failed: {exc}")
    return

  await _claim_session_stream(session_id, websocket)
  final_sent = False
  try:
    while True:
      message = await websocket.receive()
      if message["type"] == "websocket.disconnect":
        log.info("voice_ws_disconnected", session_id=session_id)
        return

      if message.get("bytes") is not None:
        update = await asyncio.to_thread(session.accept_pcm, message["bytes"])
        if update.text is not None:
          await websocket.send_json({"type": "partial", "text": update.text})
        if update.cap_reached:
          final_sent = await _finish_voice_stream(websocket, cfg, session_id, session)
          return
        continue

      if message.get("text") is not None:
        try:
          is_stop = _is_stop_frame(message["text"])
        except ValueError:
          await _send_error_and_close(websocket, "invalid voice frame")
          return
        if is_stop:
          final_sent = await _finish_voice_stream(websocket, cfg, session_id, session)
          return
        await _send_error_and_close(websocket, "invalid voice frame")
        return

      await _send_error_and_close(websocket, "invalid voice frame")
      return
  except WebSocketDisconnect:
    log.info("voice_ws_disconnected", session_id=session_id)
  except Exception as exc:
    log.exception("voice_ws_failed", session_id=session_id)
    if not final_sent:
      await _send_error_and_close(websocket, f"speech inference failed: {exc}")
  finally:
    await _release_session_stream(session_id, websocket)


async def _claim_session_stream(session_id: str, websocket: WebSocket) -> None:
  async with _active_voice_lock:
    previous = _active_voice_sockets.get(session_id)
    _active_voice_sockets[session_id] = websocket
  if previous is not None and previous is not websocket:
    await _send_error_and_close(previous, "voice stream was replaced")
  log.info("voice_ws_connected", session_id=session_id)


async def _release_session_stream(session_id: str, websocket: WebSocket) -> None:
  async with _active_voice_lock:
    if _active_voice_sockets.get(session_id) is websocket:
      _active_voice_sockets.pop(session_id, None)
  log.info("voice_ws_closed", session_id=session_id)


def _is_stop_frame(text: str) -> bool:
  try:
    data = json.loads(text)
  except json.JSONDecodeError as exc:
    raise ValueError("invalid voice JSON frame") from exc
  return data == {"type": "stop"}


async def _finish_voice_stream(
    websocket: WebSocket,
    cfg: CharlieBotConfig,
    session_id: str,
    session: SimulatedStreamingTranscriptionSession,
) -> bool:
  final_text = await asyncio.to_thread(session.finish)
  audio_path = await asyncio.to_thread(_persist_voice_dump, cfg, session_id, session.audio_bytes, final_text)
  log.info(
      "voice_transcribed",
      session_id=session_id,
      audio_path=str(audio_path),
      audio_bytes_size=len(session.audio_bytes),
      transcription_length=len(final_text),
      transcription_preview=final_text[:80],
  )
  await websocket.send_json({"type": "final", "text": final_text})
  try:
    await websocket.close()
  except Exception:
    log.exception("voice_ws_close_failed")
  return True


def _persist_voice_dump(cfg: CharlieBotConfig, session_id: str, audio_bytes: bytes, transcription: str) -> Path:
  ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H%M%S.%f")[:-3] + "Z"
  stem = f"{ts}_{uuid4().hex[:8]}"
  audio_path = cfg.sessions_dir / session_id / "voice" / f"{stem}.wav"
  text_path = cfg.sessions_dir / session_id / "voice" / f"{stem}.txt"
  audio_path.parent.mkdir(parents=True, exist_ok=True)
  _write_wav(audio_path, audio_bytes)
  text_path.write_text(transcription, encoding="utf-8")
  return audio_path


def _write_wav(path: Path, pcm_bytes: bytes) -> None:
  with wave.open(str(path), "wb") as wav:
    wav.setnchannels(1)
    wav.setsampwidth(2)
    wav.setframerate(SAMPLE_RATE)
    wav.writeframes(pcm_bytes)


async def _send_error_and_close(websocket: WebSocket, text: str) -> None:
  try:
    await websocket.send_json({"type": "error", "text": text})
  except Exception:
    log.exception("voice_ws_error_send_failed")
  try:
    await websocket.close()
  except Exception:
    log.exception("voice_ws_close_failed")

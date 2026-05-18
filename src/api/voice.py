"""Voice transcription API route."""

import asyncio
import uuid
from datetime import datetime, timezone
from pathlib import Path

import structlog
from fastapi import APIRouter, Depends, File, Form, HTTPException, UploadFile

from src.agents.master_agent import AudioTranscriber
from src.api.chat import run_and_finalize
from src.api.deps import get_session_manager
from src.core.config import CharlieBotConfig, get_config
from src.core.models import VoiceTranscriptionResponse
from src.core.sessions import SessionManager
from src.core.tasks import create_logged_task

router = APIRouter()

log = structlog.get_logger()

SUPPORTED_MIME_TYPES = {
    "audio/webm",
    "audio/ogg",
    "audio/mp4",
    "audio/mpeg",
    "audio/wav",
    "audio/flac",
    "audio/x-m4a",
}

_MIME_EXT = {
    "audio/webm": ".webm",
    "audio/ogg": ".ogg",
    "audio/mp4": ".mp4",
    "audio/mpeg": ".mp3",
    "audio/wav": ".wav",
    "audio/flac": ".flac",
    "audio/x-m4a": ".m4a",
}


def _persist_voice_dump(audio_path: Path, audio_bytes: bytes, transcription: str) -> None:
  audio_path.parent.mkdir(parents=True, exist_ok=True)
  audio_path.write_bytes(audio_bytes)
  audio_path.with_suffix(".txt").write_text(transcription, encoding="utf-8")


_transcriber: AudioTranscriber | None = None


def _get_transcriber() -> AudioTranscriber:
  global _transcriber
  if _transcriber is None:
    _transcriber = AudioTranscriber(get_config())
  return _transcriber


@router.post("/transcribe", response_model=VoiceTranscriptionResponse)
async def transcribe_audio(
    audio: UploadFile = File(...),
    session_id: str = Form(...),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Transcribe uploaded audio using Gemini."""
  content_type = audio.content_type or "audio/webm"
  if content_type not in SUPPORTED_MIME_TYPES:
    # Accept anyway — Gemini is permissive
    pass

  audio_bytes = await audio.read()
  if not audio_bytes:
    raise HTTPException(status_code=400, detail="Empty audio file")
  if len(audio_bytes) < 2048:
    raise HTTPException(status_code=400, detail="Audio too short to transcribe")

  try:
    transcriber = _get_transcriber()
    transcription = await transcriber.transcribe_audio(audio_bytes, content_type, cfg.voice_custom_words or None)
  except NotImplementedError:
    raise HTTPException(status_code=503, detail="Audio transcription requires Gemini API key")
  except Exception as e:
    raise HTTPException(status_code=500, detail=f"Transcription failed: {str(e)}")

  ext = _MIME_EXT.get(content_type, ".bin")
  ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H%M%S.%f")[:-3] + "Z"
  audio_path = cfg.charliebot_home / "sessions" / session_id / "voice" / f"{ts}_{uuid.uuid4().hex[:8]}{ext}"
  try:
    # Run blocking I/O off the event loop; don't fail the request if dump fails.
    await asyncio.to_thread(_persist_voice_dump, audio_path, audio_bytes, transcription)
    log.info(
        "voice_transcribed",
        session_id=session_id,
        audio_path=str(audio_path),
        audio_bytes_size=len(audio_bytes),
        mime_type=content_type,
        transcription_length=len(transcription),
        transcription_preview=transcription[:80],
    )
  except Exception as e:
    log.warning("voice_dump_failed", session_id=session_id, error=str(e))

  meta = await session_mgr.get_session(session_id)
  if meta:
    create_logged_task(run_and_finalize(cfg, meta, transcription, session_mgr, is_voice=True))

  return VoiceTranscriptionResponse(transcription=transcription)

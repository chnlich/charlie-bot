"""Voice transcription API route."""

import asyncio

import structlog
from fastapi import APIRouter, Depends, File, Form, HTTPException, UploadFile

from src.agents.master_agent import AudioTranscriber
from src.api.chat import run_and_finalize
from src.api.deps import get_session_manager
from src.core.config import CharlieBotConfig, get_config
from src.core.models import VoiceTranscriptionResponse
from src.core.sessions import SessionManager

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

  try:
    transcriber = _get_transcriber()
    transcription = await transcriber.transcribe_audio(audio_bytes, content_type, cfg.voice_custom_words or None)
  except NotImplementedError:
    raise HTTPException(status_code=503, detail="Audio transcription requires Gemini API key")
  except Exception as e:
    raise HTTPException(status_code=500, detail=f"Transcription failed: {str(e)}")

  meta = await session_mgr.get_session(session_id)
  if meta:
    asyncio.create_task(run_and_finalize(cfg, meta, transcription, session_mgr, is_voice=True))

  return VoiceTranscriptionResponse(transcription=transcription)

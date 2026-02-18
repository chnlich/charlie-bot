"""Voice transcription API route."""

from fastapi import APIRouter, Depends, File, Form, HTTPException, UploadFile

from src.agents.master_agent import MasterAgent
from src.api.deps import get_master_agent
from src.core.models import VoiceTranscriptionResponse

router = APIRouter()

SUPPORTED_MIME_TYPES = {
  "audio/webm",
  "audio/ogg",
  "audio/mp4",
  "audio/mpeg",
  "audio/wav",
  "audio/flac",
  "audio/x-m4a",
}


@router.post("/transcribe", response_model=VoiceTranscriptionResponse)
async def transcribe_audio(
  audio: UploadFile = File(...),
  session_id: str = Form(...),
  master: MasterAgent = Depends(get_master_agent),
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
    transcription = await master.transcribe_audio(audio_bytes, content_type)
  except NotImplementedError:
    raise HTTPException(status_code=503, detail="Audio transcription requires Gemini API key")
  except Exception as e:
    raise HTTPException(status_code=500, detail=f"Transcription failed: {str(e)}")

  return VoiceTranscriptionResponse(transcription=transcription)

"""The fallback upload's devices field: logged when present, 400 when malformed.

Each test drives upload_voice_recording directly with the speech bundle faked;
the browser's devices object rides the multipart form as JSON, the
voice_transcribed line logs its five fields, and a present-but-malformed field
is a 400 — only an absent field (a page loaded before the devices existed)
logs all five as None. Every string here is synthetic.
"""

import io
import json
import wave
from pathlib import Path

import pytest
from fastapi import UploadFile
from structlog.testing import capture_logs

from src.api import voice
from src.core.config import CharlieBotConfig

SESSION_ID = "session-a"
BACKEND = "local"
DECODED_TEXT = "synthetic decoded words"
DEVICES = {
    "input_device": "Test Microphone",
    "capture_settings":
        {
            "echoCancellation": True,
            "noiseSuppression": False,
            "autoGainControl": True,
            "sampleRate": 16000
        },
    "output_device": "Test Speakers",
    "communications_output_device": "Test Comms Headset",
    "device_error": None,
}


def _wav_body(samples: int = 160) -> bytes:
  buffer = io.BytesIO()
  with wave.open(buffer, "wb") as writer:
    writer.setnchannels(1)
    writer.setsampwidth(2)
    writer.setframerate(16_000)
    writer.writeframes(b"\x00\x00" * samples)
  return buffer.getvalue()


def _upload_file(body: bytes) -> UploadFile:
  return UploadFile(file=io.BytesIO(body), filename="recording.wav")


@pytest.fixture
def endpoint(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """The upload endpoint against a temporary sessions dir and a faked decode."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  monkeypatch.setattr(voice, "get_config", lambda: cfg)
  monkeypatch.setattr(voice, "_speech_bundle", _fake_bundle)
  monkeypatch.setattr(voice, "_transcribe_with_bundle", _fake_decode)
  return cfg


async def _fake_bundle() -> object:
  return object()


async def _fake_decode(session_id: str, bundle: object, pcm_bytes: bytes) -> str:
  return DECODED_TEXT


async def _post(cfg: CharlieBotConfig, devices: str | None) -> tuple[object, list[dict]]:
  with capture_logs() as logs:
    response = await voice.upload_voice_recording(
        SESSION_ID, audio=_upload_file(_wav_body()), backend=BACKEND, devices=devices)
  return response, logs


@pytest.mark.asyncio
async def test_a_devices_field_is_logged_field_for_field(endpoint) -> None:
  response, logs = await _post(endpoint, json.dumps(DEVICES))

  assert response.body == b'{"text":"synthetic decoded words"}'
  transcribed = [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert len(transcribed) == 1
  assert transcribed[0]["input_device"] == "Test Microphone"
  assert transcribed[0]["capture_settings"] == DEVICES["capture_settings"]
  assert transcribed[0]["output_device"] == "Test Speakers"
  assert transcribed[0]["communications_output_device"] == "Test Comms Headset"
  assert transcribed[0]["device_error"] is None
  # The pre-devices fields are unchanged.
  assert transcribed[0]["backend"] == "local"
  assert transcribed[0]["selected_backend"] == BACKEND
  assert transcribed[0]["transcription_preview"] == DECODED_TEXT


@pytest.mark.asyncio
async def test_an_absent_devices_field_logs_all_five_as_none(endpoint) -> None:
  # The compatibility case: a page loaded before the devices existed keeps its
  # old script across a server restart and sends no devices field.
  response, logs = await _post(endpoint, None)

  assert response.status_code == 200
  transcribed = [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert transcribed[0]["input_device"] is None
  assert transcribed[0]["capture_settings"] is None
  assert transcribed[0]["output_device"] is None
  assert transcribed[0]["communications_output_device"] is None
  assert transcribed[0]["device_error"] is None


@pytest.mark.asyncio
async def test_malformed_devices_json_is_a_400_that_persists_nothing(endpoint, tmp_path: Path) -> None:
  response, logs = await _post(endpoint, "not json at all")

  assert response.status_code == 400
  assert "malformed devices form field" in json.loads(response.body)["error"]
  assert not [entry for entry in logs if entry["event"] == "voice_transcribed"]
  # The 400 landed before the persist step: the sessions dir holds no recording.
  assert list((tmp_path / "home" / "sessions").rglob("*")) == []


@pytest.mark.asyncio
async def test_a_devices_field_with_a_wrong_key_set_is_a_400(endpoint, tmp_path: Path) -> None:
  short = {key: value for key, value in DEVICES.items() if key != "output_device"}

  response, logs = await _post(endpoint, json.dumps(short))

  assert response.status_code == 400
  assert "exactly" in json.loads(response.body)["error"]
  assert not [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert list((tmp_path / "home" / "sessions").rglob("*")) == []

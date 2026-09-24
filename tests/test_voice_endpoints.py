"""Endpoint tests for the record-then-upload voice API (confirm probe + full upload).

The speech bundle is stubbed below transcriber.get_ready_model_paths, so these run
without models; the real-model decode is covered by the local_only suites.
"""

from __future__ import annotations

import io
import threading
import wave
from pathlib import Path

import pytest
from conftest import make_home_config
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.agents import transcriber
from src.api import voice
from src.core.config import CharlieBotConfig


def _wav_body(sample_count: int, rate: int = 16_000) -> bytes:
  buf = io.BytesIO()
  with wave.open(buf, "wb") as wav:
    wav.setnchannels(1)
    wav.setsampwidth(2)
    wav.setframerate(rate)
    wav.writeframes(b"\x01\x02" * sample_count)
  return buf.getvalue()


def _upload_parts(
    sample_count: int,
    fields: dict[str, str] | None = None,
) -> dict:
  """The multipart post body for the full upload: the WAV plus optional fields."""
  parts: dict = {"files": {"audio": ("recording.wav", _wav_body(sample_count), "audio/wav")}}
  if fields:
    parts["data"] = fields
  return parts


@pytest.fixture
def voice_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[TestClient, CharlieBotConfig, list[bytes]]:
  """The voice router over a tmp config, with the speech bundle stubbed ready."""
  cfg = make_home_config(tmp_path)
  bundle = transcriber._SpeechModelBundle(
      recognizer=object(),
      vad_config=object(),
      decode_lock=threading.Lock(),
      engine="sherpa",
      model_id="test",
  )
  decoded: list[bytes] = []

  def fake_decode(_bundle: object, pcm_bytes: bytes) -> str:
    decoded.append(pcm_bytes)
    return "recognized words"

  monkeypatch.setattr(voice, "get_config", lambda: cfg)
  monkeypatch.setattr(transcriber, "get_ready_model_paths", lambda: object())
  monkeypatch.setattr(transcriber, "_get_model_bundle", lambda _cfg, _paths: bundle)
  monkeypatch.setattr(transcriber, "transcribe_pcm_offline", fake_decode)
  app = FastAPI()
  app.include_router(voice.router, prefix="/api/voice")
  client = TestClient(app, raise_server_exceptions=False)
  return client, cfg, decoded


def test_confirm_returns_text_and_persists_nothing(voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, cfg, decoded = voice_env

  response = client.post("/api/voice/session-a/confirm", content=_wav_body(160_000))

  assert response.status_code == 200
  assert response.json() == {"text": "recognized words"}
  assert len(decoded) == 1
  assert not (cfg.sessions_dir / "session-a").exists()


def test_confirm_accepts_exactly_ten_seconds(voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, _cfg, _decoded = voice_env

  response = client.post("/api/voice/session-a/confirm", content=_wav_body(160_000))

  assert response.status_code == 200


def test_confirm_rejects_over_limit_duration(voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, cfg, decoded = voice_env

  response = client.post("/api/voice/session-a/confirm", content=_wav_body(160_001))

  assert response.status_code == 400
  assert "10s" in response.json()["error"]
  assert decoded == []
  assert not (cfg.sessions_dir / "session-a").exists()


def test_confirm_rejects_malformed_body(voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, _cfg, decoded = voice_env

  response = client.post("/api/voice/session-a/confirm", content=b"not a wav at all")

  assert response.status_code == 400
  assert "malformed" in response.json()["error"]
  assert decoded == []


def test_confirm_rejects_wrong_format(voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, _cfg, decoded = voice_env

  response = client.post("/api/voice/session-a/confirm", content=_wav_body(1000, rate=44_100))

  assert response.status_code == 400
  assert "16 kHz mono PCM16" in response.json()["error"]
  assert decoded == []


def test_confirm_maps_models_not_ready_to_503(
    voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]], monkeypatch: pytest.MonkeyPatch) -> None:
  client, cfg, _decoded = voice_env

  def not_ready() -> object:
    raise transcriber.SpeechModelsNotReadyError("speech models are still downloading")

  monkeypatch.setattr(transcriber, "get_ready_model_paths", not_ready)
  response = client.post("/api/voice/session-a/confirm", content=_wav_body(160_000))

  assert response.status_code == 503
  assert "still downloading" in response.json()["error"]
  assert not (cfg.sessions_dir / "session-a").exists()


def test_confirm_maps_decode_failure_to_500(
    voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]], monkeypatch: pytest.MonkeyPatch) -> None:
  client, _cfg, _decoded = voice_env

  def broken(_bundle: object, _pcm: bytes) -> str:
    raise RuntimeError("decode exploded")

  monkeypatch.setattr(transcriber, "transcribe_pcm_offline", broken)
  response = client.post("/api/voice/session-a/confirm", content=_wav_body(160_000))

  assert response.status_code == 500
  assert "decode exploded" in response.json()["error"]


def test_full_upload_persists_then_returns_text(voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, cfg, decoded = voice_env

  response = client.post("/api/voice/session-a", **_upload_parts(48_000))

  assert response.status_code == 200
  assert response.json() == {"text": "recognized words"}
  assert len(decoded) == 1
  voice_dir = cfg.sessions_dir / "session-a" / "voice"
  wav_files = list(voice_dir.glob("*.wav"))
  txt_files = list(voice_dir.glob("*.txt"))
  assert len(wav_files) == 1 and len(txt_files) == 1
  assert txt_files[0].read_text(encoding="utf-8") == "recognized words"
  with wave.open(str(wav_files[0]), "rb") as wav:
    assert (wav.getnchannels(), wav.getsampwidth(), wav.getframerate()) == (1, 2, 16_000)
    assert wav.getnframes() == 48_000


def test_full_upload_decode_failure_leaves_the_wav_on_disk(
    voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]], monkeypatch: pytest.MonkeyPatch) -> None:
  client, cfg, _decoded = voice_env

  def broken(_bundle: object, _pcm: bytes) -> str:
    raise RuntimeError("decode exploded")

  monkeypatch.setattr(transcriber, "transcribe_pcm_offline", broken)
  response = client.post("/api/voice/session-a", **_upload_parts(48_000))

  # Persist-before-decode: the 500 still leaves the recording behind.
  assert response.status_code == 500
  assert "error" in response.json()
  voice_dir = cfg.sessions_dir / "session-a" / "voice"
  wav_files = list(voice_dir.glob("*.wav"))
  assert len(wav_files) == 1
  assert list(voice_dir.glob("*.txt")) == []


def test_full_upload_rejects_over_five_minutes(voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, cfg, decoded = voice_env

  response = client.post("/api/voice/session-a", **_upload_parts(transcriber.MAX_RECORDING_SAMPLES + 1))

  assert response.status_code == 400
  assert "300s" in response.json()["error"]
  assert decoded == []
  assert not (cfg.sessions_dir / "session-a").exists()


def test_full_upload_models_not_ready_persists_nothing(
    voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]], monkeypatch: pytest.MonkeyPatch) -> None:
  client, cfg, _decoded = voice_env

  def not_ready() -> object:
    raise transcriber.SpeechModelsNotReadyError("speech models are still downloading")

  monkeypatch.setattr(transcriber, "get_ready_model_paths", not_ready)
  response = client.post("/api/voice/session-a", **_upload_parts(48_000))

  assert response.status_code == 503
  assert not (cfg.sessions_dir / "session-a").exists()


def test_full_upload_accepts_exactly_five_minutes(voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, cfg, _decoded = voice_env

  response = client.post("/api/voice/session-a", **_upload_parts(transcriber.MAX_RECORDING_SAMPLES))

  assert response.status_code == 200
  voice_dir = cfg.sessions_dir / "session-a" / "voice"
  assert len(list(voice_dir.glob("*.wav"))) == 1

# --- The transcript path: the preview relay's final rides the upload ---------


def test_upload_with_transcript_persists_both_files_and_skips_the_decode(
    voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, cfg, decoded = voice_env

  response = client.post(
      "/api/voice/session-a",
      **_upload_parts(48_000, {"transcript": "\u4f60\u597d\u4e16\u754c", "backend": "gemini"}),
  )

  # The transcript is written verbatim and returned unchanged: the input box
  # text and the persisted .txt hold the same words.
  assert response.status_code == 200
  assert response.json() == {"text": "\u4f60\u597d\u4e16\u754c"}
  assert decoded == []  # no speech-model decode ran
  voice_dir = cfg.sessions_dir / "session-a" / "voice"
  wav_files = list(voice_dir.glob("*.wav"))
  txt_files = list(voice_dir.glob("*.txt"))
  assert len(wav_files) == 1 and len(txt_files) == 1
  assert txt_files[0].read_text(encoding="utf-8") == "\u4f60\u597d\u4e16\u754c"
  with wave.open(str(wav_files[0]), "rb") as wav:
    assert wav.getnframes() == 48_000


def test_upload_with_transcript_needs_no_speech_model_readiness(
    voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]], monkeypatch: pytest.MonkeyPatch) -> None:
  client, cfg, _decoded = voice_env

  def not_ready() -> object:
    raise transcriber.SpeechModelsNotReadyError("speech models are still downloading")

  monkeypatch.setattr(transcriber, "get_ready_model_paths", not_ready)
  response = client.post(
      "/api/voice/session-a", **_upload_parts(48_000, {"transcript": "relay words", "backend": "muse"}))

  assert response.status_code == 200
  assert response.json() == {"text": "relay words"}
  voice_dir = cfg.sessions_dir / "session-a" / "voice"
  assert [p.read_text(encoding="utf-8") for p in voice_dir.glob("*.txt")] == ["relay words"]


def test_upload_transcript_log_line_carries_the_backend(
    voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]], monkeypatch: pytest.MonkeyPatch) -> None:
  client, _cfg, _decoded = voice_env
  events: list[tuple[str, dict]] = []

  class _Recorder:
    def info(self, event: str, **fields: object) -> None:
      events.append((event, fields))

  monkeypatch.setattr(voice, "log", _Recorder())
  response = client.post(
      "/api/voice/session-a", **_upload_parts(48_000, {"transcript": "relay words", "backend": "gemini"}))

  assert response.status_code == 200
  assert events[0][0] == "voice_transcribed"
  assert events[0][1]["backend"] == "gemini"


def test_upload_without_transcript_logs_no_backend(
    voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]], monkeypatch: pytest.MonkeyPatch) -> None:
  client, _cfg, _decoded = voice_env
  events: list[tuple[str, dict]] = []

  class _Recorder:
    def info(self, event: str, **fields: object) -> None:
      events.append((event, fields))

  monkeypatch.setattr(voice, "log", _Recorder())
  response = client.post("/api/voice/session-a", **_upload_parts(48_000))

  assert response.status_code == 200
  assert events[0][1]["backend"] is None  # today's decode path names no backend


def test_upload_with_transcript_rejects_a_bad_wav_before_persisting(
    voice_env: tuple[TestClient, CharlieBotConfig, list[bytes]]) -> None:
  client, cfg, _decoded = voice_env

  response = client.post(
      "/api/voice/session-a",
      files={"audio": ("recording.wav", b"not a wav at all", "audio/wav")},
      data={"transcript": "relay words"},
  )

  # The same validation the raw body ever had: a 400 persists nothing.
  assert response.status_code == 400
  assert "malformed" in response.json()["error"]
  assert not (cfg.sessions_dir / "session-a").exists()

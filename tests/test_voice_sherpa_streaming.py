from __future__ import annotations

import concurrent.futures
import time
import unicodedata
import wave
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

import server
from src.agents import transcriber
from src.api import voice
from src.core.config import CharlieBotConfig

SAMPLE_WAV = Path(
    "/home/chaoli/.charliebot/sessions/8ad514a7-e599-4ce2-ad17-2ab351362e3b/acceptance_sample.wav"
)
GROUNDTRUTH_TXT = Path(
    "/home/chaoli/.charliebot/sessions/8ad514a7-e599-4ce2-ad17-2ab351362e3b/acceptance_sample_groundtruth.txt"
)
AUDIO_DURATION_SECONDS = 48.06
CHUNK_SAMPLES = 2048

# Both tests stream the host's acceptance voice sample through locally cached
# sherpa-onnx speech models; nothing they need exists on a CI runner.
pytestmark = pytest.mark.local_only


def _require_voice_assets(cfg: CharlieBotConfig) -> None:
  if not SAMPLE_WAV.is_file() or not GROUNDTRUTH_TXT.is_file():
    pytest.skip("acceptance voice sample is not present on this host")
  if not transcriber.models_are_cached(cfg):
    pytest.skip("speech models are not present locally")
  transcriber.ensure_models_cached(cfg)


def _pcm_chunks(path: Path, chunk_samples: int = CHUNK_SAMPLES) -> list[bytes]:
  with wave.open(str(path), "rb") as wav:
    assert wav.getnchannels() == 1
    assert wav.getframerate() == transcriber.SAMPLE_RATE
    assert wav.getsampwidth() == 2
    chunks: list[bytes] = []
    while True:
      data = wav.readframes(chunk_samples)
      if not data:
        return chunks
      chunks.append(data)


def _normalize_for_cer(text: str) -> str:
  chars: list[str] = []
  for ch in text.casefold():
    category = unicodedata.category(ch)
    if category.startswith("P") or category.startswith("S") or ch.isspace():
      continue
    chars.append(ch)
  return "".join(chars)


def _edit_distance(a: str, b: str) -> int:
  previous = list(range(len(b) + 1))
  for i, ca in enumerate(a, start=1):
    current = [i]
    for j, cb in enumerate(b, start=1):
      current.append(
          min(
              previous[j] + 1,
              current[j - 1] + 1,
              previous[j - 1] + (ca != cb),
          ))
    previous = current
  return previous[-1]


def _character_error_rate(hypothesis: str, reference: str) -> float:
  normalized_hypothesis = _normalize_for_cer(hypothesis)
  normalized_reference = _normalize_for_cer(reference)
  assert normalized_reference
  return _edit_distance(normalized_hypothesis, normalized_reference) / len(normalized_reference)


def test_sherpa_simulated_streaming_acceptance_baseline() -> None:
  cfg = CharlieBotConfig()
  _require_voice_assets(cfg)

  session = transcriber.create_transcription_session(cfg)
  started = time.perf_counter()
  for chunk in _pcm_chunks(SAMPLE_WAV):
    session.accept_pcm(chunk)
  final_text = session.finish()
  wall_seconds = time.perf_counter() - started

  assert final_text
  cer = _character_error_rate(final_text, GROUNDTRUTH_TXT.read_text(encoding="utf-8"))
  rtf = wall_seconds / AUDIO_DURATION_SECONDS
  print(f"Final text: {final_text}")
  print(f"CER: {cer:.4f}")
  print(f"RTF: {rtf:.4f}")


def test_voice_websocket_streams_partials_final_and_persists_dump(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  cfg.charliebot_home.mkdir(parents=True)
  model_cache = Path.home() / ".charliebot" / "models"
  if not model_cache.exists():
    pytest.skip("speech models are not present locally")
  (cfg.charliebot_home / "models").symlink_to(model_cache, target_is_directory=True)
  _require_voice_assets(cfg)

  monkeypatch.setattr(server, "get_config", lambda: cfg)
  monkeypatch.setattr(voice, "get_config", lambda: cfg)

  session_id = "voice-ws-test"
  messages: list[dict] = []

  def receive_until_final(ws) -> None:
    while True:
      message = ws.receive_json()
      messages.append(message)
      if message["type"] == "final":
        return
      if message["type"] == "error":
        raise AssertionError(message["text"])

  client = TestClient(server.app)
  with client.websocket_connect(f"/ws/voice/{session_id}") as ws:
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
      future = executor.submit(receive_until_final, ws)
      for chunk in _pcm_chunks(SAMPLE_WAV):
        ws.send_bytes(chunk)
      ws.send_text('{"type": "stop"}')
      future.result(timeout=120)

  types = [message["type"] for message in messages]
  assert "partial" in types
  assert types.count("final") == 1
  final_text = messages[-1]["text"]
  assert final_text

  voice_dir = cfg.sessions_dir / session_id / "voice"
  wav_files = list(voice_dir.glob("*.wav"))
  txt_files = list(voice_dir.glob("*.txt"))
  assert len(wav_files) == 1
  assert len(txt_files) == 1
  assert txt_files[0].read_text(encoding="utf-8") == final_text
  with wave.open(str(wav_files[0]), "rb") as wav:
    assert wav.getnchannels() == 1
    assert wav.getframerate() == transcriber.SAMPLE_RATE
    assert wav.getsampwidth() == 2

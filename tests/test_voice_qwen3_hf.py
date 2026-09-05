"""End-to-end qwen3_hf GPU decode test (local_only: needs the GPU host's weights + CUDA)."""

from __future__ import annotations

import concurrent.futures
import threading
import time
import wave

import numpy as np
import pytest

import src.core.voice_setup as voice_setup
from src.agents import transcriber
from src.core.config import CharlieBotConfig

pytestmark = pytest.mark.local_only

CHUNK_SAMPLES = 2048


@pytest.fixture(autouse=True)
def reset_bundle_cache():
  """Clear the process-wide bundle cache around each test so the GPU bundle stays local."""
  with transcriber._state_lock:
    transcriber._bundle = None
    transcriber._bundle_engine = None
  yield
  with transcriber._state_lock:
    transcriber._bundle = None
    transcriber._bundle_engine = None


def _require_gpu_assets(cfg: CharlieBotConfig) -> None:
  torch = pytest.importorskip("torch")
  if not torch.cuda.is_available():
    pytest.skip("no CUDA device on this host")
  if not transcriber.models_are_cached(cfg):
    pytest.skip("qwen3_hf weights are not present locally")
  transcriber.ensure_models_cached(cfg)


def _load_wav_frames(path) -> bytes:
  with wave.open(str(path), "rb") as wav:
    assert wav.getnchannels() == 1
    assert wav.getframerate() == transcriber.SAMPLE_RATE
    assert wav.getsampwidth() == 2
    return wav.readframes(wav.getnframes())


def test_qwen3_hf_gpu_decode_real_recording() -> None:
  """The official transformers weights decode a real recording on cuda; text comes out."""
  cfg = CharlieBotConfig(voice_engine="qwen3_hf")
  _require_gpu_assets(cfg)

  wav_path = voice_setup.pick_preflight_recording(cfg.sessions_dir)
  with wave.open(str(wav_path), "rb") as wav:
    audio_seconds = wav.getnframes() / wav.getframerate()
  bundle = transcriber.create_qwen3_hf_bundle(cfg, transcriber.get_ready_model_paths())
  samples = np.frombuffer(_load_wav_frames(wav_path), dtype="<i2").astype(np.float32) / 32768.0

  started = time.perf_counter()
  text = transcriber._decode_samples(bundle, samples)
  decode_seconds = time.perf_counter() - started

  assert text
  print(f"Engine: {bundle.engine} model: {bundle.model_id}")
  print(f"Audio: {wav_path} ({audio_seconds:.1f}s)")
  print(f"Decode: {decode_seconds:.3f}s (RTF {decode_seconds / audio_seconds:.4f})")
  print(f"Text: {text}")


def test_qwen3_hf_gpu_session_streams_and_finalizes() -> None:
  """The production session path (VAD + partials + finish) runs on the GPU engine."""
  cfg = CharlieBotConfig(voice_engine="qwen3_hf")
  _require_gpu_assets(cfg)

  wav_path = voice_setup.pick_preflight_recording(cfg.sessions_dir)
  session = transcriber.create_transcription_session(cfg)
  assert session._bundle.engine == "qwen3_hf"

  with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:

    def feed() -> str:
      with wave.open(str(wav_path), "rb") as wav:
        while True:
          data = wav.readframes(CHUNK_SAMPLES)
          if not data:
            break
          session.accept_pcm(data)
      return session.finish()

    final_text = executor.submit(feed).result(timeout=120)

  if session._saw_speech:
    assert final_text
    print(f"Final text: {final_text}")

"""End-to-end qwen3_hf GPU decode test (local_only: needs the GPU host's weights + CUDA)."""

from __future__ import annotations

import time
import wave
from pathlib import Path

import numpy as np
import pytest
from conftest import fresh_state_fixture, voice_models_cached

from src.agents import transcriber
from src.core import voice_setup
from src.core.config import CharlieBotConfig

pytestmark = pytest.mark.local_only

reset_bundle_cache = fresh_state_fixture(transcriber.reset_bundle_cache_for_tests)


def _require_gpu_assets(cfg: CharlieBotConfig) -> None:
  torch = pytest.importorskip("torch")
  if not torch.cuda.is_available():
    pytest.skip("no CUDA device on this host")
  if not voice_models_cached(cfg):
    pytest.skip("qwen3_hf weights are not present locally")
  transcriber.ensure_models_cached(cfg)


def _load_wav_frames(path: Path) -> bytes:
  with wave.open(str(path), "rb") as wav:
    assert wav.getnchannels() == 1
    assert wav.getframerate() == transcriber.SAMPLE_RATE
    assert wav.getsampwidth() == 2
    return wav.readframes(wav.getnframes())


def test_qwen3_hf_gpu_decode_real_recording() -> None:
  """The official transformers weights decode a real recording on cuda; text comes out."""
  cfg = CharlieBotConfig(voice={"engine": "qwen3_hf"})
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


def test_qwen3_hf_gpu_offline_decode_meets_latency_budget() -> None:
  """The offline path (VAD segmentation + one decode per segment) runs on the GPU
  engine, and a recording within the 25.4 s single-decode-window bound returns its
  full text inside the plan's 5 s budget on the healthy-GPU host."""
  cfg = CharlieBotConfig(voice={"engine": "qwen3_hf"})
  _require_gpu_assets(cfg)

  wav_path = voice_setup.pick_preflight_recording(cfg.sessions_dir)
  bundle = transcriber.get_transcription_bundle(cfg)
  assert bundle.engine == "qwen3_hf"

  with wave.open(str(wav_path), "rb") as wav:
    pcm = wav.readframes(wav.getnframes())
  audio_seconds = len(pcm) / 2 / transcriber.SAMPLE_RATE
  assert audio_seconds <= 25.4

  started = time.perf_counter()
  text = transcriber.transcribe_pcm_offline(bundle, pcm)
  decode_seconds = time.perf_counter() - started

  assert text
  print(f"Engine: {bundle.engine} model: {bundle.model_id}")
  print(f"Audio: {wav_path} ({audio_seconds:.1f}s)")
  print(f"Decode: {decode_seconds:.3f}s (RTF {decode_seconds / audio_seconds:.4f})")
  print(f"Text: {text}")
  assert decode_seconds < 5.0, (
      f"{audio_seconds:.1f}s of audio decoded in {decode_seconds:.3f}s; the plan's "
      "healthy-GPU budget is full text within 5 s for recordings up to 25.4 s")

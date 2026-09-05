"""Transcriber engine-branch tests: default sherpa, explicit qwen3_hf, GPU-failure fallback.

The GPU bundle factory is mocked in every test here — the real GPU path runs under the
local_only marker in test_voice_qwen3_hf.py. The module-level bundle cache is reset
around each test so a stub bundle never leaks into the streaming or websocket suites.
"""

from __future__ import annotations

import threading

import pytest
from structlog.testing import capture_logs

from src.agents import transcriber
from src.core.config import CharlieBotConfig


@pytest.fixture(autouse=True)
def reset_bundle_cache():
  """Clear the process-wide bundle cache before and after each test in this file."""
  with transcriber._state_lock:
    transcriber._bundle = None
    transcriber._bundle_engine = None
  yield
  with transcriber._state_lock:
    transcriber._bundle = None
    transcriber._bundle_engine = None


def _stub_bundle(engine: str, model_id: str) -> transcriber._SpeechModelBundle:
  return transcriber._SpeechModelBundle(
      recognizer=object(), vad_config=object(), decode_lock=threading.Lock(), engine=engine, model_id=model_id)


def test_default_engine_builds_sherpa_bundle(monkeypatch: pytest.MonkeyPatch) -> None:
  """voice_engine unset: the sherpa factory builds the bundle and the GPU one is never touched."""
  cfg = CharlieBotConfig()
  sherpa = _stub_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  calls: list[str] = []

  def fail_gpu(*_args):
    raise AssertionError("create_qwen3_hf_bundle must not run for the default engine")

  monkeypatch.setattr(transcriber, "create_sherpa_bundle", lambda paths: calls.append("sherpa") or sherpa)
  monkeypatch.setattr(transcriber, "create_qwen3_hf_bundle", fail_gpu)

  bundle = transcriber._get_model_bundle(cfg, transcriber.voice_model_paths(cfg))

  assert bundle is sherpa
  assert bundle.engine == "sherpa"
  assert calls == ["sherpa"]


def test_qwen3_hf_engine_builds_gpu_bundle(monkeypatch: pytest.MonkeyPatch) -> None:
  """voice_engine=qwen3_hf: the GPU factory builds the bundle carrying the config's model id."""
  cfg = CharlieBotConfig(voice_engine="qwen3_hf", voice_model_id="Qwen/Qwen3-ASR-0.6B-hf")
  gpu = _stub_bundle("qwen3_hf", cfg.voice_model_id)
  received: list[tuple] = []

  def fake_gpu(received_cfg, paths):
    received.append((received_cfg, paths))
    return gpu

  monkeypatch.setattr(transcriber, "create_qwen3_hf_bundle", fake_gpu)
  monkeypatch.setattr(
      transcriber, "create_sherpa_bundle", lambda *_:
      (_ for _ in ()).throw(AssertionError("sherpa factory must not run")))

  paths = transcriber.voice_model_paths(cfg)
  bundle = transcriber._get_model_bundle(cfg, paths)

  assert bundle is gpu
  assert received == [(cfg, paths)]


def test_gpu_engine_failure_falls_back_to_sherpa_with_warning(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
  """A GPU init error warns and decodes on the CPU engine instead of failing the session."""
  cfg = CharlieBotConfig(voice_engine="qwen3_hf", charliebot_home=tmp_path)
  sherpa = _stub_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  ensured: list[CharlieBotConfig] = []

  def fail_gpu(*_args):
    raise RuntimeError("no CUDA GPU is available")

  monkeypatch.setattr(transcriber, "create_qwen3_hf_bundle", fail_gpu)
  monkeypatch.setattr(transcriber, "create_sherpa_bundle", lambda paths: sherpa)
  monkeypatch.setattr(
      transcriber, "_ensure_sherpa_paths_cached",
      lambda received_cfg: ensured.append(received_cfg) or transcriber.voice_model_paths(received_cfg))

  with capture_logs() as logs:
    bundle = transcriber._get_model_bundle(cfg, transcriber.voice_model_paths(cfg))

  assert bundle is sherpa
  assert bundle.engine == "sherpa"
  assert ensured == [cfg]
  warnings = [event for event in logs if event["event"] == "voice_gpu_engine_init_failed"]
  assert warnings and warnings[0]["fallback"] == "sherpa"
  assert "no CUDA GPU is available" in warnings[0]["error"]
  ready = [event for event in logs if event["event"] == "voice_model_bundle_ready"]
  assert ready and ready[0]["engine"] == "sherpa"
  assert ready[0]["model_id"] == transcriber.QWEN3_ASR_DIR_NAME


def test_gpu_fallback_result_is_cached_under_the_requested_engine(monkeypatch: pytest.MonkeyPatch) -> None:
  """After a fallback build, later sessions reuse it without retrying the GPU engine."""
  cfg = CharlieBotConfig(voice_engine="qwen3_hf")
  sherpa = _stub_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  gpu_calls: list[int] = []
  sherpa_calls: list[int] = []

  def fail_gpu(*_args):
    gpu_calls.append(1)
    raise RuntimeError("missing CUDA driver")

  monkeypatch.setattr(transcriber, "create_qwen3_hf_bundle", fail_gpu)
  monkeypatch.setattr(transcriber, "create_sherpa_bundle", lambda paths: sherpa_calls.append(1) or sherpa)
  monkeypatch.setattr(transcriber, "_ensure_sherpa_paths_cached", lambda cfg_: transcriber.voice_model_paths(cfg_))

  first = transcriber._get_model_bundle(cfg, transcriber.voice_model_paths(cfg))
  second = transcriber._get_model_bundle(cfg, transcriber.voice_model_paths(cfg))

  assert first is second is sherpa
  assert gpu_calls == [1]
  assert sherpa_calls == [1]


def test_bundle_cache_rebuilds_when_engine_changes(monkeypatch: pytest.MonkeyPatch) -> None:
  """A cached sherpa bundle does not answer for a qwen3_hf config; the factories re-run."""
  sherpa = _stub_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  gpu = _stub_bundle("qwen3_hf", "Qwen/Qwen3-ASR-1.7B-hf")
  monkeypatch.setattr(transcriber, "create_sherpa_bundle", lambda paths: sherpa)
  monkeypatch.setattr(transcriber, "create_qwen3_hf_bundle", lambda cfg, paths: gpu)

  sherpa_cfg = CharlieBotConfig()
  gpu_cfg = CharlieBotConfig(voice_engine="qwen3_hf")
  paths = transcriber.voice_model_paths(sherpa_cfg)

  first = transcriber._get_model_bundle(sherpa_cfg, paths)
  second = transcriber._get_model_bundle(gpu_cfg, paths)
  third = transcriber._get_model_bundle(sherpa_cfg, paths)

  assert first is sherpa
  assert second is gpu
  assert third is sherpa

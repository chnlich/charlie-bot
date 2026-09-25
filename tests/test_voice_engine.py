"""Transcriber engine-branch tests: default sherpa, explicit qwen3_hf, GPU-failure fallback.

The GPU bundle factory is mocked in every test here — the real GPU path runs under the
local_only marker in test_voice_qwen3_hf.py. The module-level bundle cache is reset
around each test so a stub bundle never leaks into the other voice suites.
"""

from __future__ import annotations

import threading
import time
from pathlib import Path

import numpy as np
import pytest
from conftest import fresh_state_fixture, stub_speech_bundle
from structlog.testing import capture_logs

from src.agents import transcriber
from src.core.config import CharlieBotConfig

reset_bundle_cache = fresh_state_fixture(transcriber.reset_bundle_cache_for_tests)


def test_default_engine_builds_sherpa_bundle(monkeypatch: pytest.MonkeyPatch) -> None:
  """voice.engine unset: the sherpa factory builds the bundle and the GPU one is never touched."""
  cfg = CharlieBotConfig()
  sherpa = stub_speech_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  calls: list[str] = []

  def fail_gpu(*_args: object) -> None:
    raise AssertionError("create_qwen3_hf_bundle must not run for the default engine")

  monkeypatch.setattr(transcriber, "create_sherpa_bundle", lambda paths: calls.append("sherpa") or sherpa)
  monkeypatch.setattr(transcriber, "create_qwen3_hf_bundle", fail_gpu)

  bundle = transcriber._get_model_bundle(cfg, transcriber.voice_model_paths(cfg))

  assert bundle is sherpa
  assert bundle.engine == "sherpa"
  assert calls == ["sherpa"]


def test_qwen3_hf_engine_builds_gpu_bundle(monkeypatch: pytest.MonkeyPatch) -> None:
  """voice.engine=qwen3_hf: the GPU factory builds the bundle carrying the config's model id."""
  cfg = CharlieBotConfig(voice={"engine": "qwen3_hf", "model_id": "Qwen/Qwen3-ASR-0.6B-hf"})
  gpu = stub_speech_bundle("qwen3_hf", cfg.voice.model_id)
  received: list[tuple] = []

  def fake_gpu(received_cfg: CharlieBotConfig, paths: transcriber.VoiceModelPaths) -> transcriber._SpeechModelBundle:
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


def test_gpu_engine_failure_falls_back_to_sherpa_with_warning(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """A GPU init error warns and decodes on the CPU engine instead of failing the session."""
  cfg = CharlieBotConfig(voice={"engine": "qwen3_hf"}, charliebot_home=tmp_path)
  sherpa = stub_speech_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  ensured: list[CharlieBotConfig] = []

  def fail_gpu(*_args: object) -> None:
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
  cfg = CharlieBotConfig(voice={"engine": "qwen3_hf"})
  sherpa = stub_speech_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  gpu_calls: list[int] = []
  sherpa_calls: list[int] = []

  def fail_gpu(*_args: object) -> None:
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
  sherpa = stub_speech_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  gpu = stub_speech_bundle("qwen3_hf", "Qwen/Qwen3-ASR-1.7B-hf")
  monkeypatch.setattr(transcriber, "create_sherpa_bundle", lambda paths: sherpa)
  monkeypatch.setattr(transcriber, "create_qwen3_hf_bundle", lambda cfg, paths: gpu)

  sherpa_cfg = CharlieBotConfig()
  gpu_cfg = CharlieBotConfig(voice={"engine": "qwen3_hf"})
  paths = transcriber.voice_model_paths(sherpa_cfg)

  first = transcriber._get_model_bundle(sherpa_cfg, paths)
  second = transcriber._get_model_bundle(gpu_cfg, paths)
  third = transcriber._get_model_bundle(sherpa_cfg, paths)

  assert first is sherpa
  assert second is gpu
  assert third is sherpa


def test_concurrent_first_access_builds_exactly_once(monkeypatch: pytest.MonkeyPatch) -> None:
  """Two first accesses racing a slow build: the build runs once and both getters share its bundle."""
  cfg = CharlieBotConfig()
  sherpa = stub_speech_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  builds: list[str] = []
  inside_build = threading.Event()

  def slow_build(_paths: transcriber.VoiceModelPaths) -> transcriber._SpeechModelBundle:
    builds.append("build")
    inside_build.set()
    time.sleep(0.2)
    return sherpa

  monkeypatch.setattr(transcriber, "create_sherpa_bundle", slow_build)

  results: list[transcriber._SpeechModelBundle] = []
  failures: list[BaseException] = []

  def getter() -> None:
    try:
      results.append(transcriber._get_model_bundle(cfg, transcriber.voice_model_paths(cfg)))
    except BaseException as exc:
      failures.append(exc)

  first = threading.Thread(target=getter)
  first.start()
  assert inside_build.wait(timeout=10), "the first getter never entered the build"
  second = threading.Thread(target=getter)
  second.start()
  first.join(timeout=10)
  second.join(timeout=10)

  assert failures == []
  assert builds == ["build"]
  assert results == [sherpa, sherpa]
  assert results[0] is results[1] is sherpa


def test_build_lock_recheck_honors_a_cache_published_before_acquisition(monkeypatch: pytest.MonkeyPatch) -> None:
  """A bundle published between the outer miss and the build-lock acquisition wins: no second build."""
  cfg = CharlieBotConfig()
  winner = stub_speech_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  loser = stub_speech_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  builds: list[str] = []
  real_lock = transcriber._bundle_build_lock

  def fail_build(_paths: transcriber.VoiceModelPaths) -> transcriber._SpeechModelBundle:
    builds.append("build")
    return loser

  class PublishThenLock:
    """Simulates the winner's build publishing before this getter acquires the build lock."""

    def __enter__(self) -> None:
      with transcriber._state_lock:
        if transcriber._bundle is None:
          transcriber._bundle = winner
          transcriber._bundle_engine = cfg.voice.engine
      real_lock.acquire()

    def __exit__(self, *_exc: object) -> None:
      real_lock.release()

  monkeypatch.setattr(transcriber, "_bundle_build_lock", PublishThenLock())
  monkeypatch.setattr(transcriber, "create_sherpa_bundle", fail_build)

  bundle = transcriber._get_model_bundle(cfg, transcriber.voice_model_paths(cfg))

  assert bundle is winner
  assert builds == []


def test_prepopulated_cache_yields_zero_builds(monkeypatch: pytest.MonkeyPatch) -> None:
  """A cache already holding the engine's bundle answers without running any factory."""
  cfg = CharlieBotConfig()
  cached = stub_speech_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  with transcriber._state_lock:
    transcriber._bundle = cached
    transcriber._bundle_engine = cfg.voice.engine
  monkeypatch.setattr(
      transcriber, "create_sherpa_bundle", lambda *_:
      (_ for _ in ()).throw(AssertionError("the factory must not run on a cache hit")))

  assert transcriber._get_model_bundle(cfg, transcriber.voice_model_paths(cfg)) is cached


def test_warm_up_bundle_decodes_exactly_one_deterministic_sine(monkeypatch: pytest.MonkeyPatch) -> None:
  """The warm decode is one _decode_samples call over a deterministic 0.5 s sine; the text is dropped."""
  bundle = stub_speech_bundle("sherpa", transcriber.QWEN3_ASR_DIR_NAME)
  calls: list[np.ndarray] = []

  def fake_decode(received_bundle: transcriber._SpeechModelBundle, samples: np.ndarray) -> str:
    calls.append(samples)
    assert received_bundle is bundle
    return "transcript nobody reads"

  monkeypatch.setattr(transcriber, "_decode_samples", fake_decode)

  assert transcriber.warm_up_bundle(bundle) is None
  assert transcriber.warm_up_bundle(bundle) is None

  assert len(calls) == 2, "each warm-up decodes exactly once"
  first, second = calls
  assert first.dtype == np.float32
  assert first.size == int(transcriber.SAMPLE_RATE * transcriber.WARMUP_SECONDS) == 8000
  assert np.all(np.abs(first) <= 1.0), "the sine must stay inside the float decode range"
  assert np.array_equal(first, second), "the warm input must be deterministic"

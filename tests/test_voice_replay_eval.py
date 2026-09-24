"""Voice replay evaluation harness tests: window parity, ground truth, Muse client, scoring.

The harness (scripts/voice_replay_eval.py) replays recorded dictations through the
local engine and Meta's Muse realtime API. These tests never touch real recordings
or the network beyond a loopback fake Muse server, and build all fixtures from
synthetic PCM (silence and sine tones) under tmp_path.
"""

from __future__ import annotations

import importlib.util
import sys
import threading
from pathlib import Path

import numpy as np
import pytest

from src.agents import transcriber

ROOT = Path(__file__).resolve().parents[1]


def _load_script():
  """Import scripts/voice_replay_eval.py as a module (it is an entry point, not a package)."""
  path = ROOT / "scripts" / "voice_replay_eval.py"
  spec = importlib.util.spec_from_file_location("voice_replay_eval", path)
  module = importlib.util.module_from_spec(spec)
  sys.modules["voice_replay_eval"] = module
  spec.loader.exec_module(module)
  return module


class _FakeSegment:

  def __init__(self, start: int, length: int) -> None:
    self.start = start
    self.samples = np.zeros(length, dtype=np.float32)


class _FakeVad:
  """The fake-VAD seam from test_transcriber_sampling: pre-set segments, no models."""

  def __init__(self, segments: list[tuple[int, int]]) -> None:
    self._segments = [_FakeSegment(start, length) for start, length in segments]

  def accept_waveform(self, samples: np.ndarray) -> None:
    pass

  def flush(self) -> None:
    pass

  def empty(self) -> bool:
    return not self._segments

  @property
  def front(self) -> _FakeSegment:
    return self._segments[0]

  def pop(self) -> None:
    self._segments.pop(0)


def _stub_bundle() -> transcriber._SpeechModelBundle:
  return transcriber._SpeechModelBundle(
      recognizer=object(), vad_config=object(), decode_lock=threading.Lock(), engine="sherpa", model_id="test")


def _install_fakes(
    monkeypatch: pytest.MonkeyPatch,
    segments: list[tuple[int, int]],
    decode_texts: list[str],
) -> tuple[list[np.ndarray], list[str]]:
  """Stub the VAD and decoder; return (captured decode windows, per-call texts)."""
  captured: list[np.ndarray] = []
  texts: list[str] = []

  def fake_open_vad(_config: object, _buffer_seconds: float) -> _FakeVad:
    return _FakeVad(segments)

  def fake_decode(_bundle: object, samples: np.ndarray) -> str:
    captured.append(samples.copy())
    return decode_texts[len(captured) - 1]

  monkeypatch.setattr(transcriber, "_open_vad", fake_open_vad)
  monkeypatch.setattr(transcriber, "_decode_samples", fake_decode)
  return captured, texts


def test_offline_windows_parity_with_transcribe_pcm_offline(monkeypatch: pytest.MonkeyPatch) -> None:
  """transcribe_pcm_offline equals joining per-window decodes from offline_decode_windows."""
  source = np.arange(200_000, dtype="<i2").astype(np.int16)
  segments = [(100_000, 10_000), (130_000, 20_000)]
  decode_texts = ["first", "second"]
  captured, _texts = _install_fakes(monkeypatch, segments, decode_texts)

  production_text = transcriber.transcribe_pcm_offline(_stub_bundle(), source.tobytes())
  assert production_text == "first second"
  production_windows = [c.copy() for c in captured]

  # The replay pass: fresh VAD over the same samples, decode each window the
  # extracted function returns, join exactly as production does.
  captured.clear()
  vad = _FakeVad(segments)
  windows = transcriber.offline_decode_windows(vad, source)
  replay_text = transcriber._join_segments(
      *(transcriber._decode_samples(_stub_bundle(), source[left:right].astype(np.float32) / 32768.0)
        for _start, _end, left, right in windows))
  assert replay_text == production_text
  # Both passes decoded byte-identical spans, in order.
  assert len(captured) == len(production_windows)
  for replay_window, production_window in zip(captured, production_windows, strict=True):
    np.testing.assert_array_equal(replay_window, production_window)


def test_offline_windows_report_segment_and_window_bounds(monkeypatch: pytest.MonkeyPatch) -> None:
  source = np.zeros(200_000, dtype="<i2")
  _install_fakes(monkeypatch, [(100_000, 10_000), (130_000, 20_000)], [])

  windows = transcriber.offline_decode_windows(_FakeVad([(100_000, 10_000), (130_000, 20_000)]), source)

  pause, pad = transcriber.SEGMENT_DECODE_PAUSE_SAMPLES, transcriber.SEGMENT_DECODE_PAD_SAMPLES
  assert windows[0] == (100_000, 110_000, 100_000 - pause, 110_000 + pad)
  # Window 2's left edge clips against window 1's right edge: no overlap, no replay.
  assert windows[1] == (130_000, 150_000, 110_000 + pad, 150_000 + pad)

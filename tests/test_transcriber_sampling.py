"""Offline transcription windowing tests: fake VAD segments drive the arithmetic.

The real VAD/decoder run under the local_only suites (test_voice_offline_models.py,
test_voice_qwen3_hf.py); here the bundle is a stub and the segments are pre-set, so
the feed cadence, window padding, and concatenation rules hold without models.
"""

from __future__ import annotations

import threading

import numpy as np
import pytest

from src.agents import transcriber


class _FakeSegment:

  def __init__(self, start: int, length: int) -> None:
    self.start = start
    self.samples = np.zeros(length, dtype=np.float32)


class _FakeVad:

  def __init__(self, segments: list[tuple[int, int]]) -> None:
    self._segments = [_FakeSegment(start, length) for start, length in segments]
    self.feed_sizes: list[int] = []
    self.flushed = False

  def accept_waveform(self, samples: np.ndarray) -> None:
    self.feed_sizes.append(samples.size)

  def flush(self) -> None:
    self.flushed = True

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
) -> tuple[list[np.ndarray], _FakeVad]:
  vad = _FakeVad(segments)
  captured: list[np.ndarray] = []

  def fake_open_vad(_config: object, _buffer_seconds: float) -> _FakeVad:
    return vad

  def fake_decode(_bundle: object, samples: np.ndarray) -> str:
    captured.append(samples.copy())
    return decode_texts[len(captured) - 1]

  monkeypatch.setattr(transcriber, "_open_vad", fake_open_vad)
  monkeypatch.setattr(transcriber, "_decode_samples", fake_decode)
  return captured, vad


def test_offline_feeds_vad_in_128ms_steps_and_flushes(monkeypatch: pytest.MonkeyPatch) -> None:
  _captured, vad = _install_fakes(monkeypatch, [], [])
  seconds = 5
  pcm = np.zeros(seconds * transcriber.SAMPLE_RATE, dtype="<i2").tobytes()

  transcriber.transcribe_pcm_offline(_stub_bundle(), pcm)

  assert vad.flushed
  # 80000 samples = 39 full 128 ms steps plus a 128-sample tail.
  assert vad.feed_sizes == [transcriber.OFFLINE_VAD_FEED_SAMPLES] * 39 + [128]
  assert all(size <= transcriber.OFFLINE_VAD_FEED_SAMPLES for size in vad.feed_sizes)


def test_offline_feed_tail_is_the_remainder(monkeypatch: pytest.MonkeyPatch) -> None:
  _captured, vad = _install_fakes(monkeypatch, [], [])
  # 2 full 128 ms steps plus a 100 ms remainder.
  pcm = np.zeros(2 * transcriber.OFFLINE_VAD_FEED_SAMPLES + 1600, dtype="<i2").tobytes()

  transcriber.transcribe_pcm_offline(_stub_bundle(), pcm)

  assert vad.feed_sizes == [2048, 2048, 1600]


def test_offline_decodes_padded_windows_without_overlap(monkeypatch: pytest.MonkeyPatch) -> None:
  source = np.arange(200_000, dtype="<i2").astype(np.int16)
  # Two segments; the second opens after the first's decode window has closed.
  captured, _vad = _install_fakes(
      monkeypatch,
      segments=[(100_000, 10_000), (130_000, 20_000)],
      decode_texts=["first", "second"],
  )

  text = transcriber.transcribe_pcm_offline(_stub_bundle(), source.tobytes())

  assert text == "first second"
  pause, pad = transcriber.SEGMENT_DECODE_PAUSE_SAMPLES, transcriber.SEGMENT_DECODE_PAD_SAMPLES
  # Window 1: 5 s of pause before the segment, 0.4 s of tail after, clamped at 0.
  np.testing.assert_array_equal(captured[0], source[20_000:116_400].astype(np.float32) / 32768.0)
  # Window 2: the left edge clips against window 1's right edge, so the padded
  # windows never overlap or replay audio.
  np.testing.assert_array_equal(captured[1], source[116_400:156_400].astype(np.float32) / 32768.0)
  assert pause == 5 * transcriber.SAMPLE_RATE and pad == 6_400


def test_offline_windows_clamp_to_the_recording_bounds(monkeypatch: pytest.MonkeyPatch) -> None:
  source = np.arange(30_000, dtype="<i2").astype(np.int16)
  # The segment sits at the very start and its pad runs past the recording's end.
  captured, _vad = _install_fakes(monkeypatch, [(0, 10_000)], ["only"])

  transcriber.transcribe_pcm_offline(_stub_bundle(), source.tobytes())

  np.testing.assert_array_equal(captured[0], source[0:16_400].astype(np.float32) / 32768.0)


def test_offline_skips_empty_segment_texts(monkeypatch: pytest.MonkeyPatch) -> None:
  source = np.zeros(40_000, dtype="<i2")
  _captured, _vad = _install_fakes(
      monkeypatch,
      segments=[(0, 10_000), (20_000, 10_000)],
      decode_texts=["", "kept"],
  )

  assert transcriber.transcribe_pcm_offline(_stub_bundle(), source.tobytes()) == "kept"


def test_offline_without_speech_decodes_nothing(monkeypatch: pytest.MonkeyPatch) -> None:
  captured, vad = _install_fakes(monkeypatch, [], [])

  assert transcriber.transcribe_pcm_offline(_stub_bundle(), np.zeros(16_000, dtype="<i2").tobytes()) == ""

  assert captured == []
  assert vad.flushed


def test_offline_odd_pcm_length_raises(monkeypatch: pytest.MonkeyPatch) -> None:
  _install_fakes(monkeypatch, [], [])

  with pytest.raises(ValueError, match="byte length must be even"):
    transcriber.transcribe_pcm_offline(_stub_bundle(), b"\x00\x00\x00")

from __future__ import annotations

import numpy as np

from src.agents import transcriber


class _LengthOnlySamples:

  def __init__(self, length: int) -> None:
    self._length = length

  def __len__(self) -> int:
    return self._length

  def __array__(self, *args, **kwargs):
    raise AssertionError("segment.samples must not be decoded")


class _ClosedSegment:

  def __init__(self, start: int, length: int) -> None:
    self.start = start
    self.samples = _LengthOnlySamples(length)


class _ClosedVad:

  def __init__(self, segment: _ClosedSegment) -> None:
    self._segment = segment
    self._empty = False

  def empty(self) -> bool:
    return self._empty

  @property
  def front(self) -> _ClosedSegment:
    return self._segment

  def pop(self) -> None:
    self._empty = True


class _LiveSegment:

  def __init__(self, start: int) -> None:
    self.start = start


class _LiveVad:

  def __init__(self, segment: _LiveSegment) -> None:
    self.current_segment = segment

  def is_speech_detected(self) -> bool:
    return True


def _session_with_pcm(samples: np.ndarray) -> transcriber.SimulatedStreamingTranscriptionSession:
  session = transcriber.SimulatedStreamingTranscriptionSession.__new__(
      transcriber.SimulatedStreamingTranscriptionSession)
  session._bundle = object()
  session._vad = None
  session._pcm_chunks = [samples.astype("<i2").tobytes()]
  session._frozen_segments = []
  session._fed_samples = samples.size
  session._last_partial = ""
  session._last_live_text = ""
  session._last_live_decode_at = -transcriber.LIVE_DECODE_INTERVAL_SAMPLES
  session._saw_speech = False
  session._decoded_region_end = 0
  session._finished = False
  return session


def test_closed_vad_segments_decode_padded_raw_pcm_without_overlap(monkeypatch) -> None:
  source = np.arange(20_000, dtype=np.int16)
  session = _session_with_pcm(source)
  session._decoded_region_end = 5_000
  session._vad = _ClosedVad(_ClosedSegment(start=10_000, length=1_000))
  captured: list[np.ndarray] = []

  def fake_decode(bundle, samples: np.ndarray) -> str:
    captured.append(samples.copy())
    return "decoded"

  monkeypatch.setattr(transcriber, "_decode_samples", fake_decode)

  assert session._drain_closed_segments()

  expected = source[5_000:17_400].astype(np.float32) / 32768.0
  np.testing.assert_allclose(captured[0], expected)
  assert session._frozen_segments == ["decoded"]
  assert session._decoded_region_end == 17_400
  assert session._saw_speech


def test_live_vad_segment_decodes_padded_raw_pcm_without_advancing_frozen_boundary(monkeypatch) -> None:
  source = np.arange(20_000, dtype=np.int16)
  session = _session_with_pcm(source)
  session._decoded_region_end = 5_000
  session._vad = _LiveVad(_LiveSegment(start=10_000))
  captured: list[np.ndarray] = []

  def fake_decode(bundle, samples: np.ndarray) -> str:
    captured.append(samples.copy())
    return "live"

  monkeypatch.setattr(transcriber, "_decode_samples", fake_decode)

  assert session._decode_live_segment_if_due() == "live"

  expected = source[5_000:20_000].astype(np.float32) / 32768.0
  np.testing.assert_allclose(captured[0], expected)
  assert session._decoded_region_end == 5_000
  assert session._last_live_decode_at == 20_000
  assert session._saw_speech

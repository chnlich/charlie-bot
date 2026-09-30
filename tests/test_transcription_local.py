"""The local transcription backend's offline decode fan-out.

transcribe_pcm_offline fans a recording's VAD windows round-robin over the
bundle's recognizer pool. The pins: the joined transcript is the window order no
matter which instance finishes first, a busy recognizer never takes a second
concurrent decode (two simultaneous voice requests share the resident bundle),
and a failing window fails the whole call instead of dropping its text.
"""

from __future__ import annotations

import threading
import time

import numpy as np
import pytest

from src.agents import transcriber
from src.agents.transcriber import _SpeechModelBundle


class _StubStream:

  def __init__(self) -> None:
    self.samples: np.ndarray | None = None
    self.result = type("Result", (), {"text": ""})()

  def accept_waveform(self, sample_rate: int, samples: np.ndarray) -> None:
    del sample_rate
    self.samples = samples


class _StubRecognizer:
  """Decodes a window's marker sample into ``w<marker>`` after a per-window delay.

  ``enter_count`` ticks at every decode_stream entry — the concurrency probe reads
  it while a decode is still running, so it observes blocked-on-the-lock versus
  inside-the-decode deterministically. A second decode entering while one is
  running raises: the bundle's per-instance lock makes that unreachable, so the
  raise is the contract's tripwire.
  """

  def __init__(self, name: str, delays: dict[int, float], on_enter=None, fail_on: int | None = None) -> None:
    self.name = name
    self._delays = delays
    self._on_enter = on_enter
    self._fail_on = fail_on
    self._busy = False
    self.enter_count = 0
    self.decode_count = 0

  def create_stream(self) -> _StubStream:
    return _StubStream()

  def decode_stream(self, stream: _StubStream) -> None:
    assert stream.samples is not None and stream.samples.size > 0
    marker = int(round(float(stream.samples[0]) * 32768))
    self.enter_count += 1
    if self._on_enter is not None:
      self._on_enter(marker)
    if self._busy:
      raise AssertionError(f"{self.name}: concurrent decode of window w{marker}")
    self._busy = True
    try:
      self.decode_count += 1
      time.sleep(self._delays.get(marker, 0.0))
      if self._fail_on == marker:
        raise ValueError(f"window w{marker} decode failed")
      stream.result.text = f"w{marker}"
    finally:
      self._busy = False


def _stub_bundle(recognizers: list[_StubRecognizer]) -> _SpeechModelBundle:
  return _SpeechModelBundle(
      recognizers=tuple(recognizers),
      vad_config=None,
      decode_locks=tuple(threading.Lock() for _ in recognizers),
      engine="sherpa",
      model_id="stub",
  )


def _pcm_for_windows(window_count: int) -> bytes:
  # Window k's samples all carry marker k, so the stub's text names its window.
  samples = np.zeros(window_count * 16, dtype="<i2")
  for marker in range(window_count):
    samples[marker * 16:(marker + 1) * 16] = marker
  return samples.tobytes()


@pytest.fixture
def pool_windows(monkeypatch: pytest.MonkeyPatch):
  """Swap the VAD pass for one window per 16-sample block of the stub pcm."""

  def _install(window_count: int) -> list[tuple[int, int, int, int]]:
    windows = [(k * 16, (k + 1) * 16, k * 16, (k + 1) * 16) for k in range(window_count)]
    monkeypatch.setattr(transcriber, "_open_vad", lambda vad, buffer_seconds: None)
    monkeypatch.setattr(transcriber, "offline_decode_windows", lambda vad, samples: windows)
    return windows

  return _install


def test_offline_decode_joins_in_window_order_not_completion_order(
    monkeypatch: pytest.MonkeyPatch, pool_windows) -> None:
  """The first window decodes slowest; the transcript still reads w0..w4."""
  pool_windows(5)
  # Window 0 sleeps 0.3 s, the rest 0.01 s: completion order is w4..w0.
  delays = {0: 0.3, 1: 0.01, 2: 0.01, 3: 0.01, 4: 0.01}
  bundle = _stub_bundle([_StubRecognizer("inst0", delays), _StubRecognizer("inst1", delays)])
  text = transcriber.transcribe_pcm_offline(bundle, _pcm_for_windows(5))
  assert text == "w0 w1 w2 w3 w4"
  assert all(recognizer.decode_count > 0 for recognizer in bundle.recognizers)


def test_offline_decode_single_instance_pool_decodes_every_window(
    monkeypatch: pytest.MonkeyPatch, pool_windows) -> None:
  """The GPU shape (one recognizer) decodes the whole recording in order."""
  pool_windows(4)
  bundle = _stub_bundle([_StubRecognizer("solo", {})])
  assert transcriber.transcribe_pcm_offline(bundle, _pcm_for_windows(4)) == "w0 w1 w2 w3"


def test_concurrent_offline_decodes_never_share_a_recognizer(monkeypatch: pytest.MonkeyPatch, pool_windows) -> None:
  """Two simultaneous voice requests share the resident bundle; the per-instance
  lock serializes them. While the first call's window holds the instance, the
  second call's same-instance window stays lock-blocked; without the lock it
  would enter decode_stream immediately and the entry count would tick."""
  pool_windows(2)
  entered = threading.Event()

  def _on_enter(marker: int) -> None:
    if marker == 0:
      entered.set()

  delays = {0: 0.2}
  bundle = _stub_bundle([
      _StubRecognizer("inst0", delays, on_enter=_on_enter),
      _StubRecognizer("inst1", delays),
  ])
  pcm = _pcm_for_windows(2)
  failures: list[BaseException] = []

  def _request() -> None:
    try:
      assert transcriber.transcribe_pcm_offline(bundle, pcm) == "w0 w1"
    except BaseException as exc:  # re-raised on the test thread below
      failures.append(exc)

  first = threading.Thread(target=_request)
  first.start()
  assert entered.wait(timeout=5), "first decode never started"
  second = threading.Thread(target=_request)
  second.start()
  time.sleep(0.05)  # the first w0 decode still holds its instance for another ~0.15 s
  assert bundle.recognizers[0].enter_count == 1, (
      "a second decode entered the instance whose first decode is still running")
  first.join(timeout=5)
  second.join(timeout=5)
  if failures:
    raise failures[0]


def test_failing_window_fails_the_whole_call(monkeypatch: pytest.MonkeyPatch, pool_windows) -> None:
  """A window's decode error propagates; the transcript is never silently short."""
  pool_windows(3)
  bundle = _stub_bundle([_StubRecognizer("inst0", {}, fail_on=2), _StubRecognizer("inst1", {})])
  with pytest.raises(ValueError, match="window w2 decode failed"):
    transcriber.transcribe_pcm_offline(bundle, _pcm_for_windows(3))

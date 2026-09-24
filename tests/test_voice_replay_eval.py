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
from collections.abc import Awaitable, Callable
from pathlib import Path
from types import ModuleType

import numpy as np
import pytest

from src.agents import transcriber

ROOT = Path(__file__).resolve().parents[1]


def _load_script() -> ModuleType:
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
      *(
          transcriber._decode_samples(_stub_bundle(), source[left:right].astype(np.float32) / 32768.0)
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


# ---------------------------------------------------------------------------
# Harness module (loaded once the script exists; the parity tests above cover
# the transcriber seam it depends on).
script = _load_script()


def _sine_samples(seconds: float) -> np.ndarray:
  positions = np.arange(int(script.SAMPLE_RATE * seconds), dtype=np.float64)
  return (np.sin(2 * np.pi * 440.0 * positions / script.SAMPLE_RATE) * 10_000).astype("<i2")


def _write_wav(path: Path, samples: np.ndarray, rate: int = script.SAMPLE_RATE) -> None:
  import wave
  path.parent.mkdir(parents=True, exist_ok=True)
  with wave.open(str(path), "wb") as wav:
    wav.setnchannels(1)
    wav.setsampwidth(2)
    wav.setframerate(rate)
    wav.writeframes(samples.astype("<i2").tobytes())


def _write_events(session_dir: Path, events: list[dict]) -> None:
  import json
  events_path = session_dir / "data" / "chat_events.jsonl"
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text("".join(json.dumps(event) + "\n" for event in events), encoding="utf-8")


def _voice_event(content: object, timestamp: str, is_voice: bool = True) -> dict:
  return {"type": "user", "content": content, "is_voice": is_voice, "timestamp": timestamp}


RECORDED_AT = script.datetime(2026, 1, 1, 0, 0, 0, tzinfo=script.UTC)


def test_ground_truth_picks_first_voice_user_event_within_900s(tmp_path: Path) -> None:
  session = tmp_path / "s1"
  _write_events(
      session, [
          _voice_event("too early", "2025-12-31T23:59:00+00:00"),
          _voice_event("not voice", "2026-01-01T00:00:10+00:00", is_voice=False),
          {
              "type": "assistant",
              "content": "hi",
              "timestamp": "2026-01-01T00:00:11+00:00"
          },
          _voice_event({"not": "a string"}, "2026-01-01T00:00:12+00:00"),
          _voice_event("past the window", "2026-01-01T00:20:00+00:00"),
          _voice_event("the match", "2026-01-01T00:05:00+00:00"),
          _voice_event("later match", "2026-01-01T00:06:00+00:00"),
      ])

  assert script.find_ground_truth(session, RECORDED_AT) == "the match"


def test_ground_truth_none_when_only_past_window_events(tmp_path: Path) -> None:
  session = tmp_path / "s2"
  _write_events(session, [_voice_event("too late", "2026-01-01T01:00:00+00:00")])

  assert script.find_ground_truth(session, RECORDED_AT) is None


def test_load_clips_accepts_only_16k_mono_pcm16(tmp_path: Path) -> None:
  from src.core.config import CharlieBotConfig
  cfg = CharlieBotConfig(charliebot_home=tmp_path)
  voice = cfg.sessions_dir / "sess" / "voice"
  _write_wav(voice / "2026-01-01T000000.000Z_deadbeef.wav", _sine_samples(0.1))
  _write_wav(voice / "2026-01-01T000100.000Z_deadbeef.wav", _sine_samples(0.1), rate=8_000)
  (voice / "notes.txt").write_text("not audio", encoding="utf-8")

  clips = script.load_clips(cfg)

  assert [clip.path.name for clip in clips] == ["2026-01-01T000000.000Z_deadbeef.wav"]
  assert clips[0].session_id == "sess"
  assert clips[0].recorded_at == RECORDED_AT
  assert clips[0].duration_s == pytest.approx(0.1)


def test_out_guard_refuses_repository_tree_including_symlinks(tmp_path: Path) -> None:
  repo = tmp_path / "repo"
  (repo / "sub").mkdir(parents=True)
  allowed = script.ensure_out_dir(tmp_path / "elsewhere", repo)
  assert allowed == (tmp_path / "elsewhere").resolve()

  with pytest.raises(SystemExit, match="inside the repository"):
    script.ensure_out_dir(repo / "sub" / "out", repo)
  # A symlink outside the tree pointing into it is caught too: resolve() follows it.
  link = tmp_path / "linked-out"
  link.symlink_to(repo / "sub")
  with pytest.raises(SystemExit, match="inside the repository"):
    script.ensure_out_dir(link, repo)


def test_term_scoring_ignores_spaces_and_takes_the_minimum() -> None:
  scores = script.score_terms("I said Charlie Bot and charliebot today", "CharlieBot CharlieBot extra", ["CharlieBot"])

  assert scores["CharlieBot"] == {"sent": 2, "output": 2, "correct": 2}
  few = script.score_terms("one charliebot", "CharlieBot CharlieBot", ["CharlieBot"])
  assert few["CharlieBot"] == {"sent": 2, "output": 1, "correct": 1}


# ---------------------------------------------------------------------------
# Muse client against a loopback fake server.
import json  # noqa: E402

from websockets.asyncio.server import ServerConnection, serve  # noqa: E402

EXPECTED_HANDSHAKE = {
    "mode": "PUSH_TO_TALK",
    "authorization": {
        "accessToken": "test-token"
    },
    "audioEncoding": "PCM_16KHZ",
    "model": "muse-voice-transcribe-1.0",
    "partialMode": "CUMULATIVE",
    "emitAudioProgress": False,
}


def _serve(handler: Callable[[ServerConnection], Awaitable[None]]) -> serve:
  return serve(handler, "127.0.0.1", 0)


@pytest.mark.asyncio
async def test_muse_client_handshake_frames_and_final() -> None:
  observed: dict = {}

  async def handler(socket: ServerConnection) -> None:
    observed["handshake"] = json.loads(await socket.recv())
    await socket.send(json.dumps({"type": "ready"}))
    frames = []
    async for message in socket:
      frames.append(message)
      if isinstance(message, bytes):
        if len(frames) == 2:
          await socket.send(json.dumps({"type": "transcript", "text": "par one", "final": False}))
        elif len(frames) == 3:
          await socket.send(json.dumps({"type": "transcript", "text": "par one two", "final": False}))
      else:
        observed["end_stream"] = json.loads(message)
        await socket.send(json.dumps({"type": "transcript", "text": "hello world", "final": True}))
        await socket.send(json.dumps({"type": "speechComplete", "transcript": "hello world"}))
        await socket.close()
    observed["frames"] = frames

  samples = _sine_samples(3 * script.MUSE_FRAME_SAMPLES / script.SAMPLE_RATE)
  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    result = await script.muse_transcribe(f"ws://127.0.0.1:{port}", "test-token", samples, [], [])

  assert result.ok, result.error
  # Handshake is the first frame, with exactly the settled fields; keywords and
  # languageBias are absent when empty.
  assert observed["handshake"] == EXPECTED_HANDSHAKE
  audio = list(observed["frames"][:-1])
  assert all(isinstance(frame, bytes) for frame in audio)
  assert len(audio) == 3
  assert observed["end_stream"] == {"type": "endStream"}
  assert observed["frames"][-1] == json.dumps({"type": "endStream"})
  assert result.text == "hello world"
  assert result.stop_to_final_s is not None and result.stop_to_final_s >= 0
  assert result.first_partial_s is not None and result.first_partial_s >= 0
  # The access token never leaves the handshake.
  assert "test-token" not in json.dumps(result.__dict__)


@pytest.mark.asyncio
async def test_muse_client_sends_keywords_and_language_bias_when_set() -> None:
  observed: dict = {}

  async def handler(socket: ServerConnection) -> None:
    observed["handshake"] = json.loads(await socket.recv())
    await socket.send(json.dumps({"type": "ready"}))
    async for _message in socket:
      await socket.send(json.dumps({"type": "transcript", "text": "x", "final": True}))
      await socket.close()

  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    await script.muse_transcribe(
        f"ws://127.0.0.1:{port}", "test-token", _sine_samples(0.2), ["CharlieBot"], ["Mandarin Chinese", "English"])

  assert observed["handshake"] == {
      **EXPECTED_HANDSHAKE,
      "keywords": ["CharlieBot"],
      "languageBias": ["Mandarin Chinese", "English"],
  }


@pytest.mark.asyncio
async def test_muse_handshake_error_records_failure_and_run_continues() -> None:
  connections = {"count": 0}

  async def handler(socket: ServerConnection) -> None:
    connections["count"] += 1
    await socket.recv()
    if connections["count"] == 1:
      await socket.send(json.dumps({"type": "error", "message": "invalid credentials"}))
      await socket.close()
      return
    await socket.send(json.dumps({"type": "ready"}))
    async for message in socket:
      if isinstance(message, str):
        await socket.send(json.dumps({"type": "transcript", "text": "second clip", "final": True}))
        await socket.close()

  clips = [
      script.Clip(path=Path(f"clip{i}.wav"), session_id="s", recorded_at=RECORDED_AT, samples=_sine_samples(0.2))
      for i in range(2)
  ]
  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    records = await script.run_muse_engine(f"ws://127.0.0.1:{port}", "test-token", clips, [], [])

  assert len(records) == 2
  assert records[0]["ok"] is False
  assert "handshake rejected" in records[0]["error"]
  assert records[1]["ok"] is True
  assert records[1]["text"] == "second clip"


@pytest.mark.asyncio
async def test_muse_abnormal_close_mid_stream_is_a_failure_with_code() -> None:

  async def handler(socket: ServerConnection) -> None:
    await socket.recv()
    await socket.send(json.dumps({"type": "ready"}))
    async for _message in socket:
      await socket.close(code=1008, reason="audio backlog exceeded")

  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    result = await script.muse_transcribe(f"ws://127.0.0.1:{port}", "test-token", _sine_samples(0.4), [], [])

  assert result.ok is False
  assert result.close_code == 1008
  assert result.close_reason == "audio backlog exceeded"

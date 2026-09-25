"""Voice replay evaluation harness tests: window parity, ground truth, out guard, scoring.

The harness (scripts/voice_replay_eval.py) replays recorded dictations through the
registered transcription backends. The backend clients themselves (local, Gemini,
Muse) are covered by the tests/test_transcription_*.py suites against loopback fake
servers; these tests pin the harness's own pieces and never touch real recordings
or the network, building all fixtures from synthetic PCM under tmp_path.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import pytest
from conftest import FakeOfflineVad, load_voice_replay_eval_script, sine_wav_frames, stub_speech_bundle, write_wav_file

from src.agents import transcriber


def _install_fakes(
    monkeypatch: pytest.MonkeyPatch,
    segments: list[tuple[int, int]],
    decode_texts: list[str],
) -> tuple[list[np.ndarray], list[str]]:
  """Stub the VAD and decoder; return (captured decode windows, per-call texts)."""
  captured: list[np.ndarray] = []
  texts: list[str] = []

  def fake_open_vad(_config: object, _buffer_seconds: float) -> FakeOfflineVad:
    return FakeOfflineVad(segments)

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

  production_text = transcriber.transcribe_pcm_offline(stub_speech_bundle("sherpa", "test"), source.tobytes())
  assert production_text == "first second"
  production_windows = [c.copy() for c in captured]

  # The replay pass: fresh VAD over the same samples, decode each window the
  # extracted function returns, join exactly as production does.
  captured.clear()
  vad = FakeOfflineVad(segments)
  windows = transcriber.offline_decode_windows(vad, source)
  replay_text = transcriber._join_segments(
      *(
          transcriber._decode_samples(
              stub_speech_bundle("sherpa", "test"), source[left:right].astype(np.float32) / 32768.0)
          for _start, _end, left, right in windows))
  assert replay_text == production_text
  # Both passes decoded byte-identical spans, in order.
  assert len(captured) == len(production_windows)
  for replay_window, production_window in zip(captured, production_windows, strict=True):
    np.testing.assert_array_equal(replay_window, production_window)


def test_offline_windows_report_segment_and_window_bounds(monkeypatch: pytest.MonkeyPatch) -> None:
  source = np.zeros(200_000, dtype="<i2")
  _install_fakes(monkeypatch, [(100_000, 10_000), (130_000, 20_000)], [])

  windows = transcriber.offline_decode_windows(FakeOfflineVad([(100_000, 10_000), (130_000, 20_000)]), source)

  pause, pad = transcriber.SEGMENT_DECODE_PAUSE_SAMPLES, transcriber.SEGMENT_DECODE_PAD_SAMPLES
  assert windows[0] == (100_000, 110_000, 100_000 - pause, 110_000 + pad)
  # Window 2's left edge clips against window 1's right edge: no overlap, no replay.
  assert windows[1] == (130_000, 150_000, 110_000 + pad, 150_000 + pad)


# ---------------------------------------------------------------------------
# Harness module (loaded once the script exists; the parity tests above cover
# the transcriber seam it depends on).
script = load_voice_replay_eval_script()


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
  write_wav_file(
      voice / "2026-01-01T000000.000Z_deadbeef.wav", sine_wav_frames(0.1, script.SAMPLE_RATE), script.SAMPLE_RATE)
  write_wav_file(voice / "2026-01-01T000100.000Z_deadbeef.wav", sine_wav_frames(0.1, script.SAMPLE_RATE), 8_000)
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

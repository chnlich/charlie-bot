"""End-to-end offline transcription on the host's real speech models (local_only).

The fixture is a persisted (recording, transcript) pair under the session home —
the transcript is what the production pipeline wrote for that recording, so the
offline decode regresses against real speech and real recognized text. Nothing
here exists on a CI runner.
"""

from __future__ import annotations

import time
import unicodedata
import wave

import numpy as np
import pytest
from conftest import voice_fixture_pair, voice_models_cached

from src.agents import transcriber
from src.core.config import CharlieBotConfig

pytestmark = pytest.mark.local_only

# Evaluated at import, before the autouse profile fixture points CHARLIEBOT_HOME at
# an empty per-test profile: the models and the fixture recordings live in the real
# host home.
_REAL_HOME = CharlieBotConfig().charliebot_home

# Healthy dictations decode within a few characters of their persisted transcript;
# degenerate recordings (broken streaming era, misrecognitions) sit far above.
REGRESSION_CER_THRESHOLD = 0.3


def _real_cfg() -> CharlieBotConfig:
  return CharlieBotConfig(charliebot_home=_REAL_HOME)


def _require_models(cfg: CharlieBotConfig) -> None:
  if not voice_models_cached(cfg):
    pytest.skip("speech models are not present locally")
  transcriber.ensure_models_cached(cfg)


def _normalize_for_cer(text: str) -> str:
  chars: list[str] = []
  for ch in text.casefold():
    category = unicodedata.category(ch)
    if category.startswith(("P", "S")) or ch.isspace():
      continue
    chars.append(ch)
  return "".join(chars)


def _character_error_rate(hypothesis: str, reference: str) -> float:
  normalized_hypothesis = _normalize_for_cer(hypothesis)
  normalized_reference = _normalize_for_cer(reference)
  assert normalized_reference
  previous = list(range(len(normalized_reference) + 1))
  for i, ca in enumerate(normalized_hypothesis, start=1):
    current = [i]
    for j, cb in enumerate(normalized_reference, start=1):
      current.append(min(
          previous[j] + 1,
          current[j - 1] + 1,
          previous[j - 1] + (ca != cb),
      ))
    previous = current
  return previous[-1] / len(normalized_reference)


def test_offline_transcription_regression_against_persisted_transcript() -> None:
  cfg = _real_cfg()
  _require_models(cfg)
  bundle = transcriber.get_transcription_bundle(cfg)
  wav_path, expected = voice_fixture_pair(cfg)
  with wave.open(str(wav_path), "rb") as wav:
    pcm = wav.readframes(wav.getnframes())
  audio_seconds = len(pcm) / 2 / transcriber.SAMPLE_RATE

  started = time.perf_counter()
  text = transcriber.transcribe_pcm_offline(bundle, pcm)
  decode_seconds = time.perf_counter() - started

  assert text
  cer = _character_error_rate(text, expected)
  rtf = decode_seconds / audio_seconds
  print(f"Fixture: {wav_path} ({audio_seconds:.1f}s)")
  print(f"Text: {text}")
  print(f"Persisted: {expected}")
  print(f"CER: {cer:.4f}  RTF: {rtf:.4f}")
  assert cer <= REGRESSION_CER_THRESHOLD, (
      f"offline decode drifted from the persisted transcript (CER {cer:.3f}):\n"
      f"  text: {text}\n  persisted: {expected}")


def test_vad_feed_step_keeps_block_boundary_onset_unclipped() -> None:
  """The worst case: the sentence onset lands exactly on a 128 ms feed boundary.

  sherpa marks a segment's start at most 5024 samples (~0.31 s) before the end of
  the feed where speech is first detected, so the 128 ms step back-dates the start
  onto the onset (no clip), while one whole-recording feed lands ~L-5024 late and
  drops everything before it.
  """
  cfg = _real_cfg()
  paths = transcriber.voice_model_paths(cfg)
  if not paths.silero_vad.is_file():
    pytest.skip("silero VAD model is not present locally")
  wav_path, _expected = voice_fixture_pair(cfg)
  with wave.open(str(wav_path), "rb") as wav:
    pcm = wav.readframes(wav.getnframes())
  speech = np.frombuffer(pcm, dtype="<i2")

  vad_config = transcriber._create_vad_config(paths)
  baseline = transcriber._offline_vad_segments(
      transcriber._open_vad(vad_config, speech.size / transcriber.SAMPLE_RATE + 10), speech)
  assert baseline, "fixture recording carries no speech the VAD can segment"
  lead = transcriber.OFFLINE_VAD_FEED_SAMPLES * 8  # onset exactly on a feed boundary
  onset = lead + baseline[0][0]
  fed = np.concatenate([np.zeros(lead, dtype="<i2"), speech])

  segments = transcriber._offline_vad_segments(
      transcriber._open_vad(vad_config, fed.size / transcriber.SAMPLE_RATE + 10), fed)

  assert segments, "lead silence made the VAD lose the recording's speech"
  # One silero window (512 samples) of slack: measured on the boundary alignment
  # the segment start lands exactly on the onset.
  assert segments[0][0] <= onset + 512, (
      f"128 ms feed clipped the onset: segment starts at {segments[0][0]}, onset at {onset}")

  whole = transcriber._open_vad(vad_config, fed.size / transcriber.SAMPLE_RATE + 10)
  whole.accept_waveform(fed.astype(np.float32) / 32768.0)
  whole.flush()
  whole_segments = []
  while not whole.empty():
    segment = whole.front
    whole_segments.append((int(segment.start), int(segment.start) + len(segment.samples)))
    whole.pop()
  # The failure mode the feed step prevents, pinned against sherpa 1.13.4's
  # back-dating: one large feed starts the segment ~L-5024 into the recording.
  assert whole_segments[0][0] >= onset + 4000, (
      f"expected the single large feed to clip the onset (start {whole_segments[0][0]}, onset {onset})")

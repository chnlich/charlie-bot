"""decode_audio.py wiring: the script drives the offline transcription function.

The wiring test stubs the transcriber (CI-safe); the real-decode test runs the
script end-to-end on a fixture cut from a persisted recording (local_only).
"""

from __future__ import annotations

import subprocess
import sys
import wave
from pathlib import Path

import pytest
from conftest import voice_fixture_pair, voice_models_cached

from src.agents import transcriber
from src.core.config import CharlieBotConfig

SCRIPT = Path(__file__).resolve().parents[1] / "skills" / "voice-notes" / "scripts" / "decode_audio.py"

# Evaluated at import, before the autouse profile fixture points CHARLIEBOT_HOME at an
# empty per-test profile: the fixture recordings live in the real host home.
_REAL_HOME = CharlieBotConfig().charliebot_home


def _write_s16_fixture(wav_path: Path, target: Path) -> Path:
  """The raw 16 kHz mono s16 twin of a persisted recording: the script decodes it
  without the av import."""
  with wave.open(str(wav_path), "rb") as wav:
    frames = wav.readframes(wav.getnframes())
  target.write_bytes(frames)
  return target


def test_decode_audio_script_feeds_the_offline_function(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  calls: list[bytes] = []

  def fake_bundle(_cfg: object) -> object:
    return object()

  def fake_transcribe(_bundle: object, pcm: bytes) -> str:
    calls.append(pcm)
    return "wired transcript"

  monkeypatch.setattr(transcriber, "ensure_models_cached", lambda _cfg: None)
  monkeypatch.setattr(transcriber, "get_transcription_bundle", fake_bundle)
  monkeypatch.setattr(transcriber, "transcribe_pcm_offline", fake_transcribe)
  fixture = tmp_path / "clip.s16"
  fixture.write_bytes(b"\x00\x00" * 16_000)
  monkeypatch.setattr(sys, "argv", [str(SCRIPT), str(fixture)])

  with pytest.raises(SystemExit) as exit_info:
    script_globals = {"__name__": "__main__", "__file__": str(SCRIPT)}
    exec(compile(SCRIPT.read_text(encoding="utf-8"), str(SCRIPT), "exec"), script_globals)

  assert exit_info.value.code == 0
  assert calls == [fixture.read_bytes()]
  assert capsys.readouterr().out.strip() == "wired transcript"


@pytest.mark.local_only
def test_decode_audio_script_decodes_a_real_fixture(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=_REAL_HOME)
  if not voice_models_cached(cfg):
    pytest.skip("speech models are not present locally")
  wav_path, _expected = voice_fixture_pair(cfg)
  fixture = _write_s16_fixture(wav_path, tmp_path / "voice-decode-fixture.s16")

  result = subprocess.run(
      [sys.executable, str(SCRIPT), str(fixture)],
      cwd=SCRIPT.parents[2],
      capture_output=True,
      text=True,
      check=False,
      timeout=600,
  )

  try:
    assert result.returncode == 0, f"stderr:\n{result.stderr}"
    transcript = result.stdout.strip()
    print(f"Fixture: {fixture} ({fixture.stat().st_size / 32000:.1f}s)")
    print(f"Transcript: {transcript}")
    assert transcript
  finally:
    fixture.unlink(missing_ok=True)

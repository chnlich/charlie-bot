"""Transcription registry tests: id lookup, lister, import weight, credentials, config validation.

The registry is the only id -> backend map; every consumer (config validation,
the replay script) reaches backends through it. These tests never touch the
network, and the replay-script one drives a registered fake backend through
script.main unchanged.
"""

from __future__ import annotations

import asyncio
import json
import subprocess
import sys
from pathlib import Path

import pytest
from conftest import load_voice_replay_eval_script, sine_wav_frames, write_wav_file
from pydantic import ValidationError

from src.agents.transcription import registry
from src.agents.transcription.base import TranscriptEvent, TranscriptionBackend, TranscriptionRejected
from src.core.config import CharlieBotConfig, load_config

ROOT = Path(__file__).resolve().parents[1]


def test_unknown_id_raises_naming_the_known_ids() -> None:
  with pytest.raises(ValueError, match="local, gemini, muse") as exc_info:
    registry.build_transcription_backend("geminii", CharlieBotConfig())
  assert "geminii" in str(exc_info.value)


def test_lister_builds_every_registered_backend() -> None:
  """One build per registration, each carrying its id, dropdown label, and partials flag."""
  backends = registry.build_transcription_backends(CharlieBotConfig())
  assert [(backend.id, backend.live_partials) for backend in backends] == [
      ("local", False),
      ("gemini", True),
      ("muse", True),
  ]
  assert [backend.label for backend in backends] == [
      "Local (sherpa)",
      "Gemini 3.5 Transcribe Live",
      "Muse Voice Transcribe",
  ]


def test_registry_import_loads_neither_numpy_nor_websockets() -> None:
  """The registry stays import-light: the M99 server import floor (docs/perf_baseline.md)
  rides it through the config validator, and the backend modules load only on build."""
  code = (
      "import json, sys; "
      "import src.agents.transcription.registry; "
      "print(json.dumps(sorted(set(sys.modules) & {'numpy', 'websockets', 'src.agents.transcriber', "
      "'src.agents.transcription.local', 'src.agents.transcription.gemini', "
      "'src.agents.transcription.muse'})))")
  proc = subprocess.run([sys.executable, "-c", code], cwd=ROOT, capture_output=True, text=True, timeout=120, check=True)
  assert proc.stdout.strip() == "[]"


async def _drain(events) -> list[TranscriptEvent]:
  return [event async for event in events]


async def _empty_audio():
  """An audio stream that ends immediately: the credential check runs before any pull."""
  return
  yield b""  # pragma: no cover — makes the function an async generator


def test_missing_credential_reports_reason_and_rejects(temp_home: Path) -> None:
  """An absent credential is availability, not a build failure: the reason names the key,
  and transcribe refuses the session before any connection."""
  cfg = CharlieBotConfig()
  cases = [
      (registry.build_transcription_backend("gemini", cfg), "needs gemini.api_key"),
      (registry.build_transcription_backend("muse", cfg), "needs meta.model_api_key")
  ]
  for backend, reason in cases:
    assert backend.unavailable_reason() == reason
    with pytest.raises(TranscriptionRejected, match=r"credentials\.yaml"):
      asyncio.run(_drain(backend.transcribe(_empty_audio(), vocabulary=[], languages=[])))
  assert registry.build_transcription_backend("local", cfg).unavailable_reason() is None


# ---------------------------------------------------------------------------
# Config validation: default_backend is validated against the registry's ids.
def test_voice_config_loads_the_three_new_keys() -> None:
  cfg = CharlieBotConfig(
      voice={
          "default_backend": "gemini",
          "vocabulary": ["CharlieBot", "Charlie Code"],
          "languages": ["zh", "en"],
      })
  assert cfg.voice.default_backend == "gemini"
  assert cfg.voice.vocabulary == ["CharlieBot", "Charlie Code"]
  assert cfg.voice.languages == ["zh", "en"]


def test_voice_config_defaults_keep_local_and_empty() -> None:
  cfg = CharlieBotConfig()
  assert cfg.voice.default_backend == "local"
  assert cfg.voice.vocabulary == []
  assert cfg.voice.languages == []


def test_unknown_default_backend_fails_config_load() -> None:
  with pytest.raises(ValidationError, match="not a transcription backend") as exc_info:
    CharlieBotConfig(voice={"default_backend": "whisper"})
  assert "whisper" in str(exc_info.value)


def test_unknown_default_backend_fails_load_config(temp_home: Path) -> None:
  """The yaml path fails at startup like any other config error."""
  config_path = temp_home / ".charliebot" / "config.yaml"
  config_path.parent.mkdir(parents=True)
  config_path.write_text("voice:\n  default_backend: whisper\n", encoding="utf-8")
  with pytest.raises(ValidationError, match="whisper"):
    load_config()


def test_unknown_voice_key_still_fails() -> None:
  with pytest.raises(ValidationError) as exc_info:
    CharlieBotConfig(voice={"bogus_key": 1})
  assert "bogus_key" in str(exc_info.value)


# ---------------------------------------------------------------------------
# Acceptance (e), replay half: one registered fake backend runs through the
# script with no script change.
class _FakeBackend(TranscriptionBackend):
  id = "fake"
  label = "Fake Backend"
  live_partials = True

  def __init__(self, cfg: CharlieBotConfig, marker: str = "") -> None:
    self.cfg = cfg
    self.marker = marker

  async def transcribe(self, audio, *, vocabulary, languages):
    chunks = 0
    async for _chunk in audio:
      chunks += 1
    yield TranscriptEvent(kind="partial", text=f"fake partial over {chunks} chunks")
    yield TranscriptEvent(kind="final", text="fake final")


def test_registered_fake_backend_drives_the_replay_script(
    temp_home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(registry, "_FACTORIES", dict(registry._FACTORIES))
  registry.register_transcription_backend("fake", _FakeBackend)
  voice_dir = temp_home / ".charliebot" / "sessions" / "sess" / "voice"
  write_wav_file(voice_dir / "2026-01-01T000000.000Z_deadbeef.wav", sine_wav_frames(0.2, 16_000), 16_000)
  out = tmp_path / "out"

  script = load_voice_replay_eval_script()
  assert script.main(["--engines", "fake", "--out", str(out)]) == 0

  payload = json.loads((out / "results.json").read_text(encoding="utf-8"))
  assert [record["engine"] for record in payload["clips"]] == ["fake"]
  record = payload["clips"][0]
  assert record["ok"] is True
  assert record["text"] == "fake final"
  assert record["first_partial_s"] is not None
  assert record["stop_to_final_s"] is not None
  summary = (out / "summary.md").read_text(encoding="utf-8")
  assert "| fake | 1 | 0 |" in summary

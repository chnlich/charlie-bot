"""voice_setup deployment-step tests: config-write idempotency, recording pick, enable flow."""

from __future__ import annotations

import wave
from pathlib import Path

import pytest

from src.core import voice_setup
from src.core.config import CharlieBotConfig


def _write_wav(path: Path, seconds: float, rate: int = 16_000) -> Path:
  path.parent.mkdir(parents=True, exist_ok=True)
  with wave.open(str(path), "wb") as wav:
    wav.setnchannels(1)
    wav.setsampwidth(2)
    wav.setframerate(rate)
    wav.writeframes(b"\x00\x00" * int(rate * seconds))
  return path


def _home(tmp_path: Path) -> Path:
  home = tmp_path / "charliebot-home"
  home.mkdir()
  return home


_CONFIG_WRITE_CASES = [
    # (config.yaml text written before the call, expected action, expected file text after).
    pytest.param(
        "# host config\nserver:\n  port: 18498\n",
        "appended",
        "# host config\nserver:\n  port: 18498\nvoice:\n  engine: qwen3_hf\n",
        id="appends-when-absent",
    ),
    pytest.param(
        "server:\n  port: 18498\nvoice:\n  engine: qwen3_hf\n",
        "skipped",
        "server:\n  port: 18498\nvoice:\n  engine: qwen3_hf\n",
        id="skips-when-already-enabled",
    ),
    pytest.param(
        "server:\n  port: 18498\nvoice:\n  engine: sherpa  # keep cpu\n# trailing comment\n",
        "updated",
        "server:\n  port: 18498\nvoice:\n  engine: qwen3_hf\n# trailing comment\n",
        id="updates-in-place",
    ),
    pytest.param(
        None,
        "appended",
        "voice:\n  engine: qwen3_hf\n",
        id="creates-missing-config",
    ),
]

_REJECT_CASES = [
    # (config.yaml text, the ValueError fragment).
    pytest.param("- just\n- a list\n", "top-level mapping", id="non-mapping-config"),
    pytest.param("voice:\n  engine: sherpa\n  engine: qwen3_hf\n", "2 lines", id="duplicate-key-lines"),
]


@pytest.mark.parametrize(("config_text", "expected_action", "expected_text"), _CONFIG_WRITE_CASES)
def test_write_voice_engine_config_write_outcome(
    tmp_path: Path, config_text: str | None, expected_action: str, expected_text: str) -> None:
  """Each reachable config-write state takes exactly its documented action, and the
  file text after the call matches — including the comments and trailing lines the
  textual rewrite promises to preserve."""
  home = _home(tmp_path)
  if config_text is not None:
    (home / "config.yaml").write_text(config_text, encoding="utf-8")

  action = voice_setup.write_voice_engine(home)

  assert action == expected_action
  assert (home / "config.yaml").read_text(encoding="utf-8") == expected_text


@pytest.mark.parametrize(("config_text", "match"), _REJECT_CASES)
def test_write_voice_engine_rejects_malformed_config(tmp_path: Path, config_text: str, match: str) -> None:
  """Malformed config.yaml raises before any write, never falls through to a
  best-effort edit."""
  home = _home(tmp_path)
  (home / "config.yaml").write_text(config_text, encoding="utf-8")

  with pytest.raises(ValueError, match=match):
    voice_setup.write_voice_engine(home)


def test_pick_preflight_recording_chooses_closest_to_ten_seconds(tmp_path: Path) -> None:
  sessions = tmp_path / "sessions"
  _write_wav(sessions / "a" / "voice" / "eight.wav", 8.0)
  ten = _write_wav(sessions / "b" / "voice" / "ten.wav", 10.0)
  _write_wav(sessions / "c" / "voice" / "twenty.wav", 20.0)

  assert voice_setup.pick_preflight_recording(sessions) == ten


def test_pick_preflight_recording_rejects_out_of_band_only(tmp_path: Path) -> None:
  sessions = tmp_path / "sessions"
  _write_wav(sessions / "a" / "voice" / "short.wav", 2.0)
  _write_wav(sessions / "b" / "voice" / "long.wav", 27.0)

  with pytest.raises(RuntimeError, match="voice recording"):
    voice_setup.pick_preflight_recording(sessions)


def test_pick_preflight_recording_skips_non_voice_rate(tmp_path: Path) -> None:
  sessions = tmp_path / "sessions"
  _write_wav(sessions / "a" / "voice" / "44k.wav", 10.0, rate=44_100)

  with pytest.raises(RuntimeError, match="voice recording"):
    voice_setup.pick_preflight_recording(sessions)


def test_enable_runs_preflight_then_writes_config(monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = CharlieBotConfig()
  preflight_reports: list[CharlieBotConfig] = []
  writes: list[Path] = []
  monkeypatch.setattr(
      voice_setup, "run_gpu_preflight",
      lambda received_cfg: preflight_reports.append(received_cfg) or {"decode_seconds": 0.3})
  monkeypatch.setattr(voice_setup, "write_voice_engine", lambda home: writes.append(home) or "appended")

  report = voice_setup.enable(cfg)

  assert preflight_reports == [cfg]
  assert writes == [cfg.charliebot_home]
  assert report == {"decode_seconds": 0.3, "config_write": "appended"}


def test_enable_twice_rewrites_nothing_when_already_enabled(monkeypatch: pytest.MonkeyPatch) -> None:
  """The deployment step is idempotent: a second enable hits the skipped write path."""
  cfg = CharlieBotConfig()
  actions = iter(["appended", "skipped"])
  monkeypatch.setattr(voice_setup, "run_gpu_preflight", lambda cfg_: {"decode_seconds": 0.3})
  monkeypatch.setattr(voice_setup, "write_voice_engine", lambda home: next(actions))

  assert voice_setup.enable(cfg)["config_write"] == "appended"
  assert voice_setup.enable(cfg)["config_write"] == "skipped"


def test_main_requires_enable_subcommand(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr("sys.argv", ["voice_setup"])
  with pytest.raises(SystemExit):
    voice_setup.main()
  monkeypatch.setattr("sys.argv", ["voice_setup", "preflight"])
  with pytest.raises(SystemExit):
    voice_setup.main()


def test_main_prints_report(monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  monkeypatch.setattr("sys.argv", ["voice_setup", "enable"])
  monkeypatch.setattr(voice_setup, "enable", lambda cfg=None: {"decode_seconds": 0.3, "config_write": "skipped"})

  voice_setup.main()

  out = capsys.readouterr().out
  assert "decode_seconds: 0.3" in out
  assert "config_write: skipped" in out

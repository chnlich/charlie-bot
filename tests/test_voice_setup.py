"""voice_setup deployment-step tests: config-write idempotency, recording pick, enable flow."""

from __future__ import annotations

import wave
from pathlib import Path

import pytest

import src.core.voice_setup as voice_setup
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


def test_write_voice_engine_appends_when_absent(tmp_path: Path) -> None:
  home = _home(tmp_path)
  (home / "config.yaml").write_text("# host config\nserver_port: 18498\n", encoding="utf-8")

  action = voice_setup.write_voice_engine(home)

  assert action == "appended"
  text = (home / "config.yaml").read_text(encoding="utf-8")
  assert text == "# host config\nserver_port: 18498\nvoice_engine: qwen3_hf\n"


def test_write_voice_engine_skips_when_already_enabled(tmp_path: Path) -> None:
  home = _home(tmp_path)
  original = "server_port: 18498\nvoice_engine: qwen3_hf\n"
  (home / "config.yaml").write_text(original, encoding="utf-8")

  action = voice_setup.write_voice_engine(home)

  assert action == "skipped"
  assert (home / "config.yaml").read_text(encoding="utf-8") == original


def test_write_voice_engine_updates_in_place(tmp_path: Path) -> None:
  home = _home(tmp_path)
  (home / "config.yaml").write_text(
      "server_port: 18498\nvoice_engine: sherpa  # keep cpu\n# trailing comment\n", encoding="utf-8")

  action = voice_setup.write_voice_engine(home)

  assert action == "updated"
  text = (home / "config.yaml").read_text(encoding="utf-8")
  assert text == "server_port: 18498\nvoice_engine: qwen3_hf\n# trailing comment\n"


def test_write_voice_engine_creates_missing_config(tmp_path: Path) -> None:
  home = _home(tmp_path)

  action = voice_setup.write_voice_engine(home)

  assert action == "appended"
  assert (home / "config.yaml").read_text(encoding="utf-8") == "voice_engine: qwen3_hf\n"


def test_write_voice_engine_rejects_fragment_defined_key(tmp_path: Path) -> None:
  """A voice_engine key in config.d would collide with the appended one at load; refuse."""
  home = _home(tmp_path)
  (home / "config.yaml").write_text("server_port: 18498\n", encoding="utf-8")
  fragment = home / "config.d" / "voice.yaml"
  fragment.parent.mkdir()
  fragment.write_text("voice_engine: sherpa\n", encoding="utf-8")

  with pytest.raises(ValueError, match="config.d"):
    voice_setup.write_voice_engine(home)


def test_write_voice_engine_ignores_fragments_without_the_key(tmp_path: Path) -> None:
  home = _home(tmp_path)
  (home / "config.yaml").write_text("server_port: 18498\n", encoding="utf-8")
  fragment = home / "config.d" / "telegram.yaml"
  fragment.parent.mkdir()
  fragment.write_text("telegram_chat_id: '123'\n", encoding="utf-8")

  assert voice_setup.write_voice_engine(home) == "appended"


def test_write_voice_engine_rejects_non_mapping_config(tmp_path: Path) -> None:
  home = _home(tmp_path)
  (home / "config.yaml").write_text("- just\n- a list\n", encoding="utf-8")

  with pytest.raises(ValueError, match="top-level mapping"):
    voice_setup.write_voice_engine(home)


def test_write_voice_engine_rejects_duplicate_key_lines(tmp_path: Path) -> None:
  home = _home(tmp_path)
  (home / "config.yaml").write_text(
      "voice_engine: sherpa\nserver_port: 18498\nvoice_engine: qwen3_hf\n", encoding="utf-8")

  with pytest.raises(ValueError, match="2 lines"):
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


def test_main_requires_enable_subcommand(monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
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

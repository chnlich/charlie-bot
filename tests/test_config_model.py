"""CharlieBotConfig schema gates: unknown-kwarg rejection.

``extra='forbid'`` plus the ``model_construct`` override turn a misnamed kwarg
into an error at the call site (pydantic 2.12.5 drops unknown construct kwargs
silently even under forbid).
"""

from pathlib import Path

import pytest
from pydantic import ValidationError

from src.core.config import CharlieBotConfig, ServerConfig, load_config

# A read-only derived property a hand-redirect could target, and a fabricated
# name; both must be rejected by name on both construction entry points.
_UNKNOWN_KWARGS = ["sessions_dir", "totally_unknown_field"]


@pytest.mark.parametrize("name", _UNKNOWN_KWARGS)
def test_constructor_rejects_unknown_kwarg(name: str) -> None:
  """extra='forbid': the constructor raises naming the key."""
  with pytest.raises(ValidationError) as exc_info:
    CharlieBotConfig(**{name: "/tmp/x"})
  assert name in str(exc_info.value)


@pytest.mark.parametrize("name", _UNKNOWN_KWARGS)
def test_model_construct_rejects_unknown_kwarg(name: str) -> None:
  """The model_construct override raises TypeError naming the key."""
  with pytest.raises(TypeError) as exc_info:
    CharlieBotConfig.model_construct(**{name: "/tmp/x"})
  assert name in str(exc_info.value)


def test_model_construct_lists_every_unknown_kwarg() -> None:
  """Several unknown names are all reported in one error."""
  with pytest.raises(TypeError) as exc_info:
    CharlieBotConfig.model_construct(sessions_dir="/tmp/x", bogus_name="y")
  message = str(exc_info.value)
  assert "sessions_dir" in message
  assert "bogus_name" in message


def test_model_construct_still_builds_known_fields(tmp_path: Path) -> None:
  """Known fields delegate to super() unchanged."""
  cfg = CharlieBotConfig.model_construct(charliebot_home=tmp_path, server=ServerConfig(port=1))
  assert cfg.charliebot_home == tmp_path
  assert cfg.server.port == 1
  assert cfg.sessions_dir == tmp_path / "sessions"


def test_voice_engine_defaults_to_sherpa() -> None:
  """A config carrying neither key behaves exactly as before: CPU sherpa, 1.7B tier."""
  cfg = CharlieBotConfig()
  assert cfg.voice.engine == "sherpa"
  assert cfg.voice.model_id == "Qwen/Qwen3-ASR-1.7B-hf"


def test_voice_engine_accepts_explicit_qwen3_hf() -> None:
  """Both keys are settable; the model id picks the tier independently of the engine."""
  cfg = CharlieBotConfig(voice={"engine": "qwen3_hf", "model_id": "Qwen/Qwen3-ASR-0.6B-hf"})
  assert cfg.voice.engine == "qwen3_hf"
  assert cfg.voice.model_id == "Qwen/Qwen3-ASR-0.6B-hf"


def test_voice_engine_rejects_unknown_engine() -> None:
  """An engine typo is a validation error naming the field, not a silent sherpa."""
  with pytest.raises(ValidationError) as exc_info:
    CharlieBotConfig(voice={"engine": "cuda"})
  assert "voice.engine" in str(exc_info.value)


def test_load_config_reads_voice_keys(temp_home: Path) -> None:
  """The YAML config path parses both keys (config.yaml -> CharlieBotConfig)."""
  config_path = temp_home / ".charliebot" / "config.yaml"
  config_path.parent.mkdir(parents=True)
  config_path.write_text("voice:\n  engine: qwen3_hf\n  model_id: Qwen/Qwen3-ASR-0.6B-hf\n", encoding="utf-8")
  cfg = load_config()
  assert cfg.voice.engine == "qwen3_hf"
  assert cfg.voice.model_id == "Qwen/Qwen3-ASR-0.6B-hf"


def test_load_config_defaults_voice_keys_when_absent(temp_home: Path) -> None:
  """An existing config without the keys loads unchanged on the default engine."""
  config_path = temp_home / ".charliebot" / "config.yaml"
  config_path.parent.mkdir(parents=True)
  config_path.write_text("server:\n  port: 18499\n", encoding="utf-8")
  cfg = load_config()
  assert cfg.voice.engine == "sherpa"
  assert cfg.server.port == 18499

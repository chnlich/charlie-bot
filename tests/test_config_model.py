"""CharlieBotConfig schema gates: unknown-kwarg rejection and copy-style home redirection.

Two mechanisms live on the model itself. ``extra='forbid'`` plus the
``model_construct`` override turn a misnamed kwarg into an error at the call
site (pydantic 2.12.5 drops unknown construct kwargs silently even under
forbid). ``with_home`` is the supported copy-style redirection entry point:
every derived Path property follows the redirected home while the original
instance and the nested models stay untouched.
"""

from pathlib import Path

import pytest
from pydantic import ValidationError

from src.core.config import CharlieBotConfig, load_config

# A read-only derived property (the incident's trap) and a fabricated name; both
# must be rejected by name on both construction entry points.
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
  cfg = CharlieBotConfig.model_construct(charliebot_home=tmp_path, server_port=1)
  assert cfg.charliebot_home == tmp_path
  assert cfg.server_port == 1
  assert cfg.sessions_dir == tmp_path / "sessions"


def _path_properties(cfg: CharlieBotConfig) -> dict[str, Path]:
  """Every CharlieBotConfig property returning a Path, as ``{name: value}``."""
  return {
      name: value
      for name, attr in vars(CharlieBotConfig).items()
      if isinstance(attr, property) and isinstance(value := attr.fget(cfg), Path)
  }


# The two properties derived from the repo/config rather than charliebot_home;
# everything else returning a Path must follow the redirected home.


def test_with_home_redirects_every_derived_path(tmp_path: Path) -> None:
  """Every home-derived Path property resolves under the new home, nothing else moves.

  The properties are enumerated dynamically so a future derived path cannot
  silently escape the redirection; the classification is pinned to the known
  derivation split so a property landing under home by coincidence shows up.
  """
  home_a = tmp_path / "home-a"
  home_b = tmp_path / "home-b"
  cfg = CharlieBotConfig(charliebot_home=home_a)
  redirected = cfg.with_home(str(home_b))

  props, redirected_props = _path_properties(cfg), _path_properties(redirected)
  assert set(props) == set(redirected_props)
  home_derived = {name for name, path in props.items() if home_a == path or home_a in path.parents}
  assert home_derived == {"sessions_dir", "claude_md_file", "memory_dir", "config_file", "config_d_dir"}
  for name, path in props.items():
    if name in home_derived:
      assert redirected_props[name] == home_b / path.relative_to(home_a)
    else:
      assert redirected_props[name] == path


def test_with_home_preserves_the_rest_of_the_instance(tmp_path: Path) -> None:
  """Exactly one field moves: nested models keep their identity, the original is untouched."""
  home_a = tmp_path / "home-a"
  home_b = tmp_path / "home-b"
  cfg = CharlieBotConfig(charliebot_home=home_a)
  redirected = cfg.with_home(home_b)

  assert redirected is not cfg
  assert cfg.charliebot_home == home_a
  assert redirected.charliebot_home == home_b
  assert redirected.model_dump() == {**cfg.model_dump(), "charliebot_home": home_b}
  assert redirected.backend_options[0] is cfg.backend_options[0]


def test_with_home_expands_tilde(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A '~' path is expanded before the redirect."""
  monkeypatch.setenv("HOME", str(tmp_path))
  redirected = CharlieBotConfig().with_home("~/redirected")
  assert redirected.charliebot_home == tmp_path / "redirected"


def test_with_home_rejects_relative_path() -> None:
  """A relative path would mean a different home per process cwd, like charliebot_home_dir."""
  with pytest.raises(ValueError, match="absolute path"):
    CharlieBotConfig().with_home("relative/dir")


def test_voice_engine_defaults_to_sherpa() -> None:
  """A config carrying neither key behaves exactly as before: CPU sherpa, 1.7B tier."""
  cfg = CharlieBotConfig()
  assert cfg.voice_engine == "sherpa"
  assert cfg.voice_model_id == "Qwen/Qwen3-ASR-1.7B-hf"


def test_voice_engine_accepts_explicit_qwen3_hf() -> None:
  """Both keys are settable; the model id picks the tier independently of the engine."""
  cfg = CharlieBotConfig(voice_engine="qwen3_hf", voice_model_id="Qwen/Qwen3-ASR-0.6B-hf")
  assert cfg.voice_engine == "qwen3_hf"
  assert cfg.voice_model_id == "Qwen/Qwen3-ASR-0.6B-hf"


def test_voice_engine_rejects_unknown_engine() -> None:
  """An engine typo is a validation error naming the field, not a silent sherpa."""
  with pytest.raises(ValidationError) as exc_info:
    CharlieBotConfig(voice_engine="cuda")
  assert "voice_engine" in str(exc_info.value)


def test_load_config_reads_voice_keys(temp_home: Path) -> None:
  """The YAML config path parses both keys (config.yaml -> CharlieBotConfig)."""
  config_path = temp_home / ".charliebot" / "config.yaml"
  config_path.parent.mkdir(parents=True)
  config_path.write_text("voice_engine: qwen3_hf\nvoice_model_id: Qwen/Qwen3-ASR-0.6B-hf\n", encoding="utf-8")
  cfg = load_config()
  assert cfg.voice_engine == "qwen3_hf"
  assert cfg.voice_model_id == "Qwen/Qwen3-ASR-0.6B-hf"


def test_load_config_defaults_voice_keys_when_absent(temp_home: Path) -> None:
  """An existing config without the keys loads unchanged on the default engine."""
  config_path = temp_home / ".charliebot" / "config.yaml"
  config_path.parent.mkdir(parents=True)
  config_path.write_text("server_port: 18499\n", encoding="utf-8")
  cfg = load_config()
  assert cfg.voice_engine == "sherpa"
  assert cfg.server_port == 18499

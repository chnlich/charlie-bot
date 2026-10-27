"""Schema and loader gates for the sectioned CharlieBotConfig."""

import asyncio
import json
import pathlib

import conftest
import pytest
import yaml

from src.infra import config
from src.runtime import init_seed


@pytest.mark.parametrize("fragment_name", ["x.yaml", "cron.yaml"])
def test_config_d_fragments_are_rejected(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, fragment_name: str) -> None:
  home = tmp_path / "home"
  (home / "config.d").mkdir(parents=True)
  (home / "config.yaml").write_text("server:\n  host: 127.0.0.1\n", encoding="utf-8")
  (home / "config.d" / fragment_name).write_text("voice:\n  default_backend: gemini\n", encoding="utf-8")
  monkeypatch.setenv(config.CHARLIEBOT_HOME_ENV, str(home))
  with pytest.raises(ValueError) as excinfo:
    config.load_config()
  assert f"config.d/{fragment_name}" in str(excinfo.value)


def _credentials_home(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
  """A temp CHARLIEBOT_HOME with a minimal valid sectioned config.yaml; returns the home path."""
  home = tmp_path / "home"
  home.mkdir()
  (home / "config.yaml").write_text("server:\n  port: 2001\n", encoding="utf-8")
  monkeypatch.setenv(config.CHARLIEBOT_HOME_ENV, str(home))
  return home


def test_credentials_stay_out_of_config_and_get_returns_each_sentinel(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = _credentials_home(tmp_path, monkeypatch)
  sections = {
      "alpha": {
          "token": "sentinel-alpha-token",
          "secret": "sentinel-alpha-secret"
      },
      "beta": {
          "token": "sentinel-beta-token",
          "secret": "sentinel-beta-secret"
      },
  }
  (home / "credentials.yaml").write_text(yaml.safe_dump(sections), encoding="utf-8")
  dumped = json.dumps(config.load_config().model_dump(mode="json"))
  credentials = config.load_credentials()
  for section, keys in sections.items():
    for key, sentinel in keys.items():
      assert sentinel not in dumped
      assert credentials.get(section, key) == sentinel
  with pytest.raises(ValueError) as excinfo:
    credentials.require("alpha", "missing_key")
  assert str(excinfo.value) == f"credentials.alpha.missing_key is not set in {home / 'credentials.yaml'}"


@pytest.mark.parametrize(
    "body, fragment",
    [
        ("- one\n- two\n", "credentials must be a mapping"),
        ("alpha: scalar\n", "credentials.alpha"),
        ("alpha:\n  key: [1, 2]\n", "credentials.alpha.key"),
    ],
)
def test_credentials_shape_errors_name_the_offending_depth(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, body: str, fragment: str) -> None:
  home = _credentials_home(tmp_path, monkeypatch)
  (home / "credentials.yaml").write_text(body, encoding="utf-8")
  with pytest.raises(ValueError) as excinfo:
    config.load_credentials()
  assert fragment in str(excinfo.value)


def test_retired_slack_credential_keys_name_their_credentials_location(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = _credentials_home(tmp_path, monkeypatch)
  (home / "config.yaml").write_text("slack_bot_token: x\nslack_app_token: y\nslack_user_token: z\n", encoding="utf-8")
  with pytest.raises(ValueError) as excinfo:
    config.load_config()
  assert str(excinfo.value) == (
      f"{home / 'config.yaml'} still uses retired top-level keys; move each one:\n"
      "  slack_bot_token -> credentials.yaml slack.bot_token\n"
      "  slack_app_token -> credentials.yaml slack.app_token\n"
      "  slack_user_token -> credentials.yaml slack.user_token")


EXAMPLE_PATH = conftest.ROOT / "configs" / "config.example.yaml"

STARTER_BACKEND_IDS = ["claude-fable", "claude-opus", "claude-sonnet"]


def test_example_config_loads_to_the_model_default(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The shipped example is the default config: loading it equals constructing
  CharlieBotConfig, modulo backends.options (the example ships the three starter
  entries where the model default is empty)."""
  home = tmp_path / "home"
  home.mkdir()
  (home / "config.yaml").write_bytes(EXAMPLE_PATH.read_bytes())
  monkeypatch.setenv(config.CHARLIEBOT_HOME_ENV, str(home))
  loaded = config.load_config()
  loaded_dump = loaded.model_dump()
  default_dump = config.CharlieBotConfig(charliebot_home=home).model_dump()
  loaded_dump["backends"]["options"] = None
  default_dump["backends"]["options"] = None
  assert loaded_dump == default_dump
  assert [option.id for option in loaded.backends.options] == STARTER_BACKEND_IDS


def test_init_charliebot_home_seeds_config_and_credentials(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A fresh home gets config.yaml byte-equal to the example and credentials.yaml
  from the repo template, owner-readable only, loading as empty sections."""
  home = tmp_path / "home"
  home.mkdir()
  monkeypatch.setenv(config.CHARLIEBOT_HOME_ENV, str(home))
  fake_cfg = config.CharlieBotConfig(charliebot_home=home)
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: fake_cfg)
  asyncio.run(init_seed.init_charliebot_home())
  credentials_path = home / "credentials.yaml"
  assert credentials_path.exists()
  assert credentials_path.stat().st_mode & 0o777 == 0o600
  assert config.load_credentials().sections == {}
  assert (home / "config.yaml").read_bytes() == EXAMPLE_PATH.read_bytes()


def test_require_backends_rejects_empty_list() -> None:
  """An empty backends.options raises ValueError naming the key and the example file."""
  with pytest.raises(ValueError) as exc_info:
    config.require_backends(config.CharlieBotConfig())
  message = str(exc_info.value)
  assert "backends.options" in message
  assert "config.example.yaml" in message


# ---------------------------------------------------------------------------
# Claude account pools (accounts.claude_pools x backends.options.account_pool)
# ---------------------------------------------------------------------------


def _pooled_config(pools: dict[str, list[str]], *options: dict) -> config.CharlieBotConfig:
  """A config with accounts.claude entries a-e and the given pools and raw backend options."""
  accounts = [{"label": label, "config_dir": f"/tmp/claude-{label}"} for label in "abcde"]
  return config.CharlieBotConfig(
      accounts={
          "claude": accounts,
          "claude_pools": pools
      }, backends={"options": list(options)})


def _cc_claude(option_id: str, **extra: str) -> dict:
  return {"id": option_id, "label": option_id, "type": "cc-claude", "model": "m", **extra}


CC = "cc-claude"


def test_claude_pools_load_with_shared_labels_and_bound_options() -> None:
  """A label may sit in two pools; every cc-claude option names its pool; non-claude options
  carry no pool field."""
  cfg = _pooled_config(
      {
          "alpha": ["a", "b"],
          "beta": ["b", "c"]
      },
      _cc_claude("claude-a", account_pool="alpha"),
      _cc_claude("claude-b", account_pool="beta"),
      {
          "id": "codex-x",
          "label": "x",
          "type": "codex",
          "model": "m"
      },
  )
  assert cfg.accounts.claude_pools == {"alpha": ["a", "b"], "beta": ["b", "c"]}
  assert [option.account_pool for option in cfg.backends.options[:2]] == ["alpha", "beta"]


@pytest.mark.parametrize(
    "pools, options, fragment",
    [
        # A pool names a label accounts.claude does not list.
        ({
            "alpha": ["a", "ghost"]
        }, [_cc_claude("claude-a", account_pool="alpha")], "accounts.claude_pools['alpha']"),
        # A pool lists no account.
        ({
            "alpha": []
        }, [_cc_claude("claude-a", account_pool="alpha")], "accounts.claude_pools['alpha']"),
        # An option names a pool the table does not define.
        ({
            "alpha": ["a"]
        }, [_cc_claude("claude-lost", account_pool="beta")], "backend 'claude-lost'"),
        # An option sets a pool while no pools are defined.
        ({}, [_cc_claude("claude-orphan", account_pool="alpha")], "backend 'claude-orphan'"),
        # Pools are defined but an option names none.
        ({
            "alpha": ["a"]
        }, [_cc_claude("claude-bare")], "backend 'claude-bare'"),
    ],
)
def test_claude_pool_config_errors_name_the_offending_pool_or_option(
    pools: dict[str, list[str]], options: list[dict], fragment: str) -> None:
  with pytest.raises(ValueError) as excinfo:
    _pooled_config(pools, *options)
  assert fragment in str(excinfo.value)


def test_account_pool_on_a_non_cc_claude_option_is_rejected() -> None:
  """Only the cc-claude option type carries account_pool: another type naming it fails the
  option model's unknown-field refusal, before the cross-section validator runs."""
  with pytest.raises(ValueError) as excinfo:
    _pooled_config({}, {"id": "codex-x", "label": "x", "type": "codex", "model": "m", "account_pool": "alpha"})
  assert "account_pool" in str(excinfo.value)


def test_empty_claude_pools_keep_the_unpooled_schema() -> None:
  """No pools, no account_pool fields: the config loads exactly as before pools existed."""
  cfg = _pooled_config({}, _cc_claude("claude-a"))
  assert cfg.accounts.claude_pools == {}
  assert cfg.get_backend_option("claude-a").account_pool is None


def _family_backends(tmp_path: pathlib.Path, pool: list[str], *ids: str) -> config.CharlieBotConfig:
  """A config at *tmp_path* whose options carry *ids* (cc-claude) and the given pool."""
  options = [
      conftest.backend_option(id=backend_id, label=backend_id, type="cc-claude", model="m") for backend_id in ids
  ]
  return config.CharlieBotConfig(charliebot_home=tmp_path, backends={"pool": pool, "options": options})


def test_require_backends_accepts_known_pool_members(tmp_path: pathlib.Path) -> None:
  cfg = _family_backends(tmp_path, ["claude-sonnet", "claude-opus"], "claude-opus", "claude-sonnet")
  assert config.require_backends(cfg) is None


@pytest.mark.parametrize("key", ["preference", "worker_default"])
def test_load_config_refuses_a_retired_backends_key(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, key: str) -> None:
  """A config still carrying a retired backends key stops at load; the error names the key and backends.pool."""
  monkeypatch.setenv(config.CHARLIEBOT_HOME_ENV, str(tmp_path))
  (tmp_path / "config.yaml").write_text(f"backends:\n  {key}: []\n")
  with pytest.raises(ValueError, match=f"backends\\.{key}.*backends\\.pool"):
    config.load_config()


def test_require_backends_rejects_unknown_pool_entry(tmp_path: pathlib.Path) -> None:
  """A pool entry naming no option id stops startup; the error names the file, the entry
  and the id."""
  cfg = _family_backends(tmp_path, ["claude-opus", "codex-gpt-5.6-luna"], "claude-opus")
  with pytest.raises(ValueError) as exc_info:
    config.require_backends(cfg)
  message = str(exc_info.value)
  assert f"{tmp_path / 'config.yaml'}: backends.pool[1] names unknown backend 'codex-gpt-5.6-luna'" in message
  assert "claude-opus'" not in message


def test_require_backends_rejects_duplicate_option_id(tmp_path: pathlib.Path) -> None:
  """An option id listed twice stops startup; the error names the file, the repeated entry and
  the id."""
  cfg = _family_backends(tmp_path, [], "claude-opus", "claude-sonnet", "claude-opus")
  with pytest.raises(ValueError) as exc_info:
    config.require_backends(cfg)
  assert f"{tmp_path / 'config.yaml'}: backends.options[2] repeats id 'claude-opus'" in str(exc_info.value)

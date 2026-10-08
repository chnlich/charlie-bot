"""Schema and loader gates for the sectioned CharlieBotConfig."""

import asyncio
import json
from pathlib import Path

import pytest
import yaml
from conftest import ROOT, backend_option

from src.app import registrations
from src.infra import config as config_module
from src.infra.config import CHARLIEBOT_HOME_ENV, CharlieBotConfig, require_backends
from src.runtime.hooks import wiring
from src.runtime.init_seed import init_charliebot_home


@pytest.mark.parametrize("fragment_name", ["x.yaml", "cron.yaml"])
def test_config_d_fragments_are_rejected(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fragment_name: str) -> None:
  home = tmp_path / "home"
  (home / "config.d").mkdir(parents=True)
  (home / "config.yaml").write_text("server:\n  host: 127.0.0.1\n", encoding="utf-8")
  (home / "config.d" / fragment_name).write_text("voice:\n  engine: sherpa\n", encoding="utf-8")
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  with pytest.raises(ValueError) as excinfo:
    config_module.load_config()
  assert f"config.d/{fragment_name}" in str(excinfo.value)


def _credentials_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """A temp CHARLIEBOT_HOME with a minimal valid sectioned config.yaml; returns the home path."""
  home = tmp_path / "home"
  home.mkdir()
  (home / "config.yaml").write_text("server:\n  port: 2001\n", encoding="utf-8")
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  return home


def test_credentials_stay_out_of_config_and_get_returns_each_sentinel(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
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
  dumped = json.dumps(config_module.load_config().model_dump(mode="json"))
  credentials = config_module.load_credentials()
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
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, body: str, fragment: str) -> None:
  home = _credentials_home(tmp_path, monkeypatch)
  (home / "credentials.yaml").write_text(body, encoding="utf-8")
  with pytest.raises(ValueError) as excinfo:
    config_module.load_credentials()
  assert fragment in str(excinfo.value)


EXAMPLE_PATH = ROOT / "configs" / "config.example.yaml"

STARTER_BACKEND_IDS = ["claude-fable", "claude-opus", "claude-sonnet"]


def test_example_config_loads_to_the_model_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The shipped example is the default config: loading it equals constructing
  CharlieBotConfig, modulo backends.options (the example ships the three starter
  entries where the model default is empty)."""
  home = tmp_path / "home"
  home.mkdir()
  (home / "config.yaml").write_bytes(EXAMPLE_PATH.read_bytes())
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  loaded = config_module.load_config()
  loaded_dump = loaded.model_dump()
  default_dump = CharlieBotConfig(charliebot_home=home).model_dump()
  loaded_dump["backends"]["options"] = None
  default_dump["backends"]["options"] = None
  assert loaded_dump == default_dump
  assert [option.id for option in loaded.backends.options] == STARTER_BACKEND_IDS


def test_init_charliebot_home_seeds_config_and_credentials(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A fresh home gets config.yaml byte-equal to the example and credentials.yaml
  from the repo template, owner-readable only, loading as empty sections."""
  home = tmp_path / "home"
  home.mkdir()
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  fake_cfg = CharlieBotConfig(charliebot_home=home)
  monkeypatch.setattr("src.infra.config.get_config", lambda: fake_cfg)
  asyncio.run(init_charliebot_home())
  credentials_path = home / "credentials.yaml"
  assert credentials_path.exists()
  assert credentials_path.stat().st_mode & 0o777 == 0o600
  assert config_module.load_credentials().sections == {}
  assert (home / "config.yaml").read_bytes() == EXAMPLE_PATH.read_bytes()


def test_require_backends_rejects_empty_list() -> None:
  """An empty backends.options raises ValueError naming the key and the example file."""
  with pytest.raises(ValueError) as exc_info:
    require_backends(CharlieBotConfig())
  message = str(exc_info.value)
  assert "backends.options" in message
  assert "config.example.yaml" in message


# ---------------------------------------------------------------------------
# Claude account pools (accounts.claude_pools x backends.options.account_pool)
# ---------------------------------------------------------------------------


def _pooled_config(pools: dict[str, list[str]], *options: dict) -> CharlieBotConfig:
  """A config with accounts.claude entries a-e and the given pools and raw backend options."""
  accounts = [{"label": label, "config_dir": f"/tmp/claude-{label}"} for label in "abcde"]
  return CharlieBotConfig(accounts={"claude": accounts, "claude_pools": pools}, backends={"options": list(options)})


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


def _family_backends(tmp_path: Path, preference: list[str], *ids: str) -> CharlieBotConfig:
  """A config at *tmp_path* whose options carry *ids* (cc-claude) and the given preference."""
  options = [backend_option(id=backend_id, label=backend_id, type="cc-claude", model="m") for backend_id in ids]
  return CharlieBotConfig(charliebot_home=tmp_path, backends={"preference": preference, "options": options})


def test_require_backends_accepts_known_preference_entries(tmp_path: Path) -> None:
  cfg = _family_backends(tmp_path, ["claude-sonnet", "claude-opus"], "claude-opus", "claude-sonnet")
  assert require_backends(cfg) is None


def test_require_backends_rejects_unknown_preference_entry(tmp_path: Path) -> None:
  """A preference entry naming no option id stops startup; the error names the file, the entry
  and the id."""
  cfg = _family_backends(tmp_path, ["claude-opus", "codex-gpt-5.6-luna"], "claude-opus")
  with pytest.raises(ValueError) as exc_info:
    require_backends(cfg)
  message = str(exc_info.value)
  assert f"{tmp_path / 'config.yaml'}: backends.preference[1] names unknown backend 'codex-gpt-5.6-luna'" in message
  assert "claude-opus'" not in message


def _home_with_cron_tasks(home: Path, cron_tasks: dict[str, dict]) -> CharlieBotConfig:
  """Write a config.yaml whose only backend option is claude-opus plus one cron.d file per task
  in *cron_tasks*, point CHARLIEBOT_HOME at it (the profile_home fixture), and load the config.

  Each task, or each step of a chained task, gets the one prompt file the loader requires;
  the assert fails the test when a task file is rejected, which would pass the checks vacuously.
  """
  option = {"id": "claude-opus", "label": "claude-opus", "type": "cc-claude", "model": "m"}
  (home / "config.yaml").write_text(yaml.safe_dump({"backends": {"options": [option]}}), encoding="utf-8")
  prompt_file = home / "prompt.md"
  prompt_file.write_text("run\n", encoding="utf-8")
  cron_d = home / "config.d" / "cron.d"
  cron_d.mkdir(parents=True)
  for name, body in cron_tasks.items():
    task = {"cron": "0 * * * *", **body}
    if "steps" in task:
      task["steps"] = [{"prompt_file": str(prompt_file), **step} for step in task["steps"]]
    else:
      task["prompt_file"] = str(prompt_file)
    (cron_d / f"{name}.yaml").write_text(yaml.safe_dump(task), encoding="utf-8")
  cfg = config_module.load_config()
  assert {task.name for task in config_module.get_scheduled_tasks()} == set(cron_tasks)
  return cfg


def _run_startup_checks(cfg: CharlieBotConfig) -> None:
  registrations.register_all()
  for check in wiring.startup_checks():
    check(cfg)


def test_cron_backend_references_that_name_option_ids_pass_the_startup_checks(profile_home: Path) -> None:
  """A cron task or step backend naming an option id passes, and a task or step without a
  backend stays valid."""
  cfg = _home_with_cron_tasks(
      profile_home, {
          "pinned": {
              "backend": "claude-opus"
          },
          "unpinned": {},
          "chain": {
              "steps": [{
                  "name": "a",
                  "backend": "claude-opus"
              }, {
                  "name": "b"
              }]
          },
      })
  require_backends(cfg)
  _run_startup_checks(cfg)


@pytest.mark.parametrize(
    ("body", "line"), [
        ({
            "backend": "charlie-code-kimi-k3"
        }, "backend names unknown backend 'charlie-code-kimi-k3'"),
        (
            {
                "steps": [{
                    "name": "build",
                    "backend": "claude-opus-5"
                }]
            }, "steps 'build' backend names unknown backend 'claude-opus-5'"),
    ])
def test_cron_backend_naming_no_option_id_stops_the_startup_checks_not_the_load(
    profile_home: Path, body: dict, line: str) -> None:
  """load_config and require_backends accept the cron task, and the registered startup checks
  stop the start; the error line names the task's cron.d file, the entry and the id."""
  cfg = _home_with_cron_tasks(profile_home, {"nightly": body})
  require_backends(cfg)
  with pytest.raises(ValueError) as exc_info:
    _run_startup_checks(cfg)
  assert f"{profile_home / 'config.d' / 'cron.d' / 'nightly.yaml'}: {line}" in str(exc_info.value)


def test_require_backends_rejects_duplicate_option_id(tmp_path: Path) -> None:
  """An option id listed twice stops startup; the error names the file, the repeated entry and
  the id."""
  cfg = _family_backends(tmp_path, [], "claude-opus", "claude-sonnet", "claude-opus")
  with pytest.raises(ValueError) as exc_info:
    require_backends(cfg)
  assert f"{tmp_path / 'config.yaml'}: backends.options[2] repeats id 'claude-opus'" in str(exc_info.value)

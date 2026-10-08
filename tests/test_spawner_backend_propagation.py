from pathlib import Path

import pytest
from conftest import (
    AGY_BACKEND_OPTION,
    CODEX_BACKEND_OPTION,
    OPUS_BACKEND_ID,
    backend_option,
)

from src.infra.config import CharlieBotConfig
from src.runtime import spawner


def _build_cfg() -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={
          "options":
              [
                  backend_option(
                      id=OPUS_BACKEND_ID,
                      label="Opus",
                      type="cc-claude",
                      model="claude-opus-4-6",
                      effort="max",
                      cli_binary="claude-alt",
                  ),
                  CODEX_BACKEND_OPTION,
              ]
      },
  )


def test_resolve_backend_option_requires_valid_backend_and_model() -> None:
  cfg = _build_cfg()
  opt = spawner.spawner_backends.resolve_backend_option(cfg, OPUS_BACKEND_ID, "claude-opus-4-6")
  assert opt.id == OPUS_BACKEND_ID
  assert opt.model == "claude-opus-4-6"
  assert opt.effort == "max"
  assert opt.cli_binary == "claude-alt"

  with pytest.raises(ValueError, match=r"is not in backends.options"):
    spawner.spawner_backends.resolve_backend_option(cfg, "missing", "o3")

  with pytest.raises(ValueError, match="model is required"):
    spawner.spawner_backends.resolve_backend_option(cfg, "codex-o3", "")


def test_resolve_backend_option_allows_antigravity_missing_model() -> None:
  cfg = CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={"options": [AGY_BACKEND_OPTION,]},
  )

  opt = spawner.spawner_backends.resolve_backend_option(cfg, "agy", None)

  assert opt.id == "agy"
  assert opt.model is None


@pytest.mark.parametrize(
    "backend_type, entry_kwargs",
    [
        ("cc-claude", {}),
        ("cc-kimi", {
            "credential": "test-kimi"
        }),
        ("cc-openai-compatible", {
            "api_base": "https://api.test/v1"
        }),
        ("codex", {}),
        ("charlie-code", {}),
        ("gemini", {}),
        ("opencode", {}),
    ],
)
def test_resolve_backend_option_rejects_missing_model_for_model_required_backends(
    backend_type: str, entry_kwargs: dict) -> None:
  # The sectioned schema rejects a model-less model-required entry outright, so
  # each config entry carries a model and the resolver's own None rejection is
  # what the test drives.
  cfg = CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={
          "options":
              [
                  backend_option(
                      id=backend_type, label=backend_type, type=backend_type, model="fake-model", **entry_kwargs),
              ]
      },
  )

  with pytest.raises(ValueError, match="model is required"):
    spawner.spawner_backends.resolve_backend_option(cfg, backend_type, None)

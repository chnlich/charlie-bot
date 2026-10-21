import pathlib

import conftest
import pytest

from src.infra import config
from src.runtime import spawner_backends


def _build_cfg() -> config.CharlieBotConfig:
  return config.CharlieBotConfig(
      charliebot_home=pathlib.Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={
          "options":
              [
                  conftest.backend_option(
                      id=conftest.OPUS_BACKEND_ID,
                      label="Opus",
                      type="cc-claude",
                      model="claude-opus-4-6",
                      effort="max",
                      cli_binary="claude-alt",
                  ),
                  conftest.CODEX_BACKEND_OPTION,
              ]
      },
  )


def test_resolve_backend_option_requires_valid_backend_and_model() -> None:
  cfg = _build_cfg()
  opt = spawner_backends.resolve_backend_option(cfg, conftest.OPUS_BACKEND_ID, "claude-opus-4-6")
  assert opt.id == conftest.OPUS_BACKEND_ID
  assert opt.model == "claude-opus-4-6"
  assert opt.effort == "max"
  assert opt.cli_binary == "claude-alt"

  with pytest.raises(ValueError, match=r"is not in backends.options"):
    spawner_backends.resolve_backend_option(cfg, "missing", "o3")

  with pytest.raises(ValueError, match="model is required"):
    spawner_backends.resolve_backend_option(cfg, "codex-o3", "")


@pytest.mark.parametrize(
    "backend_type, entry_kwargs",
    [
        ("cc-claude", {}),
        ("cc-anthropic-compatible", {
            "api_base": "https://api.test",
            "context_window": 409600,
        }),
        ("cc-openai-compatible", {
            "api_base": "https://api.test/v1"
        }),
        ("codex", {}),
        ("charlie-code", {}),
        ("gemini", {}),
        ("opencode", {}),
        ("antigravity", {}),
    ],
)
def test_resolve_backend_option_rejects_missing_model_for_model_required_backends(
    backend_type: str, entry_kwargs: dict) -> None:
  # The sectioned schema rejects a model-less model-required entry outright, so
  # each config entry carries a model and the resolver's own None rejection is
  # what the test drives.
  cfg = config.CharlieBotConfig(
      charliebot_home=pathlib.Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={
          "options":
              [
                  conftest.backend_option(
                      id=backend_type, label=backend_type, type=backend_type, model="fake-model", **entry_kwargs),
              ]
      },
  )

  with pytest.raises(ValueError, match="resolved model is required"):
    spawner_backends.resolve_backend_option(cfg, backend_type, "")

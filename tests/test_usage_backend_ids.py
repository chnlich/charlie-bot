"""Tests for usage's startup check (src/features/usage/backend_ids.py): every backend option id
attributes to its type's usage source.

The check reads the real registry the suite's conftest fills through ``registrations.register_all()``,
so a type's attribution and registered prefixes are the ones the server would read at start.
"""

from __future__ import annotations

import pathlib
from typing import Any

import conftest
import pytest

from src.features.usage import backend_ids
from src.infra import config
from src.runtime.hooks import wiring


def _cfg(tmp_path: pathlib.Path, *entries: dict[str, Any]) -> config.CharlieBotConfig:
  return config.CharlieBotConfig(
      charliebot_home=tmp_path / "home", backends={"options": [conftest.backend_option(**entry) for entry in entries]})


def test_an_id_per_source_passes_whatever_prefix_spelling_it_uses(tmp_path: pathlib.Path) -> None:
  """One option per source uses the long or short prefix and includes the configured model."""
  cfg = _cfg(
      tmp_path,
      {
          "id": "codex-gpt-sol",
          "label": "Codex",
          "type": "codex",
          "model": "gpt-6.1-sol"
      },
      {
          "id": "claude-opus-5",
          "label": "Opus",
          "type": "cc-claude",
          "model": "claude-opus-5-5"
      },
      {
          "id": "oc-kimi-k3",
          "label": "OC Kimi",
          "type": "opencode",
          "model": "kimi-k3"
      },
      {
          "id": "clc-glm5-flash",
          "label": "CLC GLM",
          "type": "charlie-code",
          "model": "glm"
      },
      {
          "id": "agy-antigravity",
          "label": "Antigravity",
          "type": "antigravity",
          "model": "gemini-2.5-pro"
      },
  )
  backend_ids.check_backend_ids(cfg)


def test_an_id_outside_its_type_prefixes_is_rejected_and_every_violation_is_listed(tmp_path: pathlib.Path) -> None:
  """Each offending line names the config file, the options index, the id and the allowed
  prefixes; one error carries every violation."""
  cfg = _cfg(
      tmp_path,
      {
          "id": "codex-gpt-sol",
          "label": "Codex",
          "type": "codex",
          "model": "gpt-6.1-sol"
      },
      {
          "id": "glm-air",
          "label": "GLM",
          "type": "charlie-code",
          "model": "glm-air"
      },
      {
          "id": "kimi-oc",
          "label": "Kimi",
          "type": "opencode",
          "model": "kimi-k3"
      },
  )
  with pytest.raises(ValueError) as excinfo:
    backend_ids.check_backend_ids(cfg)
  message = str(excinfo.value)
  assert f"{cfg.config_file}: backends.options[1] id 'glm-air' type 'charlie-code'" in message
  assert "('charlie-code-', 'clc-')" in message
  assert "backends.options[2] id 'kimi-oc' type 'opencode'" in message
  assert "('opencode-', 'oc-')" in message
  assert "backends.options[0]" not in message  # the passing codex entry is not listed


def test_a_type_with_no_usage_source_is_rejected(tmp_path: pathlib.Path) -> None:
  """gemini registers no usage source, so an option of its type cannot pass: attribution would
  page-500 once the id left the config."""
  cfg = _cfg(tmp_path, {"id": "gem-pro", "label": "Gemini", "type": "gemini", "model": "gemini-2"})
  with pytest.raises(ValueError,
                     match=r"backends\.options\[0\] id 'gem-pro' type 'gemini' attributes to no usage source"):
    backend_ids.check_backend_ids(cfg)


def test_the_check_is_registered_with_the_wiring() -> None:
  """The server runs the registered checks at start, after require_backends; the registration
  is what puts this one in that loop."""
  assert backend_ids.check_backend_ids in wiring.startup_checks()

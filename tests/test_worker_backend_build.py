"""Worker._build_backend: the two construction contracts, family level.

``_build_backend`` makes one registry construction call; ``on_spawn`` picks
the failure policy wrapped around it:

- ``on_spawn=None`` (restart recovery's drain) builds a translate-only parser.
  It must succeed for every backend type the registry can build, even on a host
  where the backend's CLI binary is absent — construction failures degrade to
  the method's identity-translate fallback, never raise.
- ``on_spawn=<callable>`` (a real run) builds a launcher. It must keep failing
  loudly in the constructor for the types that resolve their CLI binary in
  ``__init__``; a real run never silently degrades to another backend.
"""

from pathlib import Path
from typing import Any

import pytest
from conftest import backend_option, stub_credentials

from src.agents.worker import Worker
from src.core.config import CharlieBotConfig
from src.core.models import ThreadMetadata

# The types that resolve a CLI binary in __init__ (via resolve_binary); the
# other four never do and therefore never raise FileNotFoundError on build.
BINARY_RESOLVING_TYPES = ["opencode", "antigravity", "codex", "gemini", "charlie-code"]

# Each binary-resolving backend reads resolve_binary through one module attribute:
# four bind it at import scope (their own namespace), and gemini_cli reads the base
# module's attribute at call time (`base.resolve_binary`), so hiding a binary means
# patching each reader's own attribute, not one shared name.
_RESOLVER_PATCH_TARGETS = [
    "src.agents.backends.opencode.resolve_binary",
    "src.agents.backends.antigravity_cli.resolve_binary",
    "src.agents.backends.codex.resolve_binary",
    "src.agents.backends.base.resolve_binary",
    "src.agents.backends.charlie_code.resolve_binary",
]


def _hide_all_binaries(monkeypatch: pytest.MonkeyPatch) -> None:
  """Make every agent CLI binary unresolvable, independent of host install state."""

  def _missing(name: str, fallback_dir: str) -> str:
    raise FileNotFoundError(f"{name} binary not found on PATH or at {Path(fallback_dir) / name}")

  for target in _RESOLVER_PATCH_TARGETS:
    monkeypatch.setattr(target, _missing)


# Sections the registry reads secrets from: cc-kimi resolves its api_key under
# its own credential section, cc-openai-compatible under charliebot.access_key.
_CREDENTIAL_SECTIONS = {"test-kimi": {"api_key": "test-key"}, "charliebot": {"access_key": "test-key"}}


def _worker(tmp_path: Path, backend_type: str) -> Worker:
  stub_credentials(_CREDENTIAL_SECTIONS)
  option_kwargs: dict[str, Any] = {"id": "opt", "label": "Opt", "type": backend_type, "model": "test-model"}
  if backend_type in ("cc-openai-compatible", "charlie-code"):
    option_kwargs["api_base"] = "http://test.invalid"  # charlie-code requires it (validated before its binary)
  if backend_type == "cc-kimi":
    option_kwargs["credential"] = "test-kimi"
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
  )
  return Worker(
      thread_metadata=ThreadMetadata(session_id="sess-1", description="test"),
      working_dir=tmp_path / "work",
      events_log_path=tmp_path / "data" / "events.jsonl",
      task_description="test",
      cfg=cfg,
      backend_option=backend_option(**option_kwargs),
  )


async def _on_spawn(pid: int) -> None:
  del pid


@pytest.mark.parametrize("backend_type", BINARY_RESOLVING_TYPES)
def test_launcher_build_still_fails_without_binaries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, backend_type: str) -> None:
  """on_spawn=<callable>: binary-resolving types still raise FileNotFoundError."""
  _hide_all_binaries(monkeypatch)
  with pytest.raises(FileNotFoundError):
    _worker(tmp_path, backend_type)._build_backend(_on_spawn)

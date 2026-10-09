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

import pathlib
from typing import Any

import conftest
import pytest

from src.infra import config, models
from src.runtime import worker

# The types that resolve a CLI binary in __init__ (via resolve_binary); the
# other four never do and therefore never raise FileNotFoundError on build.
BINARY_RESOLVING_TYPES = ["opencode", "antigravity", "codex", "gemini", "charlie-code"]

# Every binary-resolving backend reads resolve_binary as base.resolve_binary at
# call time, so hiding a binary means patching that one shared attribute.
_RESOLVER_PATCH_TARGETS = ["src.runtime.agent_process.base.resolve_binary"]


def _hide_all_binaries(monkeypatch: pytest.MonkeyPatch) -> None:
  """Make every agent CLI binary unresolvable, independent of host install state."""

  def _missing(name: str, fallback_dir: str) -> str:
    raise FileNotFoundError(f"{name} binary not found on PATH or at {pathlib.Path(fallback_dir) / name}")

  for target in _RESOLVER_PATCH_TARGETS:
    monkeypatch.setattr(target, _missing)


# Sections the registry reads secrets from: cc-kimi resolves its api_key under
# its own credential section, cc-openai-compatible under charliebot.access_key.
_CREDENTIAL_SECTIONS = {"test-kimi": {"api_key": "test-key"}, "charliebot": {"access_key": "test-key"}}


def _worker(tmp_path: pathlib.Path, backend_type: str) -> worker.Worker:
  conftest.stub_credentials(_CREDENTIAL_SECTIONS)
  option_kwargs: dict[str, Any] = {"id": "opt", "label": "Opt", "type": backend_type, "model": "test-model"}
  if backend_type in ("cc-openai-compatible", "charlie-code"):
    option_kwargs["api_base"] = "http://test.invalid"  # charlie-code requires it (validated before its binary)
  if backend_type == "cc-kimi":
    option_kwargs["credential"] = "test-kimi"
  cfg = config.CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
  )
  return worker.Worker(
      thread_metadata=models.ThreadMetadata(session_id="sess-1", description="test"),
      working_dir=tmp_path / "work",
      events_log_path=tmp_path / "data" / "events.jsonl",
      task_description="test",
      cfg=cfg,
      backend_option=conftest.backend_option(**option_kwargs),
  )


async def _on_spawn(pid: int) -> None:
  del pid


@pytest.mark.parametrize("backend_type", BINARY_RESOLVING_TYPES)
def test_launcher_build_still_fails_without_binaries(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, backend_type: str) -> None:
  """on_spawn=<callable>: binary-resolving types still raise FileNotFoundError."""
  _hide_all_binaries(monkeypatch)
  with pytest.raises(FileNotFoundError):
    _worker(tmp_path, backend_type)._build_backend(_on_spawn)

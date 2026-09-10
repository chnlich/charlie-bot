"""Worker._build_backend: the two construction contracts, family level.

``_build_backend`` has two call shapes, distinguished by ``on_spawn``:

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

# Every backend type the registry can build (src/agents/backends/registry.py).
ALL_BACKEND_TYPES = [
    "cc-claude",
    "cc-kimi",
    "cc-openai-compatible",
    "codex",
    "charlie-code",
    "gemini",
    "opencode",
    "antigravity",
    "tui-cli",
]

# The types that resolve a CLI binary in __init__ (via resolve_binary); the
# other four never do and therefore never raise FileNotFoundError on build.
BINARY_RESOLVING_TYPES = ["opencode", "antigravity", "codex", "gemini", "charlie-code"]

# Each binary-resolving backend imports resolve_binary into its own module
# namespace, so hiding a binary means patching every module, not base.
_RESOLVER_MODULES = [
    "src.agents.backends.opencode",
    "src.agents.backends.antigravity_cli",
    "src.agents.backends.codex",
    "src.agents.backends.gemini_cli",
    "src.agents.backends.charlie_code",
]


def _hide_all_binaries(monkeypatch: pytest.MonkeyPatch) -> None:
  """Make every agent CLI binary unresolvable, independent of host install state."""

  def _missing(name: str, fallback_dir: str) -> str:
    raise FileNotFoundError(f"{name} binary not found on PATH or at {Path(fallback_dir) / name}")

  for module in _RESOLVER_MODULES:
    monkeypatch.setattr(f"{module}.resolve_binary", _missing)


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


@pytest.mark.parametrize("backend_type", ALL_BACKEND_TYPES)
def test_translate_only_build_succeeds_without_binaries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, backend_type: str) -> None:
  """on_spawn=None: construction never fails on a missing binary, for all types."""
  _hide_all_binaries(monkeypatch)
  backend = _worker(tmp_path, backend_type)._build_backend(None)
  assert backend is not None


@pytest.mark.parametrize("backend_type", BINARY_RESOLVING_TYPES)
def test_launcher_build_still_fails_without_binaries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, backend_type: str) -> None:
  """on_spawn=<callable>: binary-resolving types still raise FileNotFoundError."""
  _hide_all_binaries(monkeypatch)
  with pytest.raises(FileNotFoundError):
    _worker(tmp_path, backend_type)._build_backend(_on_spawn)


def test_translate_only_degrade_falls_back_to_identity_translate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The degrade lands on the method's binary-free fallback branch: a
  ClaudeCodeBackend (whose translate_event is base's identity implementation),
  and a warning keeps the environment problem diagnosable."""
  from structlog.testing import capture_logs

  from src.agents.backends.claude_code import ClaudeCodeBackend

  _hide_all_binaries(monkeypatch)
  worker = _worker(tmp_path, "opencode")
  with capture_logs() as logs:
    backend = worker._build_backend(None)

  assert isinstance(backend, ClaudeCodeBackend)
  event = {"type": "assistant", "message": {"content": [{"type": "text", "text": "x"}]}}
  assert backend.translate_event(event) == [event]

  warnings = [entry for entry in logs if entry.get("event") == "translate_backend_unresolved"]
  assert len(warnings) == 1
  assert warnings[0]["log_level"] == "warning"
  assert warnings[0]["thread_id"] == worker._thread.id
  assert warnings[0]["backend"] == "opt"
  assert warnings[0]["backend_type"] == "opencode"
  assert "opencode binary not found" in warnings[0]["error"]

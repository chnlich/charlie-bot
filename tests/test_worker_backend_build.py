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
from src.runtime import runs, worker
from src.runtime.hooks import backend_lifecycle

# The types that resolve a CLI binary in __init__ (via resolve_binary); the
# other three never do and therefore never raise FileNotFoundError on build.
BINARY_RESOLVING_TYPES = ["antigravity", "codex", "charlie-code"]


def _hide_all_binaries(monkeypatch: pytest.MonkeyPatch) -> None:
  """Make every agent CLI binary unresolvable, independent of host install state."""

  def _missing(name: str, fallback_dir: str) -> str:
    raise FileNotFoundError(f"{name} binary not found on PATH or at {pathlib.Path(fallback_dir) / name}")

  # Every binary-resolving backend reads resolve_binary as base.resolve_binary at call time, so
  # hiding a binary means patching that one shared attribute.
  monkeypatch.setattr(conftest.BASE_RESOLVE_BINARY_PATCH_TARGET, _missing)


# cc-openai-compatible resolves its proxy credential under charliebot.access_key.
_CREDENTIAL_SECTIONS = {"charliebot": {"access_key": "test-key"}}


def _worker(tmp_path: pathlib.Path, backend_type: str, instructions_content: str | None = None) -> worker.Worker:
  conftest.stub_credentials(_CREDENTIAL_SECTIONS)
  option_kwargs: dict[str, Any] = {"id": "opt", "label": "Opt", "type": backend_type, "model": "test-model"}
  if backend_type in ("cc-openai-compatible", "cc-anthropic-compatible", "charlie-code"):
    option_kwargs["api_base"] = "http://test.invalid"  # charlie-code requires it (validated before its binary)
  if backend_type == "cc-anthropic-compatible":
    option_kwargs["context_window"] = 409600
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
      instructions_content=instructions_content,
      run_kind="work",
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


def test_claude_resume_launch_still_passes_the_run_instructions_file(tmp_path: pathlib.Path) -> None:
  """A Claude worker launch that resumes a held conversation keeps --append-system-prompt-file."""
  launch = backend_lifecycle.Launch(backend_kwargs={}, resume_id="held-conversation")
  backend = _worker(tmp_path, "cc-claude", instructions_content="rules")._build_backend(_on_spawn, launch)

  cmd = backend._build_command("prompt")

  assert cmd[cmd.index("--resume") + 1] == "held-conversation"
  assert cmd[cmd.index("--append-system-prompt-file") + 1] == str(tmp_path / "data" / runs.HARNESS_INSTRUCTIONS_NAME)
  assert "--input-format" not in cmd
  assert "--replay-user-messages" not in cmd

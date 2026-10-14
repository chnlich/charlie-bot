"""_prepare_cwd instructions contract for the backends that write one.

``AgentBackend._prepare_cwd`` (src/runtime/agent_process/base.py) owns the write/skip
mechanics. A manager turn (no ``instructions_file``) writes the configured content into
the backend's file in the session cwd, named by ``_INSTRUCTIONS_TARGET``. A worker or
review run (with ``instructions_file``) writes the content into its Run directory and
leaves the worktree alone, and each backend passes that file to its CLI on every launch.
The rest of the surface stays in its natural home: antigravity's no-op pin in
test_antigravity_cli_backend.py, charlie_code's task.md composition in
test_charlie_code_backend.py.
"""

from __future__ import annotations

import pathlib
import tomllib

import conftest
import pytest

from src.backends.charlie_code import charlie_code
from src.backends.claude_code import claude_code
from src.backends.codex import codex
from src.backends.opencode import opencode
from src.runtime import runs
from src.runtime.agent_process import base

# (backend class, ctor kwargs, resolve_binary patch target or None, fake binary,
# instructions file name). Each CLI row is the backend's shared conftest rig
# (CLI_BACKEND_RIGS); claude_code takes its CLI binary through the cli_binary
# kwarg and resolves nothing, so it needs no patch target.
_INSTRUCTIONS_BACKENDS = [(claude_code.ClaudeCodeBackend, {}, None, "claude", "CLAUDE.md")]
for _cls in (charlie_code.CharlieCodeBackend, codex.CodexBackend, opencode.OpenCodeBackend):
  _patch_target, _fake_binary, _defaults = conftest.CLI_BACKEND_RIGS[_cls]
  _INSTRUCTIONS_BACKENDS.append((_cls, _defaults, _patch_target, _fake_binary, "AGENTS.md"))
_INSTRUCTIONS_BACKEND_IDS = [case[0].__name__ for case in _INSTRUCTIONS_BACKENDS]


def _build_backend(
    backend_cls: type[base.AgentBackend],
    ctor_kwargs: dict,
    patch_target: str | None,
    fake_binary: str,
    monkeypatch: pytest.MonkeyPatch,
    **kwargs: object,
) -> base.AgentBackend:
  if patch_target is None:
    return backend_cls(**ctor_kwargs, **kwargs)
  return conftest.build_cli_backend(monkeypatch, backend_cls, patch_target, fake_binary, defaults=ctor_kwargs, **kwargs)


@pytest.mark.parametrize(
    ("backend_cls", "ctor_kwargs", "patch_target", "fake_binary", "filename"),
    _INSTRUCTIONS_BACKENDS,
    ids=_INSTRUCTIONS_BACKEND_IDS,
)
def test_prepare_cwd_writes_instructions_file_when_provided(
    backend_cls: type[base.AgentBackend],
    ctor_kwargs: dict,
    patch_target: str | None,
    fake_binary: str,
    filename: str,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: pathlib.Path,
) -> None:
  content = f"# {backend_cls.__name__} instructions\nBuild stuff."
  backend = _build_backend(
      backend_cls, ctor_kwargs, patch_target, fake_binary, monkeypatch, instructions_content=content)

  backend._prepare_cwd(str(tmp_path))

  instructions_file = tmp_path / filename
  assert instructions_file.exists()
  assert instructions_file.read_text(encoding="utf-8") == content


@pytest.mark.parametrize(
    ("backend_cls", "ctor_kwargs", "patch_target", "fake_binary", "filename"),
    _INSTRUCTIONS_BACKENDS,
    ids=_INSTRUCTIONS_BACKEND_IDS,
)
def test_prepare_cwd_skips_instructions_file_when_unset(
    backend_cls: type[base.AgentBackend],
    ctor_kwargs: dict,
    patch_target: str | None,
    fake_binary: str,
    filename: str,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: pathlib.Path,
) -> None:
  backend = _build_backend(backend_cls, ctor_kwargs, patch_target, fake_binary, monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  assert not (tmp_path / filename).exists()


def test_opencode_prepare_cwd_writes_agents_md_even_when_config_exists(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  """AGENTS.md must be written even when opencode.json already exists (resumed sessions)."""
  patch_target, fake_binary, defaults = conftest.CLI_BACKEND_RIGS[opencode.OpenCodeBackend]
  backend = _build_backend(
      opencode.OpenCodeBackend, defaults, patch_target, fake_binary, monkeypatch, instructions_content="# Instructions")
  config_dir = tmp_path / ".opencode"
  config_dir.mkdir()
  (config_dir / "opencode.json").write_text("{}", encoding="utf-8")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Instructions"


# ---------------------------------------------------------------------------
# Worker and review runs: the instructions file lives in the Run directory.
# ---------------------------------------------------------------------------

_RUN_CONTENT = "# Harness rules\nDo the work."

# Text that exercises each TOML basic-string rule: quotes, a backslash, a tab, a newline,
# a control character, DEL, and non-ASCII.
_TOML_HOSTILE_CONTENT = 'quote " back\\slash\ttab\nnewline \x01 del \x7f cjk 规则 emoji 🙂'


def _run_layout(tmp_path: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path]:
  """The worktree a run works in, and the instructions file in its Run directory."""
  run_dir = tmp_path / "run"
  worktree = tmp_path / "worktree"
  run_dir.mkdir()
  worktree.mkdir()
  return worktree, run_dir / runs.HARNESS_INSTRUCTIONS_NAME


def _assert_no_guide_written(worktree: pathlib.Path) -> None:
  for name in ("CLAUDE.md", "AGENTS.md"):
    assert not (worktree / name).exists()


def _config_value(cmd: list[str], key: str) -> str:
  """The TOML value of the ``--config key=value`` override in *cmd*."""
  arg = next(arg for arg in cmd if arg.startswith(f"{key}="))
  return arg.removeprefix(f"{key}=")


@pytest.mark.parametrize(
    ("backend_cls", "ctor_kwargs", "patch_target", "fake_binary"),
    [case[:4] for case in _INSTRUCTIONS_BACKENDS],
    ids=_INSTRUCTIONS_BACKEND_IDS,
)
def test_run_instructions_land_in_run_dir_and_tracked_guides_stay_untouched(
    backend_cls: type[base.AgentBackend],
    ctor_kwargs: dict,
    patch_target: str | None,
    fake_binary: str,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: pathlib.Path,
) -> None:
  worktree, instructions = _run_layout(tmp_path)
  conftest.run_git(worktree, "init", "-q")
  conftest.run_git(worktree, "config", "user.email", "t@example.com")
  conftest.run_git(worktree, "config", "user.name", "t")
  (worktree / "CLAUDE.md").write_text("repo guide\n", encoding="utf-8")
  (worktree / "AGENTS.md").write_text("repo agents\n", encoding="utf-8")
  conftest.run_git(worktree, "add", "CLAUDE.md", "AGENTS.md")
  conftest.run_git(worktree, "commit", "-q", "-m", "guides")
  backend = _build_backend(
      backend_cls,
      ctor_kwargs,
      patch_target,
      fake_binary,
      monkeypatch,
      instructions_content=_RUN_CONTENT,
      instructions_file=instructions)

  backend._prepare_cwd(str(worktree))

  assert instructions.read_text(encoding="utf-8") == _RUN_CONTENT
  assert (worktree / "CLAUDE.md").read_bytes() == b"repo guide\n"
  assert (worktree / "AGENTS.md").read_bytes() == b"repo agents\n"
  assert conftest.run_git(worktree, "status", "--porcelain") == ""


def test_claude_worker_launch_passes_instructions_by_flag(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  worktree, instructions = _run_layout(tmp_path)
  backend = _build_backend(
      claude_code.ClaudeCodeBackend, {},
      None,
      "claude",
      monkeypatch,
      instructions_content=_RUN_CONTENT,
      instructions_file=instructions)

  backend._prepare_cwd(str(worktree))
  cmd = backend._build_command("prompt")

  assert cmd[cmd.index("--append-system-prompt-file") + 1] == str(instructions)
  _assert_no_guide_written(worktree)


def test_charlie_code_worker_launch_passes_instructions_by_flag(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  worktree, instructions = _run_layout(tmp_path)
  patch_target, fake_binary, defaults = conftest.CLI_BACKEND_RIGS[charlie_code.CharlieCodeBackend]
  backend = _build_backend(
      charlie_code.CharlieCodeBackend,
      defaults,
      patch_target,
      fake_binary,
      monkeypatch,
      instructions_content=_RUN_CONTENT,
      instructions_file=instructions)

  backend._prepare_cwd(str(worktree))
  backend._prepare_transport(instructions.parent)
  cmd = backend._build_command("prompt")

  assert cmd[cmd.index("--instructions-file") + 1] == str(instructions)
  _assert_no_guide_written(worktree)


def test_codex_worker_launch_passes_instructions_as_config(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  worktree, instructions = _run_layout(tmp_path)
  patch_target, fake_binary, defaults = conftest.CLI_BACKEND_RIGS[codex.CodexBackend]
  backend = _build_backend(
      codex.CodexBackend,
      defaults,
      patch_target,
      fake_binary,
      monkeypatch,
      instructions_content=_TOML_HOSTILE_CONTENT,
      instructions_file=instructions)

  backend._prepare_cwd(str(worktree))
  cmd = backend._build_command("prompt")

  # Codex parses each -c value as TOML: the instructions text must round-trip exactly.
  developer = tomllib.loads(f"value = {_config_value(cmd, 'developer_instructions')}")["value"]
  fallback = tomllib.loads(f"value = {_config_value(cmd, 'project_doc_fallback_filenames')}")["value"]
  assert developer == _TOML_HOSTILE_CONTENT
  assert fallback == ["CLAUDE.md"]
  _assert_no_guide_written(worktree)


def test_opencode_worker_prompt_is_framed_and_manager_prompt_stays_bare(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  patch_target, fake_binary, defaults = conftest.CLI_BACKEND_RIGS[opencode.OpenCodeBackend]
  worker = _build_backend(
      opencode.OpenCodeBackend,
      defaults,
      patch_target,
      fake_binary,
      monkeypatch,
      instructions_content=_RUN_CONTENT,
      instructions_file=tmp_path / runs.HARNESS_INSTRUCTIONS_NAME)
  manager = _build_backend(
      opencode.OpenCodeBackend, defaults, patch_target, fake_binary, monkeypatch, instructions_content=_RUN_CONTENT)

  assert worker._effective_prompt("task") == worker._frame_system_prompt(_RUN_CONTENT, "task")
  assert manager._effective_prompt("task") == "task"

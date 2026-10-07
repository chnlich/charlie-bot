"""_prepare_cwd instructions-file contract for the backends that write one.

``AgentBackend._prepare_cwd`` (src/runtime/agent_process/base.py) owns the write/skip
mechanics; each backend declares only its (filename, log event) pair as
``_INSTRUCTIONS_TARGET``. The parametrized cases drive each backend's
``_prepare_cwd`` so the wiring itself stays pinned: the configured instructions
content lands byte-identical in the backend's file inside the run cwd, and no
file is written when ``instructions_content`` is unset. The rest of the
surface stays in its natural home: antigravity's no-op pin in
test_antigravity_cli_backend.py, charlie_code's task.md composition in
test_charlie_code_backend.py.
"""

from __future__ import annotations

import pathlib

import conftest
import pytest

from src.backends.charlie_code import charlie_code
from src.backends.claude_code import claude_code
from src.backends.codex import codex
from src.backends.opencode import opencode
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

"""_prepare_cwd instructions-file contract for the backends that write one.

``AgentBackend._write_instructions_file`` (src/agents/backends/base.py) owns the
write/skip mechanics; each overriding backend pins only its (filename, log
event) pair. The parametrized cases drive each backend's own ``_prepare_cwd``
so the delegation itself stays pinned: the configured instructions content
lands byte-identical in the backend's file inside the run cwd, and no file is
written when ``instructions_content`` is unset. The rest of the surface stays
in its natural home: antigravity's no-op pin in test_antigravity_cli_backend.py,
charlie_code's task.md composition in test_charlie_code_backend.py.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import (
  CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
  CODEX_RESOLVE_BINARY_PATCH_TARGET,
  OPENCODE_RESOLVE_BINARY_PATCH_TARGET,
  build_cli_backend,
)

from src.agents.backends.base import AgentBackend
from src.agents.backends.charlie_code import CharlieCodeBackend
from src.agents.backends.claude_code import ClaudeCodeBackend
from src.agents.backends.codex import CodexBackend
from src.agents.backends.opencode import OpenCodeBackend

# (backend class, ctor kwargs, resolve_binary patch target or None, fake binary,
# instructions file name). ctor kwargs mirror each backend's per-file test rig;
# claude_code takes its CLI binary through the cli_binary kwarg and resolves
# nothing, so it needs no patch target.
_INSTRUCTIONS_BACKENDS = [
    (ClaudeCodeBackend, {}, None, "claude", "CLAUDE.md"),
    (CharlieCodeBackend, {"model": "charlie-code-test-model", "api_base": "http://test.invalid/v1"},
     CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/charlie-code", "AGENTS.md"),
    (CodexBackend, {"model": "codex-test-model"}, CODEX_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/codex", "AGENTS.md"),
    (OpenCodeBackend, {}, OPENCODE_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/opencode", "AGENTS.md"),
]
_INSTRUCTIONS_BACKEND_IDS = [case[0].__name__ for case in _INSTRUCTIONS_BACKENDS]


def _build_backend(
    backend_cls: type[AgentBackend],
    ctor_kwargs: dict,
    patch_target: str | None,
    fake_binary: str,
    monkeypatch: pytest.MonkeyPatch,
    **kwargs,
) -> AgentBackend:
  if patch_target is None:
    return backend_cls(**ctor_kwargs, **kwargs)
  return build_cli_backend(monkeypatch, backend_cls, patch_target, fake_binary, defaults=ctor_kwargs, **kwargs)


@pytest.mark.parametrize(
    ("backend_cls", "ctor_kwargs", "patch_target", "fake_binary", "filename"),
    _INSTRUCTIONS_BACKENDS,
    ids=_INSTRUCTIONS_BACKEND_IDS,
)
def test_prepare_cwd_writes_instructions_file_when_provided(
    backend_cls: type[AgentBackend],
    ctor_kwargs: dict,
    patch_target: str | None,
    fake_binary: str,
    filename: str,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
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
    backend_cls: type[AgentBackend],
    ctor_kwargs: dict,
    patch_target: str | None,
    fake_binary: str,
    filename: str,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  backend = _build_backend(backend_cls, ctor_kwargs, patch_target, fake_binary, monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  assert not (tmp_path / filename).exists()


def test_opencode_prepare_cwd_writes_agents_md_even_when_config_exists(monkeypatch, tmp_path: Path) -> None:
  """AGENTS.md must be written even when opencode.json already exists (resumed sessions)."""
  backend = _build_backend(
      OpenCodeBackend, {}, OPENCODE_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/opencode",
      monkeypatch, instructions_content="# Instructions")
  config_dir = tmp_path / ".opencode"
  config_dir.mkdir()
  (config_dir / "opencode.json").write_text("{}", encoding="utf-8")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Instructions"

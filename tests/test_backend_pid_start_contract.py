"""pid_start pin contract for every concrete AgentBackend subclass.

The restart reconcile proves a recorded turn's death from the (pid, pid_start,
started_at) triple persisted via the backend's on_spawn callback; without the
pin, death is unprovable and every restart judges the dead turn alive
(report-only limbo, user message excluded from replay). The shared
``AgentBackend.run()`` pins ``/proc/<pid>/stat`` field 22 onto
``backend.pid_start`` post-spawn and pre-``_on_spawn``; backends overriding
``run()`` with a custom spawn loop must pin it themselves, at the same point
and through the same ``runs.read_pid_stat`` attribute call (the attribute
form is what keeps the pin monkeypatchable here — it is load-bearing).

Enumeration is package-driven (pkgutil walk of ``src.agents.backends``), never
a hardcoded class list, and the fail-loud test below requires the enumerated
subclass set to equal the harness-key set EXACTLY: adding a concrete
AgentBackend subclass without a harness entry turns this file RED, so no
turn-driving backend can silently skip the pin contract. terminal/tui are not
AgentBackend subclasses and never enter the enumeration.
"""

from __future__ import annotations

import importlib
import inspect
import pkgutil
import sys
from collections.abc import Awaitable, Callable
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest
from conftest import (
    ANTIGRAVITY_RESOLVE_BINARY_PATCH_TARGET,
    OPENCODE_RESOLVE_BINARY_PATCH_TARGET,
)

import src.agents.backends as backends_package
from src.agents.backends.antigravity_cli import AntigravityCliBackend
from src.agents.backends.base import AgentBackend
from src.agents.backends.charlie_code import CharlieCodeBackend
from src.agents.backends.claude_code import AnthropicEndpointBackend, ClaudeCodeBackend
from src.agents.backends.codex import CodexBackend
from src.agents.backends.gemini_cli import GeminiCliBackend
from src.agents.backends.kimi import KimiBackend
from src.agents.backends.openai_compatible_claude import OpenAICompatibleClaudeBackend
from src.agents.backends.opencode import OpenCodeBackend

# (start_time_field, state) 2-tuple in read_pid_stat's shape; [0] must land on
# backend.pid_start and [1] must not leak into it.
_SENTINEL_STAT: tuple[str, str] = ("314159contract-start", "R")

# Minimal constructor kwargs per base-path class. The subprocess is mocked in
# the shared harness, so the resolved binary path only has to exist as a
# string; resolve_binary itself is patched where the constructor calls it.
_BASE_CTOR_KWARGS: dict[type[AgentBackend], dict] = {
    ClaudeCodeBackend: {},
    AnthropicEndpointBackend:
        {
            "base_url": "https://contract.invalid",
            "auth_token": "contract-token",
            "model": "contract-model",
        },
    KimiBackend: {
        "api_key": "contract-key",
        "model": "contract-model"
    },
    CharlieCodeBackend: {
        "model": "contract-model",
        "api_base": "https://contract.invalid"
    },
    CodexBackend: {
        "model": "contract-model"
    },
    GeminiCliBackend: {
        "model": "contract-model"
    },
    OpenAICompatibleClaudeBackend:
        {
            "proxy_base_url": "https://contract.invalid",
            "auth_token": "contract-token",
            "model": "contract-model",
        },
}

_BASE_PATH_CLASSES: tuple[type[AgentBackend], ...] = tuple(_BASE_CTOR_KWARGS)


class _SpawnObserved(Exception):
  """Control-flow marker: the on_spawn probe raises it to halt base.run() exactly at notification."""


def _enumerate_backend_classes() -> set[type[AgentBackend]]:
  """Every concrete AgentBackend subclass defined under src/agents/backends."""
  classes: set[type[AgentBackend]] = set()
  for module_info in pkgutil.iter_modules(backends_package.__path__):
    module = importlib.import_module(f"src.agents.backends.{module_info.name}")
    for _, cls in inspect.getmembers(module, inspect.isclass):
      if cls is AgentBackend or not issubclass(cls, AgentBackend) or inspect.isabstract(cls):
        continue
      classes.add(cls)
  return classes


def _install_sentinel_read_pid_stat(monkeypatch: pytest.MonkeyPatch) -> None:
  """Stub the /proc read to the sentinel pair; the pin must copy [0] onto pid_start."""
  monkeypatch.setattr("src.core.runs.read_pid_stat", lambda pid: _SENTINEL_STAT)


async def _drive_base_path(cls, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """Shared-path harness: drive the inherited base.run() with a mocked subprocess.

  The on_spawn probe records backend.pid_start at notification time and then
  raises _SpawnObserved, so the run halts before the tail-follow loop — the
  notification point is the contract surface, everything after it is shared
  machinery already covered by base.run's own tests.
  """
  _install_sentinel_read_pid_stat(monkeypatch)
  module = sys.modules[cls.__module__]
  if "resolve_binary" in vars(module):  # some constructors resolve their CLI eagerly
    monkeypatch.setattr(f"{cls.__module__}.resolve_binary", lambda name, fallback: "/usr/bin/true")
  observed: list[tuple[int, str | None]] = []
  backend: AgentBackend

  async def on_spawn(pid: int) -> None:
    observed.append((pid, backend.pid_start))
    raise _SpawnObserved

  backend = cls(on_spawn=on_spawn, log_dir=tmp_path / "logs", **_BASE_CTOR_KWARGS[cls])
  process = MagicMock()
  process.pid = 31337
  process.stdin = MagicMock()
  process.stdin.drain = AsyncMock()
  process.stdin.wait_closed = AsyncMock()
  monkeypatch.setattr("src.agents.backends.base.asyncio.create_subprocess_exec", AsyncMock(return_value=process))

  with pytest.raises(_SpawnObserved):
    async for _event in backend.run("contract prompt", str(tmp_path), {"PATH": "/usr/bin:/bin"}):
      pass

  if backend._stdin_task is not None:
    await backend._stdin_task
  assert observed == [(31337, _SENTINEL_STAT[0])]
  assert backend.pid_start == _SENTINEL_STAT[0]


async def _drive_opencode(cls, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """opencode custom run() harness (test_opencode_backend.py's MagicMock spawn shape)."""
  monkeypatch.setattr(OPENCODE_RESOLVE_BINARY_PATCH_TARGET, lambda name, fallback: "/usr/bin/opencode")
  _install_sentinel_read_pid_stat(monkeypatch)
  observed: list[tuple[int, str | None]] = []
  backend: AgentBackend

  async def on_spawn(pid: int) -> None:
    observed.append((pid, backend.pid_start))

  backend = cls(model="provider/model", on_spawn=on_spawn)
  process = MagicMock()
  process.pid = 1234
  create_process = AsyncMock(return_value=process)
  monkeypatch.setattr("src.agents.backends.opencode.asyncio.create_subprocess_exec", create_process)
  monkeypatch.setattr(backend, "_read_server_url", AsyncMock(side_effect=RuntimeError("stop after spawn")))
  monkeypatch.setattr(backend, "_stream_stderr", AsyncMock())
  monkeypatch.setattr(backend, "_cleanup_server", AsyncMock())

  _events = [event async for event in backend.run("contract prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  assert observed == [(1234, _SENTINEL_STAT[0])]
  assert backend.pid_start == _SENTINEL_STAT[0]
  assert backend._stderr_task is not None
  await backend._stderr_task


async def _drive_antigravity(cls, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """antigravity custom run() harness (test_antigravity_cli_backend.py's real-script shape)."""
  fake_agy = tmp_path / "agy"
  fake_agy.write_text(
      "#!/bin/sh\nprintf '%s\\n' '{\"status\":\"SUCCESS\",\"conversation_id\":\"conv-abc\",\"response\":\"contract answer\",\"usage\":{}}'\n",
      encoding="utf-8")
  fake_agy.chmod(0o755)
  monkeypatch.setattr(ANTIGRAVITY_RESOLVE_BINARY_PATCH_TARGET, lambda name, fallback: str(fake_agy))
  _install_sentinel_read_pid_stat(monkeypatch)
  observed: list[tuple[int, str | None]] = []
  backend: AgentBackend

  async def on_spawn(pid: int) -> None:
    observed.append((pid, backend.pid_start))

  backend = cls(on_spawn=on_spawn)

  async for _event in backend.run("contract prompt", str(tmp_path), {"PATH": "/usr/bin:/bin"}):
    pass

  assert len(observed) == 1
  assert observed[0][1] == _SENTINEL_STAT[0]
  assert backend.pid_start == _SENTINEL_STAT[0]


HarnessFn = Callable[[type[AgentBackend], pytest.MonkeyPatch, Path], Awaitable[None]]

# Harness-by-class map: the six base-path classes share one harness driving the
# inherited base.run(); opencode and antigravity each drive their own run().
# This map is the ONLY opt-in — enumeration does not consult it.
HARNESSES: dict[type[AgentBackend], HarnessFn] = {
    **dict.fromkeys(_BASE_PATH_CLASSES, _drive_base_path),
    OpenCodeBackend: _drive_opencode,
    AntigravityCliBackend: _drive_antigravity,
}


def test_every_backend_subclass_has_a_harness() -> None:
  """Fail-loud: a concrete AgentBackend subclass without a harness entry is RED here.

  The union of both directions is asserted at once: a new backend lacking a
  harness (uncovered pin contract) or a stale harness entry for a removed
  backend (dead code) both fail — fix by adding/removing the harness entry,
  never by exempting.
  """
  assert _enumerate_backend_classes() == set(HARNESSES)


@pytest.mark.asyncio
@pytest.mark.parametrize("cls", sorted(HARNESSES, key=lambda c: c.__name__), ids=lambda c: c.__name__)
async def test_pid_start_pinned_at_spawn_notification(cls, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """One run() per backend: pid_start == sentinel[0] when _on_spawn fires."""
  await HARNESSES[cls](cls, monkeypatch, tmp_path)

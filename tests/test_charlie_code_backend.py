from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest
from conftest import (
    CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
    FLAG_LIKE_PROMPT,
    build_cli_backend,
)
from pydantic import ValidationError

from src.agents.backends.base import USER_LOCAL_BIN, AgentBackend
from src.agents.backends.charlie_code import CharlieCodeBackend
from src.agents.backends.registry import build_backend
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption


def _build_backend(monkeypatch, **kwargs) -> CharlieCodeBackend:
  return build_cli_backend(
      monkeypatch,
      CharlieCodeBackend,
      CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
      "/usr/bin/charlie-code",
      defaults={
          "model": "charlie-code-test-model",
          "api_base": "http://test.invalid/v1"
      },
      **kwargs,
  )


def test_translate_success_stream_preserves_tool_pair_ids_and_usage(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  command = backend.translate_event({
      "type": "command",
      "step": 1,
      "id": "s-1",
      "command": "pwd",
  })
  assert command == [{
      "type": ET.TOOL_USE,
      "name": "Bash",
      "input": {
          "command": "pwd"
      },
      "id": "s-1",
  }]

  observation = backend.translate_event(
      {
          "type": "observation",
          "step": 1,
          "id": "s-1",
          "returncode": 0,
          "output": "/tmp/worktree\n",
      })
  assert observation == [
      {
          "type": ET.TOOL_RESULT,
          "tool_name": "Bash",
          "content": "/tmp/worktree\n",
          "tool_use_id": "s-1",
      }
  ]

  result = backend.translate_event(
      {
          "type": "result",
          "completed": True,
          "n_steps": 1,
          "usage": {
              "n_calls": 2,
              "input_tokens": 123,
              "output_tokens": 45,
          },
      })
  assert result == [
      {
          "type": ET.RESULT,
          "result": "",
          "usage":
              {
                  "input_tokens": 123,
                  "output_tokens": 45,
                  "cache_read_input_tokens": 0,
                  "cache_creation_input_tokens": 0,
              },
          "total_cost_usd": None,
      }
  ]


def test_translate_failure_stream_preserves_error_message(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "error",
      "message": "rate limit: retry later",
  })

  assert translated == [
      {
          "type": ET.ERROR,
          "message": "rate limit: retry later",
          "content": "rate limit: retry later",
      }
  ]


def test_translate_thought_and_unknown(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend.translate_event({
      "type": "thought",
      "step": 1,
      "text": "I will inspect the files.",
  }) == [{
      "type": ET.ASSISTANT,
      "message": {
          "content": [{
              "type": "text",
              "text": "I will inspect the files.",
          }]
      },
  }]
  assert not backend.translate_event({"type": "future-event"})


def test_translate_compact_event_and_unknown_still_dropped(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event(
      {
          "type": "compact",
          "step": 12,
          "layer": "mask",
          "trigger": "threshold",
          "pre_tokens": 85196,
          "post_tokens_est": 27400,
      })

  assert translated == [
      {
          "type": ET.SYSTEM,
          "subtype": ET.COMPACT_BOUNDARY,
          ET.COMPACT_METADATA: {
              "trigger": "threshold",
              "pre_tokens": 85196,
          },
      }
  ]
  assert backend.translate_event({"type": "future-event"}) == []


def test_translate_session_event(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend.translate_event({"type": "session", "session_id": "session-X"}) == [{"session_id": "session-X"}]


def test_build_command_writes_task_file_and_flags(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, instructions_content="Use concise answers.")
  session_cwd = tmp_path / "cwd"
  session_cwd.mkdir()
  backend._prepare_cwd(str(session_cwd))
  backend._prepare_transport(tmp_path)

  cmd = backend._build_command(FLAG_LIKE_PROMPT)

  assert cmd[:6] == [
      "/usr/bin/charlie-code",
      "--json",
      "--model",
      "charlie-code-test-model",
      "--api-base",
      "http://test.invalid/v1",
  ]
  assert cmd[-2:] == ["--task-file", str(tmp_path / "task.md")]
  assert "--" not in cmd
  assert "--resume" not in cmd
  assert "--context-window" not in cmd
  # Master instructions ride the cwd AGENTS.md system channel, byte-identical
  # to the assembled instructions string.
  agents_md = session_cwd / "AGENTS.md"
  assert agents_md.read_bytes() == "Use concise answers.".encode("utf-8")
  # task.md carries the bare prompt: no <system-instructions> frame anywhere.
  task_md = tmp_path / "task.md"
  assert task_md.read_bytes() == FLAG_LIKE_PROMPT.encode("utf-8")
  assert b"<system-instructions>" not in task_md.read_bytes()
  # The task text never rides argv.
  assert not any(FLAG_LIKE_PROMPT in arg for arg in cmd)


def test_no_instructions_skips_agents_md_and_writes_bare_task_file(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)
  session_cwd = tmp_path / "cwd"
  session_cwd.mkdir()
  backend._prepare_transport(tmp_path)

  backend._prepare_cwd(str(session_cwd))
  backend._build_command(FLAG_LIKE_PROMPT)

  assert not (session_cwd / "AGENTS.md").exists()
  assert (tmp_path / "task.md").read_bytes() == FLAG_LIKE_PROMPT.encode("utf-8")


def test_build_command_resume_passes_raw_prompt_and_resume_flag(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(
      monkeypatch,
      instructions_content="Use concise answers.",
      resume_session_id="session-123",
  )
  backend._prepare_transport(tmp_path)

  cmd = backend._build_command(FLAG_LIKE_PROMPT)

  resume_idx = cmd.index("--resume")
  assert cmd[resume_idx:resume_idx + 2] == ["--resume", "session-123"]
  assert resume_idx < cmd.index("--task-file")
  assert (tmp_path / "task.md").read_text(encoding="utf-8") == FLAG_LIKE_PROMPT


def test_build_command_emits_context_window_only_when_configured(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, context_window=262144)
  backend._prepare_transport(tmp_path)

  cmd = backend._build_command(FLAG_LIKE_PROMPT)

  assert cmd.count("262144") == 1
  idx = cmd.index("262144")
  assert cmd[idx - 1] == "--context-window"

  default_backend = _build_backend(monkeypatch)
  default_backend._prepare_transport(tmp_path)
  assert "--context-window" not in default_backend._build_command(FLAG_LIKE_PROMPT)


def test_build_command_before_prepare_transport_raises(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  with pytest.raises(RuntimeError, match="_prepare_transport must run before _build_command"):
    backend._build_command(FLAG_LIKE_PROMPT)


def test_build_command_overwrites_task_file_on_retry(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)
  backend._prepare_transport(tmp_path)

  backend._build_command("first task")
  backend._build_command("second task")

  assert (tmp_path / "task.md").read_text(encoding="utf-8") == "second task"


# ---------------------------------------------------------------------------
# base.run() template-method ordering: _prepare_transport runs after the
# transport dir is resolved/created/rotated and before _build_command.
# ---------------------------------------------------------------------------


class _HaltAtSpawn(Exception):
  """Control-flow sentinel: on_spawn raises it so the run halts at spawn time."""


class _OrderRecordingBackend(AgentBackend):
  """Minimal backend recording the hook order base.run() drives."""

  def __init__(self, records: list, **kwargs):
    super().__init__(**kwargs)
    self._records = records

  def _prepare_transport(self, log_dir: Path) -> None:
    self._records.append(("transport", log_dir, log_dir.is_dir()))

  def _build_command(self, prompt: str) -> list[str]:
    self._records.append(("command",))
    return ["/bin/sh", "-c", "exit 0"]


async def _drive_run_halted_at_spawn(backend: AgentBackend, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """Drive backend.run() with a mocked subprocess; on_spawn raises the sentinel.

  Mirrors tests/test_backend_pid_start_contract.py's base-path harness: patched
  spawn returning a MagicMock process and a sentinel read_pid_stat.
  """
  monkeypatch.setattr("src.core.runs.read_pid_stat", lambda pid: ("ordering-test-start", "R"))
  process = MagicMock()
  process.pid = 4242
  process.stdin = MagicMock()
  process.stdin.drain = AsyncMock()
  process.stdin.wait_closed = AsyncMock()
  monkeypatch.setattr("src.agents.backends.base.asyncio.create_subprocess_exec", AsyncMock(return_value=process))

  async def on_spawn(pid: int) -> None:
    raise _HaltAtSpawn

  backend._on_spawn = on_spawn

  with pytest.raises(_HaltAtSpawn):
    async for _event in backend.run("ordering prompt", str(tmp_path), {"PATH": "/usr/bin:/bin"}):
      pass


@pytest.mark.asyncio
async def test_run_calls_prepare_transport_before_build_command_with_log_dir(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  records: list = []
  backend = _OrderRecordingBackend(records, log_dir=tmp_path / "logs")

  await _drive_run_halted_at_spawn(backend, monkeypatch, tmp_path)

  assert records == [("transport", tmp_path / "logs", True), ("command",)]
  assert (tmp_path / "logs").is_dir()


@pytest.mark.asyncio
async def test_run_temp_transport_dir_exists_at_hook_and_removed_after(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  records: list = []
  backend = _OrderRecordingBackend(records)

  await _drive_run_halted_at_spawn(backend, monkeypatch, tmp_path)

  (kind, log_dir, existed_at_hook) = records[0]
  assert kind == "transport"
  assert log_dir.name.startswith("charliebot-run-")
  assert existed_at_hook
  assert records[1] == ("command",)
  # The sentinel raise runs run()'s finally, which removes the throwaway dir.
  assert not log_dir.exists()


# ---------------------------------------------------------------------------
# BackendOption.context_window
# ---------------------------------------------------------------------------


def test_backend_option_defaults_context_window_to_none() -> None:
  option = BackendOption(id="cc-k3-test", label="t", type="charlie-code", model="test-model")
  assert option.context_window is None


def test_backend_option_accepts_positive_context_window() -> None:
  option = BackendOption(id="cc-k3-test", label="t", type="charlie-code", model="test-model", context_window=262144)
  assert option.context_window == 262144


@pytest.mark.parametrize("bad", [0, -1])
def test_backend_option_rejects_nonpositive_context_window(bad: int) -> None:
  with pytest.raises(ValidationError):
    BackendOption(id="cc-k3-test", label="t", type="charlie-code", model="test-model", context_window=bad)


# ---------------------------------------------------------------------------
# BackendOption.api_key -> CHARLIE_CODE_API_KEY injection
# ---------------------------------------------------------------------------


def test_backend_option_defaults_api_key_to_none() -> None:
  option = BackendOption(id="cc-test", label="t", type="charlie-code", model="test-model")
  assert option.api_key is None


def test_prepare_env_injects_api_key_when_configured(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, api_key="test-api-key-placeholder")

  prepared = backend._prepare_env({"PATH": "/usr/bin"})

  assert prepared["CHARLIE_CODE_API_KEY"] == "test-api-key-placeholder"


def test_prepare_env_without_api_key_leaves_env_untouched(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)
  input_env = {"PATH": f"{USER_LOCAL_BIN}:/usr/bin", "HOME": "/home/test"}

  prepared = backend._prepare_env(dict(input_env))

  assert "CHARLIE_CODE_API_KEY" not in prepared
  assert prepared == input_env


# ---------------------------------------------------------------------------
# registry wiring
# ---------------------------------------------------------------------------


def test_registry_propagates_context_window_into_charlie_code_backend(monkeypatch) -> None:
  monkeypatch.setattr(
      CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: "/usr/bin/charlie-code",
  )
  option = BackendOption(
      id="cc-k3-test",
      label="t",
      type="charlie-code",
      model="openai/test-model",
      api_base="http://test.invalid/v1",
      context_window=262144,
  )

  backend = build_backend(option, CharlieBotConfig())

  assert isinstance(backend, CharlieCodeBackend)
  assert backend._context_window == 262144


def test_registry_propagates_api_key_into_charlie_code_backend(monkeypatch) -> None:
  monkeypatch.setattr(
      CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: "/usr/bin/charlie-code",
  )
  option = BackendOption(
      id="cc-gemini-test",
      label="t",
      type="charlie-code",
      model="openai/test-model",
      api_base="http://test.invalid/v1",
      api_key="test-api-key-placeholder",
  )

  backend = build_backend(option, CharlieBotConfig())

  assert isinstance(backend, CharlieCodeBackend)
  assert backend._api_key == "test-api-key-placeholder"


def test_api_base_required(monkeypatch) -> None:
  monkeypatch.setattr(
      CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: "/usr/bin/charlie-code",
  )

  with pytest.raises(ValueError, match="api_base"):
    CharlieCodeBackend(model="charlie-code-test-model")

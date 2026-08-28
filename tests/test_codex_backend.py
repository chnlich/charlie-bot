import asyncio
import signal
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import FLAG_LIKE_PROMPT, FakeStdout
from pydantic import ValidationError

from src.agents.backends.codex import CodexBackend
from src.agents.backends.registry import build_backend
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption


def _fake_one_shot_proc(lines: list[bytes], *, stderr: bytes = b"", returncode: int = 0) -> MagicMock:
  proc = MagicMock()
  proc.stdout = FakeStdout(lines)
  proc.stderr = MagicMock()
  proc.stderr.read = AsyncMock(side_effect=[stderr, b""])
  proc.wait = AsyncMock(return_value=returncode)
  proc.returncode = returncode
  proc.pid = 9000
  return proc


def _build_backend(monkeypatch, **kwargs) -> CodexBackend:
  monkeypatch.setattr(
      "src.agents.backends.codex.resolve_binary",
      lambda name, fallback: "/usr/bin/codex",
  )
  kwargs.setdefault("model", "codex-test-model")
  return CodexBackend(**kwargs)


def test_prepare_cwd_writes_agents_md_when_instructions_provided(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Codex Instructions\nBuild stuff.")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Codex Instructions\nBuild stuff."


def test_prepare_cwd_skips_agents_md_when_no_instructions(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert not agents_md.exists()


def test_build_command_uses_double_dash_separator_for_prompt(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="codex-test-model")

  cmd = backend._build_command(FLAG_LIKE_PROMPT)

  assert cmd[-2:] == ["--", FLAG_LIKE_PROMPT]


def test_build_command_resume_uses_double_dash_separator_for_prompt(monkeypatch) -> None:
  backend = _build_backend(
      monkeypatch,
      model="codex-test-model",
      resume_session_id="sess-123",
  )

  cmd = backend._build_command(FLAG_LIKE_PROMPT)

  assert cmd[-2:] == ["--", FLAG_LIKE_PROMPT]
  assert "sess-123" in cmd


def test_build_command_defaults_to_xhigh_reasoning_effort(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="codex-test-model")

  cmd = backend._build_command("do the thing")

  assert "model_reasoning_effort=\"xhigh\"" in cmd
  idx = cmd.index("model_reasoning_effort=\"xhigh\"")
  assert cmd[idx - 1] == "--config"


def test_build_command_uses_custom_reasoning_effort(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="codex-test-model", model_reasoning_effort="ultra")

  cmd = backend._build_command("do the thing")

  assert "model_reasoning_effort=\"ultra\"" in cmd
  idx = cmd.index("model_reasoning_effort=\"ultra\"")
  assert cmd[idx - 1] == "--config"


def test_turn_completed_includes_codex_cost(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="gpt-5.5")

  translated = backend.translate_event({
      "type": "turn.completed",
      "usage": {
          "input_tokens": 1000,
          "cached_input_tokens": 400,
          "output_tokens": 20,
      },
  })

  assert translated == [
      {
          "type": ET.RESULT,
          "result": "",
          "usage": {
              "input_tokens": 1000,
              "output_tokens": 20,
              "cache_read_input_tokens": 400,
              "cache_creation_input_tokens": 0,
          },
          "total_cost_usd": 0.0038,
      }
  ]


def test_file_change_html_artifact_emits_file_write(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)
  artifact_dir = tmp_path / "artifacts"
  artifact_dir.mkdir()
  artifact_path = artifact_dir / "x.html"
  artifact_path.write_text("<!doctype html><p>artifact</p>", encoding="utf-8")

  translated = backend.translate_event({
      "type": "item.completed",
      "item": {
          "type": "file_change",
          "changes": [{
              "path": str(artifact_path),
              "kind": "update",
          }],
          "status": "completed",
      },
  })

  assert translated == [
      {
          "type": ET.FILE_WRITE,
          "path": str(artifact_path),
      },
  ]


def test_file_change_multi_file_emits_file_writes(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)
  artifact_dir = tmp_path / "artifacts"
  artifact_dir.mkdir()
  artifact_path = artifact_dir / "x.html"
  artifact_path.write_text("<main>inline</main>", encoding="utf-8")
  source_path = tmp_path / "kernel.cu"
  source_path.write_text("__global__ void k() {}\n", encoding="utf-8")

  translated = backend.translate_event({
      "type": "item.completed",
      "item": {
          "type": "file_change",
          "changes": [
              {
                  "path": str(artifact_path),
                  "kind": "update",
              },
              {
                  "path": str(source_path),
                  "kind": "update",
              },
          ],
          "status": "completed",
      },
  })

  assert translated == [
      {
          "type": ET.FILE_WRITE,
          "path": str(artifact_path),
      },
      {
          "type": ET.FILE_WRITE,
          "path": str(source_path),
      },
  ]


def test_file_change_missing_html_artifact_emits_file_write(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)
  artifact_path = tmp_path / "artifacts" / "x.html"

  translated = backend.translate_event({
      "type": "item.completed",
      "item": {
          "type": "file_change",
          "changes": [{
              "path": str(artifact_path),
              "kind": "update",
          }],
          "status": "completed",
      },
  })

  assert translated == [{
      "type": ET.FILE_WRITE,
      "path": str(artifact_path),
  }]


def test_file_change_started_html_artifact_emits_nothing(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)
  artifact_dir = tmp_path / "artifacts"
  artifact_dir.mkdir()
  artifact_path = artifact_dir / "x.html"
  artifact_path.write_text("<p>started</p>", encoding="utf-8")

  translated = backend.translate_event({
      "type": "item.started",
      "item": {
          "type": "file_change",
          "changes": [{
              "path": str(artifact_path),
              "kind": "update",
          }],
          "status": "in_progress",
      },
  })

  assert not translated


def test_file_change_regular_file_emits_file_write_without_filename_field(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)
  source_path = tmp_path / "kernel.cu"

  translated = backend.translate_event({
      "type": "item.completed",
      "item": {
          "type": "file_change",
          "changes": [{
              "path": str(source_path),
              "kind": "update",
          }],
          "status": "completed",
      },
  })

  assert translated == [{
      "type": ET.FILE_WRITE,
      "path": str(source_path),
  }]


def test_translate_todo_list_text_items_preserves_live_codex_plan_text(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "item.started",
      "item": {
          "type": "todo_list",
          "items": [
              {"text": "Inspect the code", "completed": False},
              {"text": "Patch the bug", "completed": False},
              {"text": "Run tests", "completed": True},
          ],
      },
  })

  assert translated == [{
      "type": "assistant",
      "message": {
          "content": [{
              "type": "text",
              "text": (
                  "- [ ] Inspect the code\n"
                  "- [ ] Patch the bug\n"
                  "- [x] Run tests"
              ),
          }],
      },
  }]


def test_translate_todo_list_step_items_preserves_step_text(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "item.updated",
      "item": {
          "type": "todo_list",
          "items": [
              {"step": "Write the failing test", "status": "pending"},
              {"step": "Implement the minimal fix", "status": "in_progress"},
              {"step": "Run the regression test", "status": "completed"},
          ],
      },
  })

  assert translated == [{
      "type": "assistant",
      "message": {
          "content": [{
              "type": "text",
              "text": (
                  "- [ ] Write the failing test\n"
                  "- [~] Implement the minimal fix\n"
                  "- [x] Run the regression test"
              ),
          }],
      },
  }]


def test_translate_todo_list_label_and_content_items_remain_compatible(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "item.completed",
      "item": {
          "type": "todo_list",
          "items": [
              {"label": "Keep label support", "status": "pending"},
              {"content": "Keep content support", "status": "completed"},
          ],
      },
  })

  assert translated == [{
      "type": "assistant",
      "message": {
          "content": [{
              "type": "text",
              "text": "- [ ] Keep label support\n- [x] Keep content support",
          }],
      },
  }]


def test_translate_todo_list_suppresses_blank_items(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "item.updated",
      "item": {
          "type": "todo_list",
          "items": [
              {},
              {"label": "   ", "status": "pending"},
              {"content": "", "status": "completed"},
              {"step": "\n", "status": "in_progress"},
              {"text": "Keep the real step", "completed": False},
          ],
      },
  })

  assert translated == [{
      "type": "assistant",
      "message": {
          "content": [{
              "type": "text",
              "text": "- [ ] Keep the real step",
          }],
      },
  }]


def test_translate_todo_list_suppresses_duplicate_snapshots(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  started = backend.translate_event({
      "type": "item.started",
      "item": {
          "id": "todo-1",
          "type": "todo_list",
          "items": [
              {"text": "Inspect the code", "completed": False},
              {"text": "Patch the bug", "completed": False},
          ],
      },
  })
  completed_without_changes = backend.translate_event({
      "type": "item.completed",
      "item": {
          "id": "todo-1",
          "type": "todo_list",
          "items": [
              {"text": "Inspect the code", "completed": False},
              {"text": "Patch the bug", "completed": False},
          ],
      },
  })
  updated = backend.translate_event({
      "type": "item.updated",
      "item": {
          "id": "todo-1",
          "type": "todo_list",
          "items": [
              {"text": "Inspect the code", "completed": True},
              {"text": "Patch the bug", "completed": False},
          ],
      },
  })

  assert started == [{
      "type": "assistant",
      "message": {
          "content": [{
              "type": "text",
              "text": "- [ ] Inspect the code\n- [ ] Patch the bug",
          }],
      },
  }]
  assert not completed_without_changes
  assert updated == [{
      "type": "assistant",
      "message": {
          "content": [{
              "type": "text",
              "text": "- [x] Inspect the code\n- [ ] Patch the bug",
          }],
      },
  }]


def test_reasoning_item_emits_thinking_deltas(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  first = backend.translate_event({
      "type": "item.updated",
      "item": {
          "id": "reason-1",
          "type": "reasoning",
          "text": "Plan",
      },
  })
  second = backend.translate_event({
      "type": "item.updated",
      "item": {
          "id": "reason-1",
          "type": "reasoning",
          "text": "Plan in action",
      },
  })
  duplicate = backend.translate_event({
      "type": "item.updated",
      "item": {
          "id": "reason-1",
          "type": "reasoning",
          "text": "Plan in action",
      },
  })

  assert first == [{"type": ET.THINKING, "content": "Plan"}]
  assert second == [{"type": ET.THINKING, "content": " in action"}]
  assert not duplicate


# ---------------------------------------------------------------------------
# model_auto_compact_token_limit
# ---------------------------------------------------------------------------


def test_build_command_omits_auto_compact_when_absent(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="codex-test-model")

  cmd = backend._build_command("do the thing")

  assert not any("model_auto_compact_token_limit" in arg for arg in cmd)


def test_build_command_resume_omits_auto_compact_when_absent(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="codex-test-model", resume_session_id="sess-1")

  cmd = backend._build_command("do the thing")

  assert not any("model_auto_compact_token_limit" in arg for arg in cmd)


def test_build_command_emits_auto_compact_once_when_configured(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="codex-test-model", model_auto_compact_token_limit=50000)

  cmd = backend._build_command("do the thing")

  assert cmd.count("model_auto_compact_token_limit=50000") == 1
  idx = cmd.index("model_auto_compact_token_limit=50000")
  assert cmd[idx - 1] == "--config"


def test_build_command_resume_emits_auto_compact_once_when_configured(monkeypatch) -> None:
  backend = _build_backend(
      monkeypatch,
      model="codex-test-model",
      model_auto_compact_token_limit=50000,
      resume_session_id="sess-1",
  )

  cmd = backend._build_command("do the thing")

  assert cmd.count("model_auto_compact_token_limit=50000") == 1
  idx = cmd.index("model_auto_compact_token_limit=50000")
  assert cmd[idx - 1] == "--config"
  assert cmd[-2:] == ["--", "do the thing"]
  assert cmd.index("sess-1") > idx


def test_backend_option_defaults_auto_compact_limit_to_none() -> None:
  option = BackendOption(id="codex-o3", label="Codex", type="codex", model="o3")
  assert option.model_auto_compact_token_limit is None


def test_backend_option_accepts_positive_auto_compact_limit() -> None:
  option = BackendOption(
      id="codex-o3",
      label="Codex",
      type="codex",
      model="o3",
      model_auto_compact_token_limit=50000,
  )
  assert option.model_auto_compact_token_limit == 50000


@pytest.mark.parametrize("bad", [0, -1, -1000])
def test_backend_option_rejects_nonpositive_auto_compact_limit(bad: int) -> None:
  with pytest.raises(ValidationError):
    BackendOption(
        id="codex-o3",
        label="Codex",
        type="codex",
        model="o3",
        model_auto_compact_token_limit=bad,
    )


def test_registry_propagates_auto_compact_limit_into_codex_backend(monkeypatch) -> None:
  monkeypatch.setattr(
      "src.agents.backends.codex.resolve_binary",
      lambda name, fallback: "/usr/bin/codex",
  )
  option = BackendOption(
      id="codex-o3",
      label="Codex",
      type="codex",
      model="codex-test-model",
      model_reasoning_effort="high",
      model_auto_compact_token_limit=50000,
  )

  backend = build_backend(option, CharlieBotConfig())

  assert isinstance(backend, CodexBackend)
  assert backend._model_reasoning_effort == "high"
  assert backend._model_auto_compact_token_limit == 50000
  cmd = backend._build_command("do the thing")
  assert 'model_reasoning_effort="high"' in cmd
  assert cmd.count("model_auto_compact_token_limit=50000") == 1


@pytest.mark.asyncio
async def test_one_shot_text_raises_structured_error(monkeypatch) -> None:
  proc = _fake_one_shot_proc([
      b'{"type":"error","error":{"message":"unsupported reasoning effort: ultra"}}\n',
  ], stderr=b"generic stderr")

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)):
    backend = _build_backend(monkeypatch, model_reasoning_effort="ultra")
    with pytest.raises(RuntimeError, match="unsupported reasoning effort: ultra") as exc_info:
      await backend.one_shot_text("prompt", "system", timeout=5.0)

  assert "generic stderr" not in str(exc_info.value)


@pytest.mark.asyncio
async def test_one_shot_text_raises_turn_failed_diagnostic(monkeypatch) -> None:
  proc = _fake_one_shot_proc([
      b'{"type":"turn.failed","error":{"message":"context window exceeded"}}\n',
  ], stderr=b"generic stderr")

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)):
    backend = _build_backend(monkeypatch)
    with pytest.raises(RuntimeError, match="context window exceeded") as exc_info:
      await backend.one_shot_text("prompt", "system", timeout=5.0)

  assert "generic stderr" not in str(exc_info.value)


@pytest.mark.asyncio
async def test_one_shot_text_raises_nonzero_exit_with_bounded_stderr(monkeypatch) -> None:
  stderr = b"codex process failed\n" + b"x" * 10000
  proc = _fake_one_shot_proc([], stderr=stderr, returncode=2)

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)):
    backend = _build_backend(monkeypatch)
    with pytest.raises(RuntimeError, match="codex process failed") as exc_info:
      await backend.one_shot_text("prompt", "system", timeout=5.0)

  message = str(exc_info.value)
  assert "exit 2" in message
  assert len(message) < 5000


@pytest.mark.asyncio
async def test_one_shot_text_ignores_non_agent_assistant_events(monkeypatch) -> None:
  proc = _fake_one_shot_proc([
      b'{"type":"item.completed","item":{"type":"todo_list","id":"todo-1",'
      b'"items":[{"text":"not an assistant response","status":"completed"}]}}\n',
  ])

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)):
    backend = _build_backend(monkeypatch)
    with pytest.raises(RuntimeError, match="no assistant text"):
      await backend.one_shot_text("prompt", "system", timeout=5.0)


@pytest.mark.asyncio
async def test_one_shot_text_kills_process_group_on_timeout(monkeypatch) -> None:
  class _BlockingStdout:

    def __aiter__(self) -> "_BlockingStdout":
      return self

    async def __anext__(self) -> bytes:
      await asyncio.Event().wait()
      raise AssertionError("unreachable")

  proc = _fake_one_shot_proc([])
  proc.stdout = _BlockingStdout()

  with (
      patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)),
      patch("src.core.process.kill_process_group") as mock_kill,
  ):
    backend = _build_backend(monkeypatch)
    with pytest.raises(TimeoutError):
      await backend.one_shot_text("prompt", "system", timeout=0.01)

  mock_kill.assert_called_once_with(9000, signal.SIGKILL)

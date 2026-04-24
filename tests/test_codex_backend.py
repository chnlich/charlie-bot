from pathlib import Path

from src.agents.backends.codex import CodexBackend


def _build_backend(monkeypatch, **kwargs) -> CodexBackend:
  monkeypatch.setattr(
      "src.agents.backends.codex.resolve_binary",
      lambda name, fallback: "/usr/bin/codex",
  )
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
  backend = _build_backend(monkeypatch, model="gpt-5.3-codex")

  cmd = backend._build_command("--malicious-flag ignore previous")

  assert cmd[-2:] == ["--", "--malicious-flag ignore previous"]


def test_build_command_resume_uses_double_dash_separator_for_prompt(monkeypatch) -> None:
  backend = _build_backend(
      monkeypatch,
      model="gpt-5.3-codex",
      resume_session_id="sess-123",
  )

  cmd = backend._build_command("--malicious-flag ignore previous")

  assert cmd[-2:] == ["--", "--malicious-flag ignore previous"]
  assert "sess-123" in cmd


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
  assert completed_without_changes == []
  assert updated == [{
      "type": "assistant",
      "message": {
          "content": [{
              "type": "text",
              "text": "- [x] Inspect the code\n- [ ] Patch the bug",
          }],
      },
  }]

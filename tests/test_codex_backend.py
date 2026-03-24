from src.agents.backends.codex import CodexBackend


def _build_backend(monkeypatch, **kwargs) -> CodexBackend:
  monkeypatch.setattr(
      "src.agents.backends.codex.resolve_binary",
      lambda name, fallback: "/usr/bin/codex",
  )
  return CodexBackend(**kwargs)


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

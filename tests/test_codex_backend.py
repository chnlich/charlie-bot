from src.agents.backends.codex import CodexBackend


def _build_backend(monkeypatch, **kwargs) -> CodexBackend:
  monkeypatch.setattr(
      "src.agents.backends.codex.resolve_binary",
      lambda name, fallback: "/usr/bin/codex",
  )
  return CodexBackend(**kwargs)


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

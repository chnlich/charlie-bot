import pathlib
from typing import Any

import conftest
import pytest

from src.backends.codex import codex
from src.infra import event_types as ET


def _build_backend(monkeypatch: pytest.MonkeyPatch, **kwargs: Any) -> codex.CodexBackend:
  return conftest.build_cli_backend_rig(monkeypatch, codex.CodexBackend, **kwargs)


@pytest.mark.parametrize("resume_session_id", [
    pytest.param(None, id="fresh"),
    pytest.param("sess-123", id="resume"),
])
def test_build_command_uses_double_dash_separator_for_prompt(
    monkeypatch: pytest.MonkeyPatch, resume_session_id: str | None) -> None:
  backend = _build_backend(monkeypatch, model="codex-test-model", resume_session_id=resume_session_id)

  cmd = backend._build_command(conftest.FLAG_LIKE_PROMPT)

  assert cmd[-2:] == ["--", conftest.FLAG_LIKE_PROMPT]
  if resume_session_id is not None:
    assert resume_session_id in cmd


@pytest.mark.parametrize(
    ("model_reasoning_effort", "expected"),
    [
        pytest.param(None, "xhigh", id="default-xhigh"),
        pytest.param("ultra", "ultra", id="custom-effort"),
    ],
)
def test_build_command_carries_reasoning_effort(
    monkeypatch: pytest.MonkeyPatch, model_reasoning_effort: str | None, expected: str) -> None:
  backend = _build_backend(monkeypatch, model="codex-test-model", model_reasoning_effort=model_reasoning_effort)

  cmd = backend._build_command("do the thing")

  assert f'model_reasoning_effort="{expected}"' in cmd
  idx = cmd.index(f'model_reasoning_effort="{expected}"')
  assert cmd[idx - 1] == "--config"


def test_thread_started_translates_to_the_typed_session_attach_signal(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch, model="gpt-5.5")

  translated = backend.translate_event({"type": "thread.started", "thread_id": "thread-9"})

  assert translated == [{"type": ET.SESSION_ATTACHED, "session_id": "thread-9"}]


def test_turn_completed_includes_codex_cost(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch, model="gpt-5.5")

  translated = backend.translate_event(
      {
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
          "usage":
              {
                  "input_tokens": 1000,
                  "output_tokens": 20,
                  "cache_read_input_tokens": 400,
                  "cache_creation_input_tokens": 0,
              },
          "total_cost_usd": 0.0038,
      }
  ]


# The translation maps a completed file_change item in memory; disk state never gates the
# emit, so the missing-artifact and regular-file rows create nothing while the artifact
# rows seed their files.
_FILE_CHANGE_ROWS = [
    pytest.param(
        {"artifacts/x.html": "<!doctype html><p>artifact</p>"},
        [("artifacts/x.html", "update")],
        "item.completed",
        "completed",
        ["artifacts/x.html"],
        id="html-artifact-completed-emits-file-write",
    ),
    pytest.param(
        {
            "artifacts/x.html": "<main>inline</main>",
            "kernel.cu": "__global__ void k() {}\n",
        },
        [("artifacts/x.html", "update"), ("kernel.cu", "update")],
        "item.completed",
        "completed",
        ["artifacts/x.html", "kernel.cu"],
        id="multi-file-completed-emits-file-writes",
    ),
    pytest.param(
        {},
        [("artifacts/x.html", "update")],
        "item.completed",
        "completed",
        ["artifacts/x.html"],
        id="missing-html-artifact-still-emits-file-write",
    ),
    pytest.param(
        {"artifacts/x.html": "<p>started</p>"},
        [("artifacts/x.html", "update")],
        "item.started",
        "in_progress",
        [],
        id="started-html-artifact-emits-nothing",
    ),
    pytest.param(
        {},
        [("kernel.cu", "update")],
        "item.completed",
        "completed",
        ["kernel.cu"],
        id="regular-file-emits-file-write-without-filename-field",
    ),
]


@pytest.mark.parametrize(("files", "changes", "event_type", "status", "expected_paths"), _FILE_CHANGE_ROWS)
def test_file_change_translation(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: pathlib.Path,
    files: dict[str, str],
    changes: list[tuple[str, str]],
    event_type: str,
    status: str,
    expected_paths: list[str],
) -> None:
  """One file_change scenario per row; the emit follows the event alone, never disk state."""
  backend = _build_backend(monkeypatch)
  for rel_path, content in files.items():
    seeded = tmp_path / rel_path
    seeded.parent.mkdir(parents=True, exist_ok=True)
    seeded.write_text(content, encoding="utf-8")

  translated = backend.translate_event(
      {
          "type": event_type,
          "item":
              {
                  "type": "file_change",
                  "changes": [{
                      "path": str(tmp_path / rel_path),
                      "kind": kind,
                  } for rel_path, kind in changes],
                  "status": status,
              },
      })

  assert translated == [{"type": codex.FILE_WRITE, "path": str(tmp_path / rel_path)} for rel_path in expected_paths]

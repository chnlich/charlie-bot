import json
from pathlib import Path

from src.agents.backends.opencode import OpenCodeBackend


def _build_backend(monkeypatch, **kwargs) -> OpenCodeBackend:
  monkeypatch.setattr(
      "src.agents.backends.opencode.resolve_binary",
      lambda name, fallback: "/usr/bin/opencode",
  )
  return OpenCodeBackend(**kwargs)


def test_prepare_cwd_writes_project_opencode_json(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  project_config = tmp_path / ".opencode" / "opencode.json"
  assert project_config.exists()

  data = json.loads(project_config.read_text(encoding="utf-8"))
  permission = data["agent"]["build"]["permission"]
  assert permission["glob"] == {"*": "allow"}
  assert permission["external_directory"] == {"*": "allow"}


def test_prepare_cwd_migrates_legacy_config_json(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)
  legacy_dir = tmp_path / ".opencode"
  legacy_dir.mkdir()
  legacy_config = legacy_dir / "config.json"
  legacy_payload = {"agent": {"build": {"permission": {"glob": {"*": "allow"}}}}}
  legacy_config.write_text(json.dumps(legacy_payload), encoding="utf-8")

  backend._prepare_cwd(str(tmp_path))

  project_config = legacy_dir / "opencode.json"
  assert project_config.exists()
  assert json.loads(project_config.read_text(encoding="utf-8")) == legacy_payload
  assert json.loads(legacy_config.read_text(encoding="utf-8")) == legacy_payload


def test_prepare_cwd_writes_agents_md_when_instructions_provided(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Test Instructions\nDo things.")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Test Instructions\nDo things."


def test_prepare_cwd_skips_agents_md_when_no_instructions(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert not agents_md.exists()


def test_translate_tool_error_emits_tool_result(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "tool",
      "part": {
          "callID": "call-1",
          "tool": "glob",
          "state": {
              "input": {"pattern": "AGENTS.md", "path": "/tmp"},
              "error": "The user rejected permission to use this specific tool call.",
          },
      },
  })

  assert translated == [
      {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "tool_use",
                  "name": "glob",
                  "id": "call-1",
                  "input": {"pattern": "AGENTS.md", "path": "/tmp"},
              }]
          },
      },
      {
          "type": "tool_result",
          "tool_use_id": "call-1",
          "content": "The user rejected permission to use this specific tool call.",
      },
  ]

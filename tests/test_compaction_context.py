"""Tests for the Claude SessionStart compaction-context hook command."""

import json
import pathlib
import sys
import types

import pytest

from src.infra import config
from src.runtime.cli import compaction_context


def _tool_event(timestamp: str, name: str, tool_input: dict) -> dict:
  return {
      "type": "assistant",
      "timestamp": timestamp,
      "message": {
          "content": [{
              "type": "tool_use",
              "name": name,
              "input": tool_input,
          }],
      },
  }


def _write_events(path: pathlib.Path, events: list[dict]) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text("".join(f"{json.dumps(event)}\n" for event in events), encoding="utf-8")


def _set_config(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> types.SimpleNamespace:
  home = tmp_path / "home"
  repo = tmp_path / "repo"
  cfg = types.SimpleNamespace(charliebot_home=home, charlie_bot_repo=repo, sessions_dir=home / "sessions")
  monkeypatch.setattr(config, "get_config", lambda: cfg)
  return cfg


def _run_cli(monkeypatch: pytest.MonkeyPatch, session_id: str, slot: int) -> None:
  monkeypatch.setattr(sys, "argv", ["charliebot compaction-context", "--session", session_id, "--slot", str(slot)])
  compaction_context.main()


def test_reads_bash_read_skill_and_split_path_inputs_in_latest_read_order(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _set_config(monkeypatch, tmp_path)
  bash_old = tmp_path / "first" / "skills" / "bash-read" / "SKILL.md"
  bash_current = tmp_path / "current" / "skills" / "bash-read" / "SKILL.md"
  read_path = tmp_path / "reader" / "skills" / "read-tool" / "SKILL.md"
  bash_old.parent.mkdir(parents=True)
  bash_current.parent.mkdir(parents=True)
  read_path.parent.mkdir(parents=True)
  bash_old.write_text("old Bash version\n", encoding="utf-8")
  bash_current.write_text("current Bash text\n", encoding="utf-8")
  read_path.write_text("Read tool text\n", encoding="utf-8")

  skill_tool = cfg.charlie_bot_repo / "skills" / "skill-tool" / "SKILL.md"
  split_skill = cfg.charlie_bot_repo / "skills" / "split-path" / "SKILL.md"
  skill_tool.parent.mkdir(parents=True)
  split_skill.parent.mkdir(parents=True)
  skill_tool.write_text("Skill tool text\n", encoding="utf-8")
  split_skill.write_text("Split path text\n", encoding="utf-8")

  events_path = cfg.sessions_dir / "session-a" / "data" / "chat_events.jsonl"
  _write_events(
      events_path,
      [
          _tool_event("2026-01-01T00:01:00Z", "Bash", {
              "command": f"cat {bash_old}",
          }),
          _tool_event(
              "2026-01-01T00:02:00Z", "Bash", {
                  "command": f"cd {tmp_path} && cat skills/split-path/notes.md && echo SKILL.md",
              }),
          _tool_event("2026-01-01T00:03:00Z", "Read", {
              "file_path": str(read_path),
              "limit": 1,
          }),
          _tool_event("2026-01-01T00:04:00Z", "Bash", {
              "command": f"sed -n 2,4p {bash_current}",
          }),
          _tool_event("2026-01-01T00:05:00Z", "Skill", {
              "skill": "skill-tool",
          }),
          _tool_event(
              "2026-01-01T00:06:00Z", "Read", {
                  "file_path": str(tmp_path / "skills" / "not-a-skill" / "README.md"),
              }),
          _tool_event("2026-01-01T00:07:00Z", "Bash", {
              "command": f"cat {tmp_path}/skills/another/README.md",
          }),
      ],
  )

  skills, unresolved = compaction_context._load_skill_texts(events_path, cfg)

  assert unresolved == []
  assert [skill.name for skill in skills] == ["skill-tool", "bash-read", "read-tool", "split-path"]
  assert skills[0].text == "Skill tool text\n"
  assert skills[1].path == bash_current
  assert skills[1].text == "current Bash text\n"
  assert skills[2].text == "Read tool text\n"
  assert skills[3].path == split_skill
  assert skills[3].text == "Split path text\n"


def test_parts_obey_character_limit_and_split_only_at_line_ends(tmp_path: pathlib.Path) -> None:
  skill = compaction_context._SkillText(
      name="long-lines",
      path=tmp_path / "skills" / "long-lines" / "SKILL.md",
      text=f"{'A' * 4499}\n{'B' * 4499}\n",
  )

  parts, included, omitted = compaction_context._context_parts([skill])

  assert len(parts) == 2
  assert all(len(part) <= 9000 for part in parts)
  assert all(part.endswith("\n") for part in parts)
  assert f"{'A' * 4499}\n" in parts[0]
  assert f"{'B' * 4499}\n" in parts[1]
  assert [item.name for item in included] == ["long-lines"]
  assert omitted == []


def test_overflow_skills_are_listed_and_not_partially_attached(tmp_path: pathlib.Path) -> None:
  too_large = compaction_context._SkillText(
      name="too-large",
      path=tmp_path / "skills" / "too-large" / "SKILL.md",
      text="z" * 9001,
  )
  skills = [too_large] + [
      compaction_context._SkillText(
          name=f"skill-{index}",
          path=tmp_path / "skills" / f"skill-{index}" / "SKILL.md",
          text=f"{index}" * 8900 + "\n",
      ) for index in range(8)
  ]

  parts, included, omitted = compaction_context._context_parts(skills)
  rendered = "".join(parts)

  assert len(parts) <= 6
  assert all(len(part) <= 9000 for part in parts)
  assert omitted
  assert "Skills left out because they do not fit:" in parts[0]
  for skill in omitted:
    assert f"- {skill.name} ({skill.path})\n" in parts[0]
    assert f"Skill {skill.name} (" not in rendered
  for skill in included:
    assert f"Skill {skill.name} ({skill.path}), re-attached after compaction:" in rendered


def test_empty_slot_prints_nothing(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _set_config(monkeypatch, tmp_path)
  skill = cfg.charlie_bot_repo / "skills" / "small" / "SKILL.md"
  skill.parent.mkdir(parents=True)
  skill.write_text("A small skill.\n", encoding="utf-8")
  _write_events(
      cfg.sessions_dir / "session-b" / "data" / "chat_events.jsonl",
      [_tool_event("2026-01-01T00:00:00Z", "Skill", {"skill": "small"})],
  )

  _run_cli(monkeypatch, "session-b", 6)

  captured = capsys.readouterr()
  assert captured.out == ""
  assert "session=session-b slot=6 characters=0" in captured.err
  assert "skills=small" in captured.err


def test_missing_session_log_exits_nonzero_with_stderr_reason(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _set_config(monkeypatch, tmp_path)
  cfg.sessions_dir.mkdir(parents=True)

  with pytest.raises(SystemExit) as exc_info:
    _run_cli(monkeypatch, "missing-session", 1)

  captured = capsys.readouterr()
  assert exc_info.value.code == 1
  assert captured.out == ""
  assert captured.err.count("\n") == 1
  assert "session=missing-session slot=1 characters=0" in captured.err
  assert "chat_events.jsonl" in captured.err


_PATTERN_LOCATIONS = [
    "skills/*/SKILL.md",
    "skills/{a,b}/SKILL.md",
    "skills/$s/SKILL.md",
    "skills/google-*/SKILL.md",
    "skills/plan-approval, SKILL.md",
]


@pytest.mark.parametrize("location", _PATTERN_LOCATIONS)
def test_pattern_location_is_no_skill_read(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, location: str) -> None:
  cfg = _set_config(monkeypatch, tmp_path)
  events_path = cfg.sessions_dir / "session-pattern" / "data" / "chat_events.jsonl"
  _write_events(
      events_path,
      [
          _tool_event("2026-01-01T00:00:00Z", "Bash", {
              "command": f"cat {location}",
          }),
          _tool_event("2026-01-01T00:01:00Z", "Read", {
              "file_path": f"/repo/{location}",
          }),
      ],
  )

  skills, unresolved = compaction_context._load_skill_texts(events_path, cfg)

  assert skills == []
  assert unresolved == []


@pytest.mark.parametrize("name", ["skill.v2", "skill_v2", "skill2"])
def test_plain_skill_name_with_dot_underscore_or_digit_is_read(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, name: str) -> None:
  cfg = _set_config(monkeypatch, tmp_path)
  skill = cfg.charlie_bot_repo / "skills" / name / "SKILL.md"
  skill.parent.mkdir(parents=True)
  skill.write_text("Plain name text\n", encoding="utf-8")
  events_path = cfg.sessions_dir / "session-plain" / "data" / "chat_events.jsonl"
  _write_events(
      events_path,
      [_tool_event("2026-01-01T00:00:00Z", "Read", {
          "file_path": str(skill),
      })],
  )

  skills, unresolved = compaction_context._load_skill_texts(events_path, cfg)

  assert unresolved == []
  assert [(item.name, item.text) for item in skills] == [(name, "Plain name text\n")]


def test_deleted_skill_read_is_unresolved_while_other_skills_print(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _set_config(monkeypatch, tmp_path)
  present = cfg.charlie_bot_repo / "skills" / "present" / "SKILL.md"
  present.parent.mkdir(parents=True)
  present.write_text("Present skill text\n", encoding="utf-8")
  deleted = tmp_path / "gone" / "skills" / "deleted" / "SKILL.md"
  _write_events(
      cfg.sessions_dir / "session-deleted" / "data" / "chat_events.jsonl",
      [
          _tool_event("2026-01-01T00:00:00Z", "Read", {
              "file_path": str(deleted),
          }),
          _tool_event("2026-01-01T00:01:00Z", "Skill", {
              "skill": "present",
          }),
      ],
  )

  _run_cli(monkeypatch, "session-deleted", 1)

  captured = capsys.readouterr()
  assert "skills=present" in captured.err
  assert "unresolved=deleted" in captured.err
  assert "Present skill text" in json.loads(captured.out)["hookSpecificOutput"]["additionalContext"]

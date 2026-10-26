"""CLI hook that restores a manager session's read skills after Claude compacts."""

from __future__ import annotations

import datetime
import json
import os
import pathlib
import re
import sys
from typing import TYPE_CHECKING, NamedTuple

from src.infra import help_formatter

if TYPE_CHECKING:
  from src.infra import config

_MAX_PART_CHARS = 9000
_MAX_PARTS = 6
_SKILL_LOCATION = re.compile(
    r"(?<![A-Za-z0-9_-])(?P<directory>[^ \t\n'\"`;&|()]*?skills[/\\]"
    r"(?P<name>[^/\\ \t\n'\"`;&|()]+))(?=$|[/\\ \t\n'\"`;&|(),])")
_SKILL_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")


class _SkillRead(NamedTuple):
  name: str
  named_path: str | None
  command_cwd: pathlib.Path | None
  read_at: datetime.datetime
  order: int


class _SkillText(NamedTuple):
  name: str
  path: pathlib.Path
  text: str


def _timestamp(event: dict) -> datetime.datetime:
  raw = event.get("timestamp")
  if not isinstance(raw, str):
    raise ValueError("skill tool call has no timestamp")
  value = datetime.datetime.fromisoformat(raw.replace("Z", "+00:00"))
  if value.tzinfo is None:
    raise ValueError(f"skill tool call timestamp is not timezone-aware: {raw}")
  return value


def _skill_locations(value: str) -> list[tuple[str, str]]:
  locations = []
  for match in _SKILL_LOCATION.finditer(value):
    name = match.group("name")
    if _SKILL_NAME.fullmatch(name) is None:
      continue
    locations.append((name, match.group("directory").replace("\\", "/")))
  return locations


def _command_cwd(command: str) -> pathlib.Path | None:
  match = re.match(r"^\s*cd\s+(?P<path>'[^']*'|\"[^\"]*\"|[^;&|\s]+)(?:\s*(?:&&|;|$))", command)
  if match is None:
    return None
  raw_path = match.group("path").strip("'\"")
  path = pathlib.Path(os.path.expandvars(raw_path)).expanduser()
  if path.is_absolute():
    return path
  return None


def _reads_from_tool(block: dict) -> list[tuple[str, str | None, pathlib.Path | None]]:
  name = block.get("name")
  tool_input = block.get("input")
  if not isinstance(tool_input, dict):
    return []
  if name == "Skill":
    skill_name = tool_input.get("skill") or tool_input.get("skill_name")
    return [(skill_name, None, None)] if isinstance(skill_name, str) and skill_name else []
  if name == "Read":
    file_path = tool_input.get("file_path")
    if not isinstance(file_path, str) or pathlib.PurePosixPath(file_path.replace("\\", "/")).name != "SKILL.md":
      return []
    return [(skill_name, f"{directory}/SKILL.md", None) for skill_name, directory in _skill_locations(file_path)]
  if name == "Bash":
    command = tool_input.get("command")
    if not isinstance(command, str) or "SKILL.md" not in command:
      return []
    cwd = _command_cwd(command)
    return [(skill_name, f"{directory}/SKILL.md", cwd) for skill_name, directory in _skill_locations(command)]
  return []


def _latest_skill_reads(chat_events_path: pathlib.Path) -> list[_SkillRead]:
  latest: dict[str, _SkillRead] = {}
  with chat_events_path.open(encoding="utf-8") as handle:
    for order, line in enumerate(handle):
      event = json.loads(line)
      if not isinstance(event, dict) or event.get("type") != "assistant":
        continue
      message = event.get("message")
      if not isinstance(message, dict):
        continue
      for block in message.get("content", []):
        if not isinstance(block, dict) or block.get("type") != "tool_use":
          continue
        for name, named_path, command_cwd in _reads_from_tool(block):
          if not name:
            continue
          read = _SkillRead(
              name=name,
              named_path=named_path,
              command_cwd=command_cwd,
              read_at=_timestamp(event),
              order=order,
          )
          previous = latest.get(name)
          if previous is None or (read.read_at, read.order) > (previous.read_at, previous.order):
            latest[name] = read
  return sorted(latest.values(), key=lambda read: (read.read_at, read.order), reverse=True)


def _resolved_skill_path(read: _SkillRead, cfg: config.CharlieBotConfig) -> pathlib.Path | None:
  if read.named_path is not None:
    named = pathlib.Path(os.path.expandvars(read.named_path)).expanduser()
    if not named.is_absolute():
      named = (read.command_cwd or cfg.charlie_bot_repo) / named
    if named.is_file():
      return named
  for root in (cfg.charliebot_home / "skills", cfg.charlie_bot_repo / "skills"):
    candidate = root / read.name / "SKILL.md"
    if candidate.is_file():
      return candidate
  return None


def _load_skill_texts(
    chat_events_path: pathlib.Path,
    cfg: config.CharlieBotConfig,
) -> tuple[list[_SkillText], list[str]]:
  skills = []
  unresolved = []
  for read in _latest_skill_reads(chat_events_path):
    path = _resolved_skill_path(read, cfg)
    if path is None:
      unresolved.append(read.name)
      continue
    skills.append(_SkillText(name=read.name, path=path, text=path.read_text(encoding="utf-8")))
  return skills, unresolved


def _skill_block(skill: _SkillText) -> str:
  body = f"Skill {skill.name} ({skill.path}), re-attached after compaction:\n{skill.text}"
  return body if body.endswith("\n") else f"{body}\n"


def _partition(skills: list[_SkillText], omitted: list[_SkillText]) -> list[str] | None:
  if not skills and not omitted:
    return []
  names = ", ".join(f"{skill.name} ({skill.path})" for skill in skills) or "none"
  lines = [f"Re-attached skills: {names}\n"]
  if omitted:
    lines.append("Skills left out because they do not fit:\n")
    lines.extend(f"- {skill.name} ({skill.path})\n" for skill in omitted)
  for skill in skills:
    lines.extend(_skill_block(skill).splitlines(keepends=True))

  parts = [""]
  for line in lines:
    if len(line) > _MAX_PART_CHARS:
      return None
    if len(parts[-1]) + len(line) > _MAX_PART_CHARS:
      if len(parts) == _MAX_PARTS:
        return None
      parts.append("")
    parts[-1] += line
  return parts


def _context_parts(skills: list[_SkillText]) -> tuple[list[str], list[_SkillText], list[_SkillText]]:
  included: list[_SkillText] = []
  omitted: list[_SkillText] = []
  for skill in skills:
    if _partition([*included, skill], omitted) is None:
      omitted.append(skill)
    else:
      included.append(skill)
  parts = _partition(included, omitted)
  while parts is None and included:
    omitted.append(included.pop())
    parts = _partition(included, omitted)
  if parts is None:
    raise ValueError("the skill name and path list exceeds the 9,000-character first part")
  return parts, included, omitted


def _emit_line(
    session_id: str,
    slot: int,
    characters: int,
    skills: list[_SkillText],
    omitted: list[_SkillText],
    unresolved: list[str],
) -> None:
  attached = ",".join(skill.name for skill in skills) or "-"
  left_out = ",".join(skill.name for skill in omitted) or "-"
  unknown = ",".join(unresolved) or "-"
  print(
      f"session={session_id} slot={slot} characters={characters} skills={attached} omitted={left_out} unresolved={unknown}",
      file=sys.stderr,
  )


def main() -> None:
  """Print one SessionStart hook output part for the requested CharlieBot session."""
  parser = help_formatter.CliArgumentParser(
      description="Restore skills read by a Claude manager session after compaction")
  parser.add_argument("--session", required=True, help="CharlieBot session id")
  parser.add_argument(
      "--slot", required=True, type=int, choices=range(1, _MAX_PARTS + 1), help="part number from 1 to 6")
  args = parser.parse_args()

  try:
    from src.infra import config  # deferred: charliebot compaction-context --help
    from src.runtime import chat_events  # deferred: charliebot compaction-context --help

    cfg = config.get_config()
    events_path = chat_events.chat_events_path(cfg.sessions_dir / args.session)
    skills, unresolved = _load_skill_texts(events_path, cfg)
    parts, included, omitted = _context_parts(skills)
    part = parts[args.slot - 1] if args.slot <= len(parts) else ""
  except (OSError, UnicodeError, ValueError) as exc:
    print(f"session={args.session} slot={args.slot} characters=0 skills=- error={exc}", file=sys.stderr)
    sys.exit(1)

  if part:
    print(
        json.dumps(
            {"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": part,
            }}, ensure_ascii=False))
  _emit_line(args.session, args.slot, len(part), included, omitted, unresolved)


if __name__ == "__main__":
  main()

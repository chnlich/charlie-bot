"""Master prompt assembly uses the labeled-entry memory store.

The master instructions now assemble from the memory store at cfg.memory_dir
via src.features.memory.memory.assemble_master: resident-topic entries inject in full,
non-resident master-audience entries appear as index lines only, and staging
candidates are never injected. Fixtures are entry format v2 (title in
frontmatter, comma-list audience, heading-free body).
"""

import pathlib
import types

import conftest

from src.features.memory import memory
from src.infra.models import SlackOrigin
from src.runtime import master_cc


def _make_store(memory_dir: pathlib.Path) -> None:
  conftest.write_memory_topics(memory_dir, ["profile resident", "communication resident", "charliebot"])
  # Resident entry (full body injected, '# {title}' heading synthesized).
  conftest.write_memory_entry(memory_dir, "profile", "dark-mode", title="Dark Mode", body="User prefers dark UI.\n")
  # Non-resident entry (index line only).
  conftest.write_memory_entry(
      memory_dir, "charliebot", "cli-flags", title="CLI Flags", body="Full body that must NOT be injected.\n")
  # Staging candidate (never injected). Legacy body shape on purpose: the
  # relaxed staging rules keep such candidates parseable.
  (memory_dir / "staging").mkdir()
  (memory_dir / "staging" / "20260728T120000Z-abcd1234-pending.md").write_text(
      "---\ntopic: profile\nscope: user\naudience: both\n---\n# Pending\n\nSTAGED BODY\n", encoding="utf-8")


def _cfg(tmp_path: pathlib.Path) -> types.SimpleNamespace:
  cfg = conftest.make_instruction_cfg(tmp_path)
  _make_store(cfg.memory_dir)
  return cfg


def _main_session(session_id: str = "session-1") -> types.SimpleNamespace:
  """A session meta with no platform origin — the main-session instruction build."""
  return types.SimpleNamespace(id=session_id, group=None, slack_origin=None, discord_origin=None)


def _thread_session(session_id: str = "session-1") -> types.SimpleNamespace:
  """A session meta summoned from a Slack thread — the thread-session instruction build."""
  return types.SimpleNamespace(
      id=session_id,
      group=None,
      slack_origin=SlackOrigin(team_id="T1", channel_id="C1", thread_ts="1700000000.000100"),
      discord_origin=None)


def test_resident_body_present_non_resident_index_only(tmp_path: pathlib.Path) -> None:
  cfg = _cfg(tmp_path)
  out = master_cc.master_cc_run._build_instructions_content(_main_session(), cfg, None)
  assert out is not None
  assert "BASE PROMPT" in out
  # Resident entry: full body injected, heading synthesized from the frontmatter title.
  assert "# Dark Mode\n\nUser prefers dark UI." in out
  # Non-resident entry: index line present, full body absent.
  assert "charliebot/cli-flags · CLI Flags" in out
  assert "Full body that must NOT be injected." not in out
  # The index header line appears exactly once, immediately before the index lines.
  assert out.count(memory.INDEX_HEADER) == 1
  assert f"{memory.INDEX_HEADER}\ncharliebot/cli-flags · CLI Flags" in out


def test_staging_content_absent(tmp_path: pathlib.Path) -> None:
  cfg = _cfg(tmp_path)
  out = master_cc.master_cc_run._build_instructions_content(_main_session(), cfg, None)
  assert out is not None
  # Staging candidates are never injected.
  assert "STAGED BODY" not in out
  assert "Pending" not in out


def test_missing_memory_dir_still_builds(tmp_path: pathlib.Path) -> None:
  """A missing memory_dir gets the store scaffold on first read: the prompt builds with no memory block."""
  cfg = conftest.make_instruction_cfg(tmp_path)  # memory_dir not created yet
  assert not cfg.memory_dir.exists()
  out = master_cc.master_cc_run._build_instructions_content(_main_session(), cfg, None)
  assert out is not None
  assert "BASE PROMPT" in out
  assert memory.INDEX_HEADER not in out
  for name in (".git", "topics", ".gitignore", "entries", "staging"):
    assert (cfg.memory_dir / name).exists()


# ---------------------------------------------------------------------------
# Session-kind rule files (prompts/thread_session.md vs prompts/manager_workflows.md)
# ---------------------------------------------------------------------------


def _real_repo_cfg(tmp_path: pathlib.Path) -> types.SimpleNamespace:
  """Instruction inputs whose charlie_bot_repo is this checkout's real prompts tree; the host
  override path does not exist and memory_dir is an empty temporary store, so the built
  instructions carry exactly the rule files."""
  home = tmp_path / "home"
  return types.SimpleNamespace(
      charlie_bot_repo=conftest.ROOT,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      memory_dir=home / "memory",
      charliebot_home=home,
  )


def _repo_section_headings(filename: str) -> list[str]:
  """The second-level section headings of one repo prompts file, in order."""
  return [
      line[3:]
      for line in (conftest.ROOT / "prompts" / filename).read_text(encoding="utf-8").splitlines()
      if line.startswith("## ")
  ]


def test_main_session_instructions_carry_every_manager_section(tmp_path: pathlib.Path) -> None:
  """A main session (no platform origin) gets master.md plus manager_workflows.md: every
  second-level heading the old single-file master.md carried is present exactly once across
  the two files, and the manager workflows file's full text rides verbatim."""
  cfg = _real_repo_cfg(tmp_path)
  out = master_cc.master_cc_run._build_instructions_content(_main_session(), cfg, None)
  assert out is not None
  master_headings = _repo_section_headings("master.md")
  workflow_headings = _repo_section_headings("manager_workflows.md")
  # 16 + 6: the split neither lost nor duplicated a section.
  assert len(master_headings) + len(workflow_headings) == 22
  assert not set(master_headings) & set(workflow_headings)
  for heading in master_headings + workflow_headings:
    assert f"## {heading}" in out
  workflows = (conftest.ROOT / "prompts" / "manager_workflows.md").read_text(encoding="utf-8")
  assert workflows in out


def test_thread_session_instructions_carry_the_thread_brief_and_no_moved_sections(tmp_path: pathlib.Path) -> None:
  """A thread session gets master.md plus thread_session.md: the brief's full text rides
  verbatim and none of the six manager-workflow sections enters the instructions."""
  cfg = _real_repo_cfg(tmp_path)
  out = master_cc.master_cc_run._build_instructions_content(_thread_session(), cfg, None)
  assert out is not None
  brief = (conftest.ROOT / "prompts" / "thread_session.md").read_text(encoding="utf-8")
  assert brief in out
  for heading in _repo_section_headings("manager_workflows.md"):
    assert f"## {heading}" not in out


def test_session_id_substitution_reaches_both_second_rule_files(tmp_path: pathlib.Path) -> None:
  """The {{session_id}} replacement master.md gets applies to the second rule file too, on
  both session kinds."""
  cfg = conftest.make_instruction_cfg(tmp_path)
  (cfg.charlie_bot_repo / "prompts" / "manager_workflows.md").write_text(
      "manager rules for {{session_id}}", encoding="utf-8")
  (cfg.charlie_bot_repo / "prompts" / "thread_session.md").write_text(
      "thread rules for {{session_id}}", encoding="utf-8")
  main_out = master_cc.master_cc_run._build_instructions_content(_main_session("sess-main"), cfg, None)
  thread_out = master_cc.master_cc_run._build_instructions_content(_thread_session("sess-thread"), cfg, None)
  assert main_out is not None and thread_out is not None
  assert "manager rules for sess-main" in main_out
  assert "thread rules for sess-thread" in thread_out

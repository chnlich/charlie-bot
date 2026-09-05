"""Master prompt assembly uses the labeled-entry memory store.

The master instructions now assemble from the memory store at cfg.memory_dir
via src.core.memory.assemble_master: resident-topic entries inject in full,
non-resident master-audience entries appear as index lines only, and staging
candidates are never injected. Fixtures are entry format v2 (title in
frontmatter, comma-list audience, heading-free body).
"""

from pathlib import Path
from types import SimpleNamespace

from conftest import write_memory_entry, write_memory_topics

from src.agents import master_cc
from src.core import memory


def _make_store(memory_dir: Path) -> None:
  write_memory_topics(memory_dir, ["profile resident", "communication resident", "charliebot"])
  # Resident entry (full body injected, '# {title}' heading synthesized).
  write_memory_entry(memory_dir, "profile", "dark-mode", title="Dark Mode", body="User prefers dark UI.\n")
  # Non-resident entry (index line only).
  write_memory_entry(
      memory_dir, "charliebot", "cli-flags", title="CLI Flags", body="Full body that must NOT be injected.\n")
  # Staging candidate (never injected). Legacy body shape on purpose: the
  # relaxed staging rules keep such candidates parseable.
  (memory_dir / "staging").mkdir()
  (memory_dir / "staging" / "20260728T120000Z-abcd1234-pending.md").write_text(
      "---\ntopic: profile\nscope: user\naudience: both\n---\n# Pending\n\nSTAGED BODY\n", encoding="utf-8")


def _cfg(tmp_path: Path) -> SimpleNamespace:
  home = tmp_path / "home"
  home.mkdir()
  repo = tmp_path / "repo"
  (repo / "prompts").mkdir(parents=True)
  (repo / "prompts" / "master.md").write_text("BASE PROMPT", encoding="utf-8")
  memory_dir = home / "memory"
  _make_store(memory_dir)
  return SimpleNamespace(
      charlie_bot_repo=repo,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      memory_dir=memory_dir,
  )


def test_resident_body_present_non_resident_index_only(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  out = master_cc._build_instructions_content(SimpleNamespace(id="session-1", role=None, group=None), cfg, None)
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


def test_staging_content_absent(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  out = master_cc._build_instructions_content(SimpleNamespace(id="session-1", role=None, group=None), cfg, None)
  assert out is not None
  # Staging candidates are never injected.
  assert "STAGED BODY" not in out
  assert "Pending" not in out


def test_missing_memory_dir_still_builds(tmp_path: Path) -> None:
  """A missing memory_dir is the one tolerated degradation: prompt still builds."""
  home = tmp_path / "home"
  home.mkdir()
  repo = tmp_path / "repo"
  (repo / "prompts").mkdir(parents=True)
  (repo / "prompts" / "master.md").write_text("BASE PROMPT", encoding="utf-8")
  cfg = SimpleNamespace(
      charlie_bot_repo=repo,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      memory_dir=home / "memory",  # does not exist
  )
  out = master_cc._build_instructions_content(SimpleNamespace(id="session-1", role=None, group=None), cfg, None)
  assert out is not None
  assert "BASE PROMPT" in out
  assert "User prefers dark UI." not in out

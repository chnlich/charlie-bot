"""Master prompt assembly includes the MEMORY staging file."""
from types import SimpleNamespace

from src.agents import master_cc


def _cfg(tmp_path):
  home = tmp_path / "home"
  home.mkdir()
  repo = tmp_path / "repo"
  (repo / "prompts").mkdir(parents=True)
  (repo / "prompts" / "master.md").write_text("BASE PROMPT", encoding="utf-8")
  return SimpleNamespace(
      charlie_bot_repo=repo,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      memory_file=home / "MEMORY.md",
      memory_host_file=home / "MEMORY.host.md",
      memory_tmp_file=home / "MEMORY.tmp.md",
  )


def test_staging_file_is_appended_after_memory(tmp_path):
  cfg = _cfg(tmp_path)
  cfg.memory_file.write_text("MEMORY BODY", encoding="utf-8")
  cfg.memory_host_file.write_text("HOST BODY", encoding="utf-8")
  cfg.memory_tmp_file.write_text("STAGED BODY", encoding="utf-8")

  out = master_cc._build_instructions_content(SimpleNamespace(id="session-1"), cfg)

  assert out.index("MEMORY BODY") < out.index("HOST BODY") < out.index("STAGED BODY")


def test_missing_staging_file_is_skipped(tmp_path):
  cfg = _cfg(tmp_path)
  cfg.memory_file.write_text("MEMORY BODY", encoding="utf-8")

  out = master_cc._build_instructions_content(SimpleNamespace(id="session-1"), cfg)

  assert "MEMORY BODY" in out
  assert out.endswith("MEMORY BODY")

"""Per-model overlay segment in the master instruction assembly.

``_build_instructions_content`` appends ``prompts/model_overlays/<model with
each "/" replaced by "__">.md`` as its final part when the resolved ``model``
names a file that exists; a missing overlay appends nothing (byte-identical
to a no-model assembly), and a present-but-unreadable overlay raises rather
than silently dropping the segment. These tests assert the mechanism, not any
shipped overlay's literal content.
"""

from pathlib import Path
from types import SimpleNamespace

import pytest

from src.agents import master_cc


def _build_cfg(tmp_path: Path) -> SimpleNamespace:
  """A cfg stand-in whose ``charlie_bot_repo`` points at *tmp_path*.

  ``CharlieBotConfig.charlie_bot_repo`` is a derived property tied to the
  installed package location, so the tests redirect it through a namespace
  (same convention as test_worker_prompt_extraction / test_slack_listener) and
  never depend on the real repo contents.
  """
  repo = tmp_path / "repo"
  (repo / "prompts").mkdir(parents=True)
  (repo / "prompts" / "master.md").write_text("BASE PROMPT", encoding="utf-8")
  home = tmp_path / "home"
  home.mkdir()
  memory_dir = home / "memory"
  (memory_dir / "entries").mkdir(parents=True)
  (memory_dir / "topics").write_text("profile resident\n", encoding="utf-8")
  return SimpleNamespace(
      charlie_bot_repo=repo,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      memory_dir=memory_dir,
  )


def _session() -> SimpleNamespace:
  return SimpleNamespace(id="session-1", role=None, group=None)


def _build_base(cfg: SimpleNamespace) -> str:
  """The assembly with no model: the byte-for-byte baseline every case diff vs."""
  out = master_cc._build_instructions_content(_session(), cfg, model=None)
  assert out is not None
  return out


@pytest.mark.parametrize(
    "model",
    [
        "fpt-kimi-k3/moonshotai/Kimi-K3",
        "vendor__single",
        "a/b/c",
    ],
)
def test_overlay_present_iff_file_exists(tmp_path: Path, model: str) -> None:
  cfg = _build_cfg(tmp_path)
  base = _build_base(cfg)
  overlay_dir = cfg.charlie_bot_repo / "prompts" / "model_overlays"
  overlay_dir.mkdir(parents=True)
  sanitized = model.replace("/", "__")
  overlay_text = f"OVERLAY BODY for {model}\n"
  (overlay_dir / f"{sanitized}.md").write_text(overlay_text, encoding="utf-8")

  with_overlay = master_cc._build_instructions_content(_session(), cfg, model=model)
  assert with_overlay is not None
  assert with_overlay == f"{base}\n\n{overlay_text}"
  # Absent file -> byte-identical to the no-model product.
  without_overlay = master_cc._build_instructions_content(_session(), cfg, model="no-such-model")
  assert without_overlay == base


def test_model_with_slash_resolves_to_sanitized_filename_only(tmp_path: Path) -> None:
  """A model containing "/" picks up the file with "__" in place of "/", not the raw string."""
  cfg = _build_cfg(tmp_path)
  overlay_dir = cfg.charlie_bot_repo / "prompts" / "model_overlays"
  overlay_dir.mkdir(parents=True)
  (overlay_dir / "a__b.md").write_text("SANITIZED", encoding="utf-8")
  (overlay_dir / "a").mkdir()
  (overlay_dir / "a" / "b.md").write_text("RAW", encoding="utf-8")

  out = master_cc._build_instructions_content(_session(), cfg, model="a/b")
  assert out is not None
  assert "SANITIZED" in out
  assert "RAW" not in out


def test_only_model_differs_diff_is_exactly_the_two_overlays(tmp_path: Path) -> None:
  """Two assemblies differing only in model differ exactly by their overlays."""
  cfg = _build_cfg(tmp_path)
  overlay_dir = cfg.charlie_bot_repo / "prompts" / "model_overlays"
  overlay_dir.mkdir(parents=True)
  (overlay_dir / "model__one.md").write_text("OVERLAY ONE", encoding="utf-8")
  (overlay_dir / "model__two.md").write_text("OVERLAY TWO", encoding="utf-8")

  one = master_cc._build_instructions_content(_session(), cfg, model="model/one")
  two = master_cc._build_instructions_content(_session(), cfg, model="model/two")
  assert one is not None and two is not None
  base = _build_base(cfg)
  assert one == f"{base}\n\nOVERLAY ONE"
  assert two == f"{base}\n\nOVERLAY TWO"


def test_unreadable_overlay_raises(tmp_path: Path) -> None:
  """A present-but-unreadable overlay fails loud instead of returning a product
  without the overlay."""
  cfg = _build_cfg(tmp_path)
  overlay_dir = cfg.charlie_bot_repo / "prompts" / "model_overlays"
  overlay_dir.mkdir(parents=True)
  # A directory at the overlay path is "present" but not readable as a file.
  (overlay_dir / "model__x.md").mkdir()

  with pytest.raises(OSError):
    master_cc._build_instructions_content(_session(), cfg, model="model/x")

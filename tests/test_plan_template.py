"""Contract tests for prompts/plan_template.html: the version-note rule and kept mark tolerance."""

import re

from conftest import ROOT

from src.core import artifact_check

TEMPLATE = (ROOT / "prompts" / "plan_template.html").read_text(encoding="utf-8")
STYLE = re.search(r"<style>.*?</style>", TEMPLATE, re.DOTALL).group(0)


def test_template_states_no_revision_mark_rule() -> None:
  """The hand-written revision-mark rule is retired: a version's body carries no marks, the
  reason rides on the version record's note, and the reader gets the differences from the
  diff view. The page-budget sentence excusing badges and revnotes from the budget is gone."""
  assert "Revision marks:" not in TEMPLATE
  assert "span.revbadge" not in TEMPLATE
  assert "div.revnote" not in TEMPLATE
  assert "ride outside it" not in TEMPLATE
  assert "charliebot plan diff" in TEMPLATE


def test_template_keeps_revision_mark_css() -> None:
  """Pages already in flight still carry marks and get amended again: the CSS that renders
  them stays in all templates, only the rule prose is gone."""
  assert ".revbadge{" in STYLE
  assert ".revnote{" in STYLE


def test_artifact_check_keeps_mark_tolerance() -> None:
  """src/core/artifact_check.py keeps its mark-stripping machinery untouched: the regexes
  still recognize the mark markup the older plan pages carry."""
  assert artifact_check._REVNOTE_RE.search('<div class="revnote">r2 · trigger — what changed</div>')
  assert artifact_check._REVBADGE_RE.search('<span class="revbadge">changed · r3</span>')

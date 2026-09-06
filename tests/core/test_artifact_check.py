"""Tests for src/core/artifact_check.py and the ``charliebot artifact check`` CLI.

The goal-length and page-height measurements moved here from src/core/plans.py with their
budgets unchanged; the DOM assertions are the new mechanical half of each genre's GRAMMAR.
Renderer work and the probe's model call are doubled out — no headless chrome, no backend
subprocess, no HTTP.
"""

import re
import sys
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest
from conftest import (
    make_plan_setup,
    plan_page_html,
    write_plan_artifact,
    write_stub_chrome,
)

from src.cli.artifact import main as artifact_main
from src.core import artifact_check
from src.core.artifact_check import run_assertions, run_probe

_REPO_ROOT = Path(__file__).resolve().parents[2]

_CLI_ARTIFACT_GET_CONFIG_PATCH_TARGET = "src.cli.artifact.get_config"


def _genre_doc(genre: str, body: str) -> str:
  """Full HTML document for *genre*: its template's <style> block verbatim plus *body*."""
  template = (_REPO_ROOT / "prompts" / artifact_check._GENRE_TEMPLATES[genre]).read_text(encoding="utf-8")
  style = re.search(r"<style>.*?</style>", template, re.DOTALL).group(0)
  return f"<html><head>{style}</head><body>{body}</body></html>"


def _write(tmp_path: Path, content: str, name: str = "page.html") -> Path:
  artifact = tmp_path / name
  artifact.write_text(content, encoding="utf-8")
  return artifact


def _by_name(outcomes: list[artifact_check.AssertionOutcome]) -> dict[str, list[artifact_check.AssertionOutcome]]:
  by_name: dict[str, list[artifact_check.AssertionOutcome]] = {}
  for outcome in outcomes:
    by_name.setdefault(outcome.name, []).append(outcome)
  return by_name


def _run(genre: str, artifact: Path, cfg=None) -> dict[str, list[artifact_check.AssertionOutcome]]:
  return _by_name(run_assertions(genre, artifact, cfg))


def _chrome_cfg(tmp_path: Path, height: int = 800) -> SimpleNamespace:
  return SimpleNamespace(headless_chrome_bin=write_stub_chrome(tmp_path, height))


def _sections(genre_titles: list[str]) -> str:
  return "".join(
      f'<section><h2><span class="n">{i}</span> {title}</h2></section>' for i, title in enumerate(genre_titles, 1))


_SITREP_TITLES = ["What waits on you?", "What is this and why?", "What was verified?", "Risks", "What happens next?"]


def _sitrep_ok_doc() -> str:
  return _genre_doc(
      "sitrep", '<section><h2><span class="n">1</span> What waits on you?</h2><p>Nothing.</p></section>'
      '<section><h2><span class="n">2</span> What is this and why?</h2><p>Why. <span class="req">r1</span></p></section>'
      '<section><h2><span class="n">3</span> What was verified?</h2><p>Done. <span class="src">s</span></p></section>'
      '<section><h2><span class="n">4</span> Risks</h2>'
      '<p><span class="tag fact">Fact</span> The reading holds. <span class="src">s</span></p></section>'
      '<section><h2><span class="n">5</span> What happens next?</h2><p>Next.</p></section>')


def _debug_ok_doc() -> str:
  return _genre_doc("debug", _sections([f"S{i}" for i in range(1, 6)]))


# ---------------------------------------------------------------------------
# Genre table ownership
# ---------------------------------------------------------------------------


def test_genre_list_matches_the_assertion_set_table() -> None:
  assert artifact_check.GENRES == ("plan", "understanding", "sitrep", "debug", "explain")
  assert set(artifact_check.GENRES) == set(artifact_check._ASSERTION_SETS)
  for names in artifact_check._ASSERTION_SETS.values():
    assert set(names) <= set(artifact_check._ASSERTION_RUNNERS)
    assert "ordinal-named" in names


def test_unknown_genre_is_refused(tmp_path: Path) -> None:
  artifact = _write(tmp_path, "<html></html>")
  with pytest.raises(ValueError, match="no assertion set registered for genre 'weird'"):
    run_assertions("weird", artifact)


def test_plan_assertion_set_carries_fork_explainer_after_fork_open_shape() -> None:
  names = artifact_check._ASSERTION_SETS["plan"]
  assert names.index("fork-explainer") == names.index("fork-open-shape") + 1
  assert len(names) == 8


# ---------------------------------------------------------------------------
# style-verbatim
# ---------------------------------------------------------------------------


def test_style_verbatim_passes_with_template_style(tmp_path: Path) -> None:
  outcomes = _run("sitrep", _write(tmp_path, _sitrep_ok_doc()))
  assert [o.passed for o in outcomes["style-verbatim"]] == [True]


def test_style_verbatim_fails_on_tampered_style(tmp_path: Path) -> None:
  doc = _sitrep_ok_doc().replace("</style>", "body{color:red}\n</style>", 1)
  outcomes = _run("sitrep", _write(tmp_path, doc))
  (outcome,) = outcomes["style-verbatim"]
  assert not outcome.passed
  assert "differs from the genre template prompts/sitrep_template.html" in outcome.detail


def test_style_verbatim_fails_when_style_block_missing(tmp_path: Path) -> None:
  outcomes = _run("sitrep", _write(tmp_path, "<html><body><p>x</p></body></html>"))
  (outcome,) = outcomes["style-verbatim"]
  assert not outcome.passed
  assert "0 <style> blocks" in outcome.detail


# ---------------------------------------------------------------------------
# sections-numbered
# ---------------------------------------------------------------------------


def test_sections_numbered_passes_one_through_five(tmp_path: Path) -> None:
  outcomes = _run("sitrep", _write(tmp_path, _sitrep_ok_doc()))
  assert [o.passed for o in outcomes["sections-numbered"]] == [True]


def test_sections_numbered_reports_the_read_sequence_on_a_gap(tmp_path: Path) -> None:
  doc = _sitrep_ok_doc().replace('<span class="n">3</span>', '<span class="n">4</span>')
  (outcome,) = _run("sitrep", _write(tmp_path, doc))["sections-numbered"]
  assert not outcome.passed
  assert "read 1, 2, 4, 4, 5" in outcome.detail
  assert "expected 1..5 contiguously" in outcome.detail


def test_sections_numbered_fails_wrong_count_for_genre(tmp_path: Path) -> None:
  doc = _genre_doc("sitrep", _sections(_SITREP_TITLES[:4]))
  (outcome,) = _run("sitrep", _write(tmp_path, doc))["sections-numbered"]
  assert not outcome.passed
  assert "sitrep requires exactly 5 numbered sections, found 4" in outcome.detail


def test_sections_numbered_fails_on_duplicate_span_n(tmp_path: Path) -> None:
  doc = _sitrep_ok_doc().replace('<span class="n">2</span>', '<span class="n">2</span><span class="n">2</span>')
  (outcome,) = _run("sitrep", _write(tmp_path, doc))["sections-numbered"]
  assert not outcome.passed
  assert "carries 2 span.n numbers" in outcome.detail


def test_sections_numbered_fails_on_non_integer_number(tmp_path: Path) -> None:
  doc = _sitrep_ok_doc().replace('<span class="n">2</span>', '<span class="n">II</span>')
  (outcome,) = _run("sitrep", _write(tmp_path, doc))["sections-numbered"]
  assert not outcome.passed
  assert "'II', not an integer" in outcome.detail


def test_sections_numbered_ignores_unnumbered_h2_and_h2less_sections(tmp_path: Path) -> None:
  doc = _genre_doc("sitrep", "<section><h2>Preamble</h2></section>" + _sections(_SITREP_TITLES))
  assert [o.passed for o in _run("sitrep", _write(tmp_path, doc))["sections-numbered"]] == [True]


def test_understanding_needs_at_least_five_numbered_sections(tmp_path: Path) -> None:
  cfg = _chrome_cfg(tmp_path)
  five = _write(
      tmp_path,
      _genre_doc("understanding",
                 _sections([f"S{i}" for i in range(1, 6)]) + '<div class="foot"><p>x</p></div>'))
  assert [o.passed for o in _run("understanding", five, cfg)["sections-numbered"]] == [True]
  four = _write(
      tmp_path,
      _genre_doc("understanding",
                 _sections([f"S{i}" for i in range(1, 5)]) + '<div class="foot"><p>x</p></div>'))
  (outcome,) = _run("understanding", four, cfg)["sections-numbered"]
  assert not outcome.passed
  assert "understanding requires at least 5 numbered sections, found 4" in outcome.detail


# ---------------------------------------------------------------------------
# foot-present / explain-triad / req-chips
# ---------------------------------------------------------------------------


def test_foot_present_pass_and_fail(tmp_path: Path) -> None:
  cfg = _chrome_cfg(tmp_path)
  assert [o.passed for o in _run("plan", _write(tmp_path, plan_page_html()), cfg)["foot-present"]] == [True]
  bare = _write(tmp_path, plan_page_html().replace('<div class="foot"><p>How to respond.</p></div>', ""))
  (outcome,) = _run("plan", bare, cfg)["foot-present"]
  assert not outcome.passed
  assert "no div.foot" in outcome.detail


def test_explain_triad_pass_and_fail(tmp_path: Path) -> None:
  with_triad = _genre_doc(
      "explain", '<section><div class="triad"><div class="row"><span class="k">You know</span>X</div></div></section>' +
      _sections([f"S{i}" for i in range(1, 6)]))
  assert [o.passed for o in _run("explain", _write(tmp_path, with_triad))["explain-triad"]] == [True]
  without = _genre_doc("explain", _sections([f"S{i}" for i in range(1, 6)]))
  (outcome,) = _run("explain", _write(tmp_path, without))["explain-triad"]
  assert not outcome.passed
  assert "no div.triad" in outcome.detail


def test_req_chips_pass_and_fail(tmp_path: Path) -> None:
  assert [o.passed for o in _run("sitrep", _write(tmp_path, _sitrep_ok_doc()))["req-chips"]] == [True]
  (outcome,) = _run("sitrep", _write(tmp_path,
                                     _sitrep_ok_doc().replace('<span class="req">r1</span>', "r1")))["req-chips"]
  assert not outcome.passed
  assert "no span.req" in outcome.detail


# ---------------------------------------------------------------------------
# fork-open-shape / fork-explainer
# ---------------------------------------------------------------------------

_OPEN_FORK = (
    '<div class="fork"><p class="q"><span class="fn">1</span>Scope?</p>'
    '<p class="rec"><b>Recommendation:</b> R</p><p class="trade">Tradeoff: T</p></div>')


def test_fork_open_shape_passes_full_and_skips_resolved(tmp_path: Path) -> None:
  resolved = (
      '<div class="fork"><p class="q"><span class="fn">2</span>Done?</p>'
      '<p class="resolved">Resolved: all.</p></div>')
  doc = _genre_doc("debug", _sections([f"S{i}" for i in range(1, 6)]) + f"<section>{_OPEN_FORK}{resolved}</section>")
  assert [o.passed for o in _run("debug", _write(tmp_path, doc))["fork-open-shape"]] == [True]


def test_fork_open_shape_locates_the_offending_fork_and_section(tmp_path: Path) -> None:
  fork = (
      '<div class="fork"><p class="q"><span class="fn">1</span>Scope?</p>'
      '<p class="rec"><b>Recommendation:</b> R</p></div>')
  doc = _genre_doc(
      "debug", f'<section><h2><span class="n">5</span> Decisions</h2>{fork}</section>' +
      _sections([f"S{i}" for i in range(1, 5)]))
  (outcome,) = _run("debug", _write(tmp_path, doc))["fork-open-shape"]
  assert not outcome.passed
  assert outcome.detail == "fork #1 (section '5 Decisions') is missing p.trade"


def test_fork_explainer_reports_every_open_fork_missing_a_details_layer(tmp_path: Path) -> None:
  fork = _OPEN_FORK
  doc = _genre_doc(
      "sitrep", f'<section><h2><span class="n">1</span> What waits on you?</h2>{fork}{fork}</section>'
      '<section><h2><span class="n">2</span> W</h2><p><span class="req">r1</span></p></section>'
      '<section><h2><span class="n">3</span> V</h2></section><section><h2><span class="n">4</span> R</h2></section>'
      '<section><h2><span class="n">5</span> N</h2></section>')
  outcomes = _run("sitrep", _write(tmp_path, doc))["fork-explainer"]
  assert len(outcomes) == 2
  assert all(not o.passed for o in outcomes)
  assert outcomes[0].detail == "fork #1 (section '1 What waits on you?') has no details.details-layer"
  assert outcomes[1].detail == "fork #2 (section '1 What waits on you?') has no details.details-layer"


def test_fork_explainer_passes_with_details_layer(tmp_path: Path) -> None:
  fork = _OPEN_FORK.replace(
      "</div>", '<details class="details-layer"><summary>Why</summary><ul><li>w</li></ul></details></div>')
  doc = _genre_doc(
      "sitrep",
      f'<section><h2><span class="n">1</span> S1</h2>{fork}</section>' + _sections([f"S{i}" for i in range(2, 6)]))
  doc = doc.replace(
      '<h2><span class="n">2</span> S2</h2>', '<h2><span class="n">2</span> S2</h2><p><span class="req">r1</span></p>')
  assert [o.passed for o in _run("sitrep", _write(tmp_path, doc))["fork-explainer"]] == [True]


def test_fork_explainer_fails_when_details_layer_carries_no_body(tmp_path: Path) -> None:
  """A details-layer holding only its summary — the summary's own text does not count as the body."""
  fork = _OPEN_FORK.replace("</div>", '<details class="details-layer"><summary>Why</summary></details></div>')
  doc = _genre_doc(
      "sitrep",
      f'<section><h2><span class="n">1</span> S1</h2>{fork}</section>' + _sections([f"S{i}" for i in range(2, 6)]))
  doc = doc.replace(
      '<h2><span class="n">2</span> S2</h2>', '<h2><span class="n">2</span> S2</h2><p><span class="req">r1</span></p>')
  (outcome,) = _run("sitrep", _write(tmp_path, doc))["fork-explainer"]
  assert not outcome.passed
  assert outcome.detail == "fork #1 (section '1 S1') explainer has no body"


# ---------------------------------------------------------------------------
# fact-anchored
# ---------------------------------------------------------------------------


def test_fact_anchored_passes_with_src_in_the_same_block(tmp_path: Path) -> None:
  outcomes = _run("sitrep", _write(tmp_path, _sitrep_ok_doc()))
  assert [o.passed for o in outcomes["fact-anchored"]] == [True]


def test_fact_anchored_fails_without_src_and_reports_block_tag(tmp_path: Path) -> None:
  doc = _sitrep_ok_doc().replace('The reading holds. <span class="src">s</span>', 'The reading holds.')
  (outcome,) = _run("sitrep", _write(tmp_path, doc))["fact-anchored"]
  assert not outcome.passed
  assert outcome.detail == "fact label #1 (section '4 Risks') sits in a p with no span.src"


def test_fact_anchored_fails_on_a_label_parked_in_a_bare_div(tmp_path: Path) -> None:
  doc = _genre_doc(
      "debug",
      _sections([f"S{i}" for i in range(1, 6)]) +
      '<section><div><span class="tag fact">Fact</span> Loose.</div></section>')
  (outcome,) = _run("debug", _write(tmp_path, doc))["fact-anchored"]
  assert not outcome.passed
  assert "has no p/li/td/blockquote/h3 ancestor" in outcome.detail


def test_fact_anchored_counts_labels_in_document_order(tmp_path: Path) -> None:
  labels = (
      '<p><span class="tag fact">Fact</span> kept <span class="src">s</span></p>'
      '<li><span class="tag fact">Fact</span> dropped</li>')
  doc = _genre_doc(
      "debug",
      _sections(["S1", "S2", "S3"]) + '<section><h2><span class="n">4</span> Evidence</h2><ul>' + labels +
      '</ul></section>'
      '<section><h2><span class="n">5</span> S5</h2></section>')
  outcomes = _run("debug", _write(tmp_path, doc))["fact-anchored"]
  # The first label anchors in its own p; the second li carries no span.src nearby.
  assert [o.passed for o in outcomes] == [False]
  assert outcomes[0].detail == "fact label #2 (section '4 Evidence') sits in a li with no span.src"


# ---------------------------------------------------------------------------
# goal-budget (moved measurement, unchanged budgets)
# ---------------------------------------------------------------------------


def test_goal_budget_passes_short_goal(tmp_path: Path) -> None:
  cfg = _chrome_cfg(tmp_path)
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html()), cfg)["goal-budget"]
  assert outcome.passed
  assert outcome.detail == f"13 weighted chars (budget {artifact_check.GOAL_WEIGHTED_BUDGET})"


def test_goal_budget_rejects_over_budget_goal(tmp_path: Path) -> None:
  over = artifact_check.GOAL_WEIGHTED_BUDGET + 1
  cfg = _chrome_cfg(tmp_path)
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html(goal_body="x" * over)), cfg)["goal-budget"]
  assert not outcome.passed
  assert f"{over} weighted chars (budget {artifact_check.GOAL_WEIGHTED_BUDGET})" in outcome.detail


def test_goal_budget_rejects_missing_goal_section(tmp_path: Path) -> None:
  cfg = _chrome_cfg(tmp_path)
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html().replace("Problem / Goal", "Purpose")), cfg)["goal-budget"]
  assert not outcome.passed
  assert "no 'Problem / Goal' section" in outcome.detail


def test_goal_budget_counts_cjk_double(tmp_path: Path) -> None:
  cfg = _chrome_cfg(tmp_path)
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html(goal_body="一" * 121)), cfg)["goal-budget"]
  assert not outcome.passed
  assert "242 weighted chars" in outcome.detail


def _with_mark_after_goal_heading(mark: str) -> str:
  return plan_page_html().replace("Problem / Goal</h2>", f"Problem / Goal</h2>{mark}", 1)


def test_goal_budget_drops_a_revnote_whole(tmp_path: Path) -> None:
  """A div.revnote right after section 1's h2 — where the plan template puts it — rides outside
  the budget (cleared by the next revision): an overlong note changes nothing, inline <code>
  included."""
  cfg = _chrome_cfg(tmp_path)
  note = (
      f'<div class="revnote">r2 · trigger — {"x" * (artifact_check.GOAL_WEIGHTED_BUDGET + 40)} '
      "<code>a/b/c</code></div>")
  marked = _run("plan", _write(tmp_path, _with_mark_after_goal_heading(note)), cfg)["goal-budget"]
  plain = _run("plan", _write(tmp_path, plan_page_html(), name="plain.html"), cfg)["goal-budget"]
  assert marked == plain


def test_goal_budget_drops_a_body_revbadge(tmp_path: Path) -> None:
  """A span.revbadge inside section 1's body (not in the heading) does not contribute either."""
  cfg = _chrome_cfg(tmp_path)
  badge = '<span class="revbadge">changed · r3</span>'
  marked = _run("plan", _write(tmp_path, _with_mark_after_goal_heading(badge)), cfg)["goal-budget"]
  plain = _run("plan", _write(tmp_path, plan_page_html(), name="plain.html"), cfg)["goal-budget"]
  assert marked == plain


def test_goal_budget_keeps_non_revision_class_content(tmp_path: Path) -> None:
  """Only exact revision class tokens are outside the budget; lookalike attributes still count."""
  cfg = _chrome_cfg(tmp_path)
  content = "x" * (artifact_check.GOAL_WEIGHTED_BUDGET + 1)
  lookalikes = (f'<div class="revnote-old">{content}</div>', f'<div data-class="revnote">{content}</div>')
  for i, mark in enumerate(lookalikes):
    (outcome,) = _run("plan", _write(tmp_path, _with_mark_after_goal_heading(mark), name=f"lookalike-{i}.html"),
                      cfg)["goal-budget"]
    assert not outcome.passed


def test_goal_budget_still_rejects_an_over_budget_goal_alongside_marks(tmp_path: Path) -> None:
  """Stripping revision marks does not weaken the budget: an over-budget goal still fails."""
  cfg = _chrome_cfg(tmp_path)
  over = artifact_check.GOAL_WEIGHTED_BUDGET + 1
  doc = plan_page_html(goal_body="x" * over).replace(
      "Problem / Goal</h2>", 'Problem / Goal</h2><div class="revnote">r2 · note</div>', 1)
  (outcome,) = _run("plan", _write(tmp_path, doc), cfg)["goal-budget"]
  assert not outcome.passed
  assert f"{over} weighted chars (budget {artifact_check.GOAL_WEIGHTED_BUDGET})" in outcome.detail


# ---------------------------------------------------------------------------
# page-height (moved measurement, unchanged budgets)
# ---------------------------------------------------------------------------


def _patch_height(monkeypatch: pytest.MonkeyPatch, height: int) -> None:
  monkeypatch.setattr(artifact_check, "_measure_page_height", lambda chrome, art: height)


def test_page_height_failure_names_measured_and_budget(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _patch_height(monkeypatch, 1700)
  cfg = SimpleNamespace(headless_chrome_bin=sys.executable)  # exists, satisfying the binary check
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html()), cfg)["page-height"]
  assert not outcome.passed
  assert "1700 px as it opens: 100 px over" in outcome.detail
  assert f"{artifact_check.PAGE_HEIGHT_BUDGET} px budget" in outcome.detail


def test_page_height_passes_at_budget(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _patch_height(monkeypatch, 1600)
  cfg = SimpleNamespace(headless_chrome_bin=sys.executable)
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html()), cfg)["page-height"]
  assert outcome.passed
  assert outcome.detail == f"1600 px (budget {artifact_check.PAGE_HEIGHT_BUDGET})"


def test_page_height_rejects_missing_renderer(tmp_path: Path) -> None:
  (outcome,) = _run("plan", _write(tmp_path, plan_page_html()), SimpleNamespace(headless_chrome_bin=""))["page-height"]
  assert not outcome.passed
  assert "headless_chrome_bin is required" in outcome.detail


# ---------------------------------------------------------------------------
# ordinal-named
# ---------------------------------------------------------------------------

_ORDINAL_SENTENCE = "计划 section 1 的措辞怎么收？"


def _ordinal_doc(body: str, *, numbered_h2: int = 5, sn: tuple[str, ...] = ()) -> str:
  """Debug-genre page for one ordinal-named sentence: *numbered_h2* numbered sections, optional
  span.sn subsection markers, and *body* as one trailing paragraph."""
  return _genre_doc(
      "debug",
      _sections([f"S{i}" for i in range(1, numbered_h2 + 1)]) + "".join(f'<span class="sn">{s}</span>' for s in sn) +
      f"<p>{body}</p>")


def _ordinal_outcome(tmp_path: Path, body: str, **kwargs) -> artifact_check.AssertionOutcome:
  (outcome,) = _run("debug", _write(tmp_path, _ordinal_doc(body, **kwargs)))["ordinal-named"]
  return outcome


def test_ordinal_named_own_structure_depends_on_h2_count(tmp_path: Path) -> None:
  """The same sentence 「见第 3 节」 is the page's own structure with 3 numbered h2 and an outside
  reference with 2."""
  assert _ordinal_outcome(tmp_path, "见第 3 节", numbered_h2=3).passed
  outcome = _ordinal_outcome(tmp_path, "见第 3 节", numbered_h2=2)
  assert not outcome.passed
  assert "见第 3 节" in outcome.detail


def test_ordinal_named_document_word_marks_external(tmp_path: Path) -> None:
  """「计划第 2 节」 points off the page even when the number is inside the page's own structure;
  adding a path parenthetical names it."""
  outcome = _ordinal_outcome(tmp_path, "计划第 2 节", numbered_h2=6)
  assert not outcome.passed
  assert _ordinal_outcome(tmp_path, "计划第 2 节（artifacts/plan_01.html）", numbered_h2=6).passed


def test_ordinal_named_first_use_order(tmp_path: Path) -> None:
  """A naming sentence before a bare use passes; the bare use before the naming sentence fails on
  the earlier sentence alone."""
  named_first = _genre_doc("debug", _sections([f"S{i}" for i in range(1, 6)]) + "<p>plan 3：这样收</p><p>plan 3 的收法</p>")
  assert _run("debug", _write(tmp_path, named_first))["ordinal-named"][0].passed
  bare_first = _genre_doc("debug", _sections([f"S{i}" for i in range(1, 6)]) + "<p>plan 3 的收法</p><p>plan 3：这样收</p>")
  (outcome,) = _run("debug", _write(tmp_path, bare_first))["ordinal-named"]
  assert not outcome.passed
  assert "plan 3 的收法" in outcome.detail


@pytest.mark.parametrize(
    ("sentence", "kwargs"),
    [
        ("plan 3 见 https://example.com/a", {}),  # URL
        ("计划 section 1 改 ~/a/b", {}),  # rooted path
        ("见 plan 3 与 a/b/c 的差异", {}),  # relative path with two or more slashes
        ("见 plan 3 与 plan.md", {}),  # file name with a document extension
        ("见 plan 3 与 ABC-12", {}),  # ticket id
        ("见 plan 3，标题：「" + "一" * 81 + "」", {}),  # quoted title long enough to stay unmasked
        ("计划第 2 节（见下）", {
            "numbered_h2": 6
        }),  # parenthetical gloss right after the label
        ("section 1 · 计划目标", {
            "numbered_h2": 0
        }),  # 「 · 」 value after the label
        ("plan 3：这样收", {}),  # opening label with colon gloss
        ("页面检查流水线（plan 3）", {}),  # label inside a parenthetical following content
    ])
def test_ordinal_named_content_name_forms_pass(tmp_path: Path, sentence: str, kwargs: dict) -> None:
  assert _ordinal_outcome(tmp_path, sentence, **kwargs).passed


def test_ordinal_named_chip_value_names_the_label(tmp_path: Path) -> None:
  """Inside a span.mtag chip the text after the label is the chip's value."""
  doc = _genre_doc("debug", _sections([f"S{i}" for i in range(1, 6)]) + '<span class="mtag">主题 · plan 3 v2</span>')
  assert _run("debug", _write(tmp_path, doc))["ordinal-named"][0].passed


@pytest.mark.parametrize("sentence", ["见 plan 3 与 A/B 的差异", "见 plan 3 与 TFLOP/s 的关系"])
def test_ordinal_named_single_slash_forms_are_not_paths(tmp_path: Path, sentence: str) -> None:
  assert not _ordinal_outcome(tmp_path, sentence).passed


@pytest.mark.parametrize("quote", [("「", "」"), ('"', '"')])
def test_ordinal_named_quoted_label_is_a_mention(tmp_path: Path, quote: tuple[str, str]) -> None:
  assert _ordinal_outcome(tmp_path, f"拿{quote[0]}计划 section 1{quote[1]}这类编号指代").passed


@pytest.mark.parametrize("sentence", ["第 2 轮", "round 2", "item 5"])
def test_ordinal_named_own_sequence_labels_pass(tmp_path: Path, sentence: str) -> None:
  assert _ordinal_outcome(tmp_path, sentence).passed


def test_ordinal_named_document_word_qualifies_own_sequence(tmp_path: Path) -> None:
  assert not _ordinal_outcome(tmp_path, "plan 的 round 2").passed


def test_ordinal_named_dotted_labels_and_digit_boundaries(tmp_path: Path) -> None:
  """4.1 is the page's own subsection only through span.sn (two numbered h2 alone cannot cover it);
  decimals, hashes, and batch counts are not labels."""
  assert _ordinal_outcome(tmp_path, "其 4.1 节", numbered_h2=2, sn=("4.1",)).passed
  assert not _ordinal_outcome(tmp_path, "其 4.1 节", numbered_h2=2).passed
  assert not _ordinal_outcome(tmp_path, "计划的 4.1 节", numbered_h2=2, sn=("4.1",)).passed
  for sentence in ["共 4.9 MB", "的 0.1", "fork 2a92675f", "plan(3)"]:
    assert _ordinal_outcome(tmp_path, sentence, numbered_h2=2).passed, sentence


def test_ordinal_named_pre_and_code_literals(tmp_path: Path) -> None:
  """pre subtrees are not scanned, a label inside code is a mention, and code text in the same
  sentence still anchors a bare label outside the code."""
  sections = _sections([f"S{i}" for i in range(1, 6)])
  assert _run("debug", _write(tmp_path, _genre_doc("debug",
                                                   sections + "<pre>计划 section 1</pre>")))["ordinal-named"][0].passed
  assert _run("debug", _write(tmp_path,
                              _genre_doc("debug",
                                         sections + "<p>见 <code>plan 3</code> 的输出</p>")))["ordinal-named"][0].passed
  assert _run("debug",
              _write(tmp_path,
                     _genre_doc("debug",
                                sections + "<p>plan 3 见 <code>/a/b/c</code> 下的说明</p>")))["ordinal-named"][0].passed


def _genre_ok_doc(genre: str) -> str:
  """Minimal page that passes its genre's pre-existing assertions."""
  if genre == "plan":
    return plan_page_html()
  if genre == "understanding":
    return _genre_doc("understanding", _sections([f"S{i}" for i in range(1, 6)]) + '<div class="foot"><p>x</p></div>')
  if genre == "sitrep":
    return _sitrep_ok_doc()
  if genre == "debug":
    return _debug_ok_doc()
  return _genre_doc(
      "explain", '<section><div class="triad"><div class="row"><span class="k">You know</span>X</div></div></section>' +
      _sections([f"S{i}" for i in range(1, 6)]))


@pytest.mark.parametrize("genre", ["plan", "understanding", "sitrep", "debug", "explain"])
def test_ordinal_named_runs_in_every_genre(tmp_path: Path, genre: str) -> None:
  """The sentence fails ordinal-named on a minimal compliant page of every genre and passes without it."""
  cfg = _chrome_cfg(tmp_path)
  assert [
      o.passed
      for o in _run(genre, _write(tmp_path, _genre_ok_doc(genre), name=f"{genre}-clean.html"), cfg)["ordinal-named"]
  ] == [True]
  offending = _genre_ok_doc(genre).replace("</body></html>", f"<p>{_ORDINAL_SENTENCE}</p></body></html>")
  outcomes = _run(genre, _write(tmp_path, offending, name=f"{genre}-offending.html"), cfg)["ordinal-named"]
  assert [o.passed for o in outcomes] == [False]
  assert _ORDINAL_SENTENCE in outcomes[0].detail


# ---------------------------------------------------------------------------
# Shipped templates pass their own genre
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("genre", "template"),
    [
        ("plan", "plan_template.html"),
        ("sitrep", "sitrep_template.html"),
        ("debug", "debug_template.html"),
        ("explain", "explain_template.html"),
    ],
)
def test_shipped_template_passes_its_own_genre(tmp_path: Path, genre: str, template: str) -> None:
  cfg = _chrome_cfg(tmp_path)
  outcomes = run_assertions(genre, _REPO_ROOT / "prompts" / template, cfg)
  assert [o for o in outcomes if not o.passed] == []


# ---------------------------------------------------------------------------
# CLI: charliebot artifact check
# ---------------------------------------------------------------------------


def _cli_ok_cfg(tmp_path: Path) -> SimpleNamespace:
  return _chrome_cfg(tmp_path)


def _invoke(argv_tail: list[str]) -> SystemExit:
  with patch("sys.argv", ["artifact", "check", *argv_tail]), pytest.raises(SystemExit) as exc_info:
    artifact_main()
  return exc_info.value


def _run_cli(argv_tail: list[str]) -> int:
  exit_ = _invoke(argv_tail)
  assert isinstance(exit_.code, int)
  return exit_.code


def test_cli_plan_template_assertions_only_passes_and_prints_ok_lines(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, plan_page_html())
  monkeypatch.setattr(_CLI_ARTIFACT_GET_CONFIG_PATCH_TARGET, lambda: _cli_ok_cfg(tmp_path))
  assert _run_cli([str(artifact), "--genre", "plan", "--assertions-only"]) == 0
  assert capsys.readouterr().out.splitlines() == [
      "ok style-verbatim",
      "ok sections-numbered",
      "ok foot-present",
      "ok fork-open-shape",
      "ok fork-explainer",
      f"ok goal-budget 13 weighted chars (budget {artifact_check.GOAL_WEIGHTED_BUDGET})",
      f"ok page-height 800 px (budget {artifact_check.PAGE_HEIGHT_BUDGET})",
      "ok ordinal-named 0 external labels named in reach",
  ]


def _open_fork_sitrep_doc() -> str:
  return _genre_doc(
      "sitrep", f'<section><h2><span class="n">1</span> What waits on you?</h2>{_OPEN_FORK}{_OPEN_FORK}</section>'
      '<section><h2><span class="n">2</span> What is this and why?</h2><p><span class="req">r1</span></p></section>'
      '<section><h2><span class="n">3</span> What was verified?</h2></section>'
      '<section><h2><span class="n">4</span> Risks</h2></section>'
      '<section><h2><span class="n">5</span> What happens next?</h2></section>')


def _open_fork_plan_doc() -> str:
  return plan_page_html().replace(
      '<h2><span class="n">5</span> Trade-offs</h2>',
      f'<h2><span class="n">5</span> Trade-offs</h2>{_OPEN_FORK}{_OPEN_FORK}')


@pytest.mark.parametrize(
    ("genre", "make_doc", "section"),
    [
        ("sitrep", _open_fork_sitrep_doc, "1 What waits on you?"),
        ("plan", _open_fork_plan_doc, "5 Trade-offs"),
    ],
    ids=["sitrep", "plan"],
)
def test_cli_two_open_forks_without_explainer_report_two_locations_and_skip_the_probe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], genre: str,
    make_doc: Callable[[], str], section: str) -> None:
  """Two open forks lacking details.details-layer: two FAIL lines, exit 1, no model call."""
  artifact = _write(tmp_path, make_doc())
  factory_called: list = []

  def factory(option, cfg):
    factory_called.append(option.id)
    raise AssertionError("the probe must never run when an assertion failed")

  monkeypatch.setattr(artifact_check, "build_backend", factory)
  monkeypatch.setattr(_CLI_ARTIFACT_GET_CONFIG_PATCH_TARGET, lambda: _cli_ok_cfg(tmp_path))
  assert _run_cli([str(artifact), "--genre", genre, "--trigger", "where are we?"]) == 1
  out = capsys.readouterr().out
  fail_lines = [line for line in out.splitlines() if line.startswith("FAIL fork-explainer")]
  assert fail_lines == [
      f"FAIL fork-explainer: fork #1 (section '{section}') has no details.details-layer",
      f"FAIL fork-explainer: fork #2 (section '{section}') has no details.details-layer",
  ]
  assert "--- cold read ---" not in out
  assert not factory_called


def _probe_cfg(tmp_path: Path) -> tuple[SimpleNamespace, dict[str, SimpleNamespace]]:
  options = {name: SimpleNamespace(id=name) for name in ("alpha", "beta")}
  cfg = SimpleNamespace(
      headless_chrome_bin=write_stub_chrome(tmp_path, 800),
      model_preference=["alpha", "beta"],
      get_backend_option=options.get,
  )
  return cfg, options


class _FakeBackend:

  def __init__(self, answer: str | None = None, error: str | None = None) -> None:
    self._answer = answer
    self._error = error
    self.calls: list[dict] = []

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float) -> str:
    self.calls.append({"prompt": prompt, "system_prompt": system_prompt, "timeout": timeout})
    if self._error is not None:
      raise RuntimeError(self._error)
    assert self._answer is not None
    return self._answer


def _patch_backends(monkeypatch: pytest.MonkeyPatch, backends: dict[str, _FakeBackend]) -> None:
  monkeypatch.setattr(artifact_check, "build_backend", lambda option, cfg: backends[option.id])


def test_cli_probe_runs_after_assertions_pass_and_prints_backend_and_answers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  backends = {
      "alpha": _FakeBackend(error="boom"),
      "beta": _FakeBackend(answer="(1) The reader's problem.\n(2)-(6) fine.")
  }
  _patch_backends(monkeypatch, backends)
  monkeypatch.setattr(_CLI_ARTIFACT_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 0
  lines = capsys.readouterr().out.splitlines()
  assert lines[-5:] == [
      "--- cold read ---",
      "attempt alpha failed: boom",
      "backend beta",
      "(1) The reader's problem.",
      "(2)-(6) fine.",
  ]
  # The page text and the substituted trigger ride in one prompt; the placeholder is gone.
  prompt = backends["beta"].calls[0]["prompt"]
  assert artifact.read_text(encoding="utf-8") in prompt
  assert '"where are we?"' in prompt
  assert "(7) List every term" in prompt
  assert "<trigger message verbatim>" not in prompt
  assert backends["beta"].calls[0]["timeout"] == artifact_check.ARTIFACT_PROBE_TIMEOUT == 300.0


@pytest.mark.parametrize("genre", ["plan", "understanding"])
def test_cli_probe_runs_after_assertions_pass_on_plan_and_understanding(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], genre: str) -> None:
  artifact = _write(tmp_path, _genre_ok_doc(genre))
  cfg, _options = _probe_cfg(tmp_path)
  backends = {
      "alpha": _FakeBackend(error="boom"),
      "beta": _FakeBackend(answer="(1) The reader's problem.\n(2)-(7) fine.")
  }
  _patch_backends(monkeypatch, backends)
  monkeypatch.setattr(_CLI_ARTIFACT_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  assert _run_cli([str(artifact), "--genre", genre, "--trigger", "where are we?"]) == 0
  lines = capsys.readouterr().out.splitlines()
  assert lines[-5:] == [
      "--- cold read ---",
      "attempt alpha failed: boom",
      "backend beta",
      "(1) The reader's problem.",
      "(2)-(7) fine.",
  ]


def test_cli_probe_exit_1_when_every_backend_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _debug_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  _patch_backends(monkeypatch, {"alpha": _FakeBackend(error="down"), "beta": _FakeBackend(error="dead")})
  monkeypatch.setattr(_CLI_ARTIFACT_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  assert _run_cli([str(artifact), "--genre", "debug", "--trigger", "what broke?"]) == 1

  out = capsys.readouterr().out
  assert "attempt alpha failed: down" in out
  assert "attempt beta failed: dead" in out
  assert "probe could not run" in out


@pytest.mark.parametrize(
    ("genre", "make_doc"),
    [
        ("sitrep", _sitrep_ok_doc),
        ("plan", plan_page_html),
    ],
    ids=["sitrep", "plan"],
)
def test_cli_assertions_only_skips_probe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], genre: str,
    make_doc: Callable[[], str]) -> None:
  artifact = _write(tmp_path, make_doc())
  monkeypatch.setattr(artifact_check, "build_backend", lambda option, cfg: pytest.fail("probe must not run"))
  monkeypatch.setattr(_CLI_ARTIFACT_GET_CONFIG_PATCH_TARGET, lambda: _cli_ok_cfg(tmp_path))
  assert _run_cli([str(artifact), "--genre", genre, "--assertions-only"]) == 0
  assert "--- cold read ---" not in capsys.readouterr().out


@pytest.mark.parametrize("genre", ["sitrep", "plan"], ids=["sitrep", "plan"])
def test_cli_missing_trigger_is_usage_error(genre: str, capsys: pytest.CaptureFixture[str]) -> None:
  assert _run_cli(["page.html", "--genre", genre]) == 2
  assert "--trigger" in capsys.readouterr().err


def test_cli_missing_genre_is_usage_error(capsys: pytest.CaptureFixture[str]) -> None:
  assert _run_cli(["page.html"]) == 2


def test_cli_unknown_genre_is_usage_error(capsys: pytest.CaptureFixture[str]) -> None:
  assert _run_cli(["page.html", "--genre", "weird"]) == 2


def test_cli_missing_file_errors(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  monkeypatch.setattr(_CLI_ARTIFACT_GET_CONFIG_PATCH_TARGET, lambda: _cli_ok_cfg(tmp_path))
  assert _run_cli([str(tmp_path / "nope.html"), "--genre", "plan", "--assertions-only"]) == 1
  assert "artifact not found" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# Registration parity: plan present enforces exactly artifact check --genre plan
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plan_present_and_artifact_check_reject_the_same_assertions_on_one_fixture(tmp_path: Path) -> None:
  broken = plan_page_html().replace('<span class="n">4</span>', '<span class="n">9</span>').replace(
      '<div class="foot"><p>How to respond.</p></div>', "")
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await make_plan_setup(tmp_path)
  file_rel = write_plan_artifact(cfg, meta.id, "plan_01.html", content=broken)

  cli_failures = {o.name for o in run_assertions("plan", cfg.sessions_dir / meta.id / file_rel, cfg) if not o.passed}
  assert cli_failures == {"sections-numbered", "foot-present"}

  with pytest.raises(ValueError) as exc_info:
    await plan_mgr.present(meta.id, file=file_rel, title="P1")
  message = str(exc_info.value)
  for name in cli_failures:
    assert name in message
  assert message.endswith(
      "Measure locally with: charliebot artifact check <artifact.html> --genre plan --assertions-only")


@pytest.mark.asyncio
async def test_plan_present_never_calls_build_backend(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Registration stays model-free: present runs the assertions and constructs no backend."""
  monkeypatch.setattr(
      artifact_check, "build_backend", lambda option, cfg: pytest.fail("registration must not build a backend"))
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await make_plan_setup(tmp_path)
  file_rel = write_plan_artifact(cfg, meta.id, "plan_01.html")
  result = await plan_mgr.present(meta.id, file=file_rel, title="P1")
  assert result["state"] == "awaiting approval"


# ---------------------------------------------------------------------------
# run_probe unit behavior
# ---------------------------------------------------------------------------


def test_run_probe_returns_attempts_and_first_answering_backend(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cfg, _options = _probe_cfg(tmp_path)
  backends = {"alpha": _FakeBackend(error="boom"), "beta": _FakeBackend(answer="six answers")}
  _patch_backends(monkeypatch, backends)
  artifact = _write(tmp_path, "<html>page text</html>")
  result = run_probe(cfg, artifact, "the trigger")
  assert result.attempts == [("alpha", "boom")]
  assert result.backend_id == "beta"
  assert result.answer == "six answers"


def test_run_probe_all_backends_failing(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cfg, _options = _probe_cfg(tmp_path)
  _patch_backends(monkeypatch, {"alpha": _FakeBackend(error="a"), "beta": _FakeBackend(error="b")})
  result = run_probe(cfg, _write(tmp_path, "<html>x</html>"), "t")
  assert result.backend_id is None and result.answer is None
  assert [bid for bid, _ in result.attempts] == ["alpha", "beta"]


def test_run_probe_raises_without_resolvable_backends(tmp_path: Path) -> None:
  cfg = SimpleNamespace(model_preference=[], get_backend_option=lambda entry_id: None)
  with pytest.raises(ValueError, match="no light backends resolvable"):
    run_probe(cfg, _write(tmp_path, "<html>x</html>"), "t")

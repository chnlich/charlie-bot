"""Tests for the plan registration gates and the local ``charliebot plan check`` dry run.

The goal and height gates live in src.core.plans; ``plan check`` (src.cli.plan) runs the
same measurements locally with no session, no HTTP, and no registry write. Renderer work
is monkeypatched out — no headless chrome is involved here.
"""

import sys
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from src.cli.plan import main as plan_main
from src.core import plans


def _goal_doc(body: str) -> str:
  return f"<html><section><h2>1 Problem / Goal</h2>{body}</section></html>"


def _write_artifact(tmp_path: Path, content: str) -> Path:
  artifact = tmp_path / "plan.html"
  artifact.write_text(content, encoding="utf-8")
  return artifact


# ---------------------------------------------------------------------------
# Goal gate
# ---------------------------------------------------------------------------


def test_goal_gate_passes_short_goal(tmp_path: Path) -> None:
  artifact = _write_artifact(tmp_path, _goal_doc("<p>Short goal.</p>"))
  plans._check_goal_budget(artifact)  # no raise


def test_goal_gate_rejects_over_budget_goal(tmp_path: Path) -> None:
  over = plans.GOAL_WEIGHTED_BUDGET + 1
  artifact = _write_artifact(tmp_path, _goal_doc(f"<p>{'x' * over}</p>"))
  with pytest.raises(ValueError, match=rf"{over} weighted chars \(budget {plans.GOAL_WEIGHTED_BUDGET}\)"):
    plans._check_goal_budget(artifact)


def test_goal_gate_rejects_missing_goal_section(tmp_path: Path) -> None:
  artifact = _write_artifact(tmp_path, "<html><body><p>nothing</p></body></html>")
  with pytest.raises(ValueError, match="no 'Problem / Goal' section"):
    plans._check_goal_budget(artifact)


def test_goal_gate_counts_cjk_double(tmp_path: Path) -> None:
  artifact = _write_artifact(tmp_path, _goal_doc("<p>" + "一" * 121 + "</p>"))
  with pytest.raises(ValueError, match="242 weighted chars"):
    plans._check_goal_budget(artifact)


# ---------------------------------------------------------------------------
# Height gate
# ---------------------------------------------------------------------------


def _chrome_cfg() -> SimpleNamespace:
  # sys.executable always exists, satisfying the renderer-path existence check.
  return SimpleNamespace(headless_chrome_bin=sys.executable)


def test_height_gate_rejection_message(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  artifact = _write_artifact(tmp_path, _goal_doc("<p>Short goal.</p>"))
  monkeypatch.setattr(plans, "_measure_page_height", lambda chrome, art: 1700)
  with pytest.raises(ValueError) as exc_info:
    plans._check_page_height(_chrome_cfg(), artifact)
  message = str(exc_info.value)
  assert "1700 px" in message
  assert f"{1700 - plans.PAGE_HEIGHT_BUDGET} px over" in message
  assert f"{plans.PAGE_HEIGHT_BUDGET} px budget" in message
  assert "charliebot plan check" in message


def test_height_gate_passes_at_budget(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  artifact = _write_artifact(tmp_path, _goal_doc("<p>Short goal.</p>"))
  monkeypatch.setattr(plans, "_measure_page_height", lambda chrome, art: 1600)
  plans._check_page_height(_chrome_cfg(), artifact)  # no raise


def test_height_gate_rejects_missing_renderer(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  artifact = _write_artifact(tmp_path, _goal_doc("<p>Short goal.</p>"))

  def measure(chrome: Path, art: Path) -> int:
    pytest.fail("renderer must not launch without a configured binary")

  monkeypatch.setattr(plans, "_measure_page_height", measure)
  with pytest.raises(ValueError, match="headless_chrome_bin is required"):
    plans._check_page_height(SimpleNamespace(headless_chrome_bin=""), artifact)


# ---------------------------------------------------------------------------
# charliebot plan check
# ---------------------------------------------------------------------------


def _check(monkeypatch: pytest.MonkeyPatch, artifact: Path, measured_height: int) -> None:
  monkeypatch.setattr(plans, "_measure_page_height", lambda chrome, art: measured_height)
  monkeypatch.setattr("src.cli.plan.get_config", _chrome_cfg)
  with patch("sys.argv", ["plan", "check", "--file", str(artifact)]), pytest.raises(SystemExit) as exc_info:
    plan_main()
  assert exc_info.value.code == (0 if measured_height <= plans.PAGE_HEIGHT_BUDGET else 1)


def test_plan_check_pass_prints_both_gate_lines_and_exits_0(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write_artifact(tmp_path, _goal_doc("<p>Short goal.</p>"))
  _check(monkeypatch, artifact, 1375)
  assert capsys.readouterr().out.splitlines() == [
      f"page height: 1375 px (budget {plans.PAGE_HEIGHT_BUDGET}) PASS",
      f"goal weighted length: 11 (budget {plans.GOAL_WEIGHTED_BUDGET}) PASS",
  ]


def test_plan_check_over_budget_height_prints_fail_and_exits_1(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write_artifact(tmp_path, _goal_doc("<p>Short goal.</p>"))
  _check(monkeypatch, artifact, 1700)
  assert capsys.readouterr().out.splitlines() == [
      f"page height: 1700 px (budget {plans.PAGE_HEIGHT_BUDGET}) FAIL",
      f"goal weighted length: 11 (budget {plans.GOAL_WEIGHTED_BUDGET}) PASS",
  ]


def test_plan_check_missing_goal_section_errors_to_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write_artifact(tmp_path, "<html><body><p>nothing</p></body></html>")
  monkeypatch.setattr("src.cli.plan.get_config", _chrome_cfg)
  with patch("sys.argv", ["plan", "check", "--file", str(artifact)]), pytest.raises(SystemExit) as exc_info:
    plan_main()
  assert exc_info.value.code == 1
  assert "no 'Problem / Goal' section" in capsys.readouterr().err


def test_plan_check_missing_renderer_errors_to_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write_artifact(tmp_path, _goal_doc("<p>Short goal.</p>"))
  monkeypatch.setattr("src.cli.plan.get_config", lambda: SimpleNamespace(headless_chrome_bin="/nonexistent/chrome"))
  with patch("sys.argv", ["plan", "check", "--file", str(artifact)]), pytest.raises(SystemExit) as exc_info:
    plan_main()
  assert exc_info.value.code == 1
  assert "headless_chrome_bin is required" in capsys.readouterr().err


def test_plan_check_needs_no_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """check runs outside any session dir: no --session flag, no session resolution."""
  artifact = _write_artifact(tmp_path, _goal_doc("<p>Short goal.</p>"))
  monkeypatch.chdir(tmp_path)  # not a session dir — resolve_session_id would exit 2 here
  _check(monkeypatch, artifact, 1375)
  assert capsys.readouterr().out.splitlines()[0].endswith("PASS")

"""Tests for src/features/artifacts/artifact_check.py and the ``charliebot artifact check`` CLI.

The goal-length and page-height measurements moved here from src/features/artifacts/plans.py with their
budgets unchanged; the DOM assertions are the new mechanical half of each genre's GRAMMAR.
Renderer work and the probe's model call are doubled out — no headless chrome, no backend
subprocess, no HTTP.
"""

import re
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    CLI_COMMON_GET_CONFIG_PATCH_TARGET,
    ROOT,
    make_plan_setup,
    plan_page_html,
    write_plan_artifact,
    write_stub_chrome,
)

from src.features.artifacts import artifact_check, artifact_shared
from src.features.artifacts.artifact_check import run_assertions
from src.features.artifacts.cli import main as artifact_main
from src.infra.config import CharlieBotConfig


def _genre_doc(genre: str, body: str) -> str:
  """Full HTML document for *genre*: its template's <style> block verbatim plus *body*."""
  template = (ROOT / "prompts" / artifact_shared.GENRE_TEMPLATES[genre]).read_text(encoding="utf-8")
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


def _run(
    genre: str,
    artifact: Path,
    cfg: CharlieBotConfig | None = None,
) -> dict[str, list[artifact_check.AssertionOutcome]]:
  return _by_name(run_assertions(genre, artifact, cfg))


def _chrome_cfg(tmp_path: Path) -> SimpleNamespace:
  return SimpleNamespace(headless_chrome_bin=write_stub_chrome(tmp_path, 800))


def _sitrep_ok_doc() -> str:
  return _genre_doc(
      "sitrep", '<section><h2><span class="n">1</span> What waits on you?</h2><p>Nothing.</p></section>'
      '<section><h2><span class="n">2</span> What is this and why?</h2>'
      '<p>Why. <span class="req">r1</span></p></section>'
      '<section><h2><span class="n">3</span> What was verified?</h2><p>Done. <span class="src">s</span></p></section>'
      '<section><h2><span class="n">4</span> Risks</h2>'
      '<p><span class="tag fact">Fact</span> The reading holds. <span class="src">s</span></p></section>'
      '<section><h2><span class="n">5</span> What happens next?</h2><p>Next.</p></section>')


# ---------------------------------------------------------------------------
# style-verbatim
# ---------------------------------------------------------------------------


def test_style_verbatim_fails_on_tampered_style(tmp_path: Path) -> None:
  doc = _sitrep_ok_doc().replace("</style>", "body{color:red}\n</style>", 1)
  outcomes = _run("sitrep", _write(tmp_path, doc))
  (outcome,) = outcomes["style-verbatim"]
  assert not outcome.passed
  assert "differs from the genre template prompts/sitrep_template.html" in outcome.detail


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
  outcomes = run_assertions(genre, ROOT / "prompts" / template, cfg)
  assert [o for o in outcomes if not o.passed] == []


# ---------------------------------------------------------------------------
# CLI: charliebot artifact check
# ---------------------------------------------------------------------------


def _invoke(argv_tail: list[str]) -> SystemExit:
  with patch("sys.argv", ["artifact", "check", *argv_tail]), pytest.raises(SystemExit) as exc_info:
    artifact_main()
  return exc_info.value


def _run_cli(argv_tail: list[str]) -> int:
  exit_ = _invoke(argv_tail)
  assert isinstance(exit_.code, int)
  return exit_.code


def _probe_cfg(tmp_path: Path) -> tuple[SimpleNamespace, dict[str, SimpleNamespace]]:
  options = {name: SimpleNamespace(id=name) for name in ("alpha", "beta")}
  cfg = SimpleNamespace(
      headless_chrome_bin=write_stub_chrome(tmp_path, 800),
      backends=SimpleNamespace(preference=["alpha", "beta"]),
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
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda option, cfg: backends[option.id])


def test_cli_probe_runs_after_assertions_pass_and_prints_backend_and_answers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  backends = {
      "alpha": _FakeBackend(error="boom"),
      "beta": _FakeBackend(answer="(1) The reader's problem.\n(2)-(6) fine.")
  }
  _patch_backends(monkeypatch, backends)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
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
  assert "(7) Read as an engineer who knows the domain" in prompt
  assert "<trigger message verbatim>" not in prompt
  assert backends["beta"].calls[0]["timeout"] == artifact_check.ARTIFACT_PROBE_TIMEOUT == 300.0


def test_cli_unknown_genre_is_usage_error() -> None:
  assert _run_cli(["page.html", "--genre", "weird"]) == 2


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


# ---------------------------------------------------------------------------
# byte-integrity
# ---------------------------------------------------------------------------

_DAMAGED_BYTES = [(b"\t", "0x09"), (b"\x0c", "0x0c"), (b"\r", "0x0d")]


@pytest.mark.parametrize("damaged_byte,hex_name", _DAMAGED_BYTES)
def test_byte_integrity_fails_on_non_lf_control_byte(tmp_path: Path, damaged_byte: bytes, hex_name: str) -> None:
  """TAB / formfeed / CR in the raw source bytes fail the gate, each offset and value named."""
  doc = _sitrep_ok_doc().replace("Nothing.", f"Before{damaged_byte.decode('latin-1')}After")
  artifact = tmp_path / "damaged.html"
  artifact.write_bytes(doc.encode("utf-8"))
  (outcome,) = _run("sitrep", artifact)["byte-integrity"]
  assert not outcome.passed
  assert hex_name in outcome.detail
  assert "at offset" in outcome.detail

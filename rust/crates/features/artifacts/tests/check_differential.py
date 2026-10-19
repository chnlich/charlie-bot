import copy
import json
import pathlib
import re
import subprocess
import sys
import tempfile
import types

from src.features.artifacts import artifact_check

ROOT = pathlib.Path(__file__).resolve().parents[5]


def plan_page(goal: str = "Ship the fix.", extra: str = "") -> str:
  template = (ROOT / "prompts/plan_template.html").read_text(encoding="utf-8")
  style = re.search(r"<style>.*?</style>", template, re.DOTALL).group(0)
  titles = ["Problem / Goal", "Context", "High Level Solution", "Detailed Design", "Trade-offs", "Other Details"]
  sections = "".join(
      f'<section class="plan-section"><h2><span class="n">{index}</span> {title}</h2>'
      f"<p>{goal if index == 1 else title}</p></section>" for index, title in enumerate(titles, 1))
  return f"<html><head>{style}</head><body>{sections}{extra}<div class=\"foot\"><p>How to respond.</p></div></body></html>"


def write_page(directory: pathlib.Path, name: str, source: str) -> pathlib.Path:
  path = directory / name
  path.write_bytes(source.encode("utf-8"))
  return path


def without_style(source: str) -> str:
  return re.sub(r"<style>.*?</style>", "", source, count=1, flags=re.DOTALL)


def write_failing_chrome(path: pathlib.Path) -> str:
  path.write_text("#!/bin/sh\necho 'synthetic renderer failure' >&2\nexit 1\n", encoding="utf-8")
  path.chmod(0o755)
  return str(path)


def outcomes_for_python(page: pathlib.Path, chrome: str) -> list[dict[str, object]]:
  config = types.SimpleNamespace(headless_chrome_bin=chrome)
  outcomes = artifact_check.run_assertions("plan", page, config)
  return [{"name": outcome.name, "passed": outcome.passed, "detail": outcome.detail} for outcome in outcomes]


def outcomes_for_rust(fixture: pathlib.Path, page: pathlib.Path, chrome: str) -> list[dict[str, object]]:
  process = subprocess.run([str(fixture), str(page), chrome], capture_output=True, check=False, text=True)
  if process.returncode != 0:
    raise RuntimeError(f"fixture failed for {page.name}: {process.stderr}")
  return [json.loads(line) for line in process.stdout.splitlines()]


def real_page_with_many_lines() -> str:
  lines = "".join(f"<p>line {index}</p>" for index in range(200))
  return plan_page(extra=f'<div class="renderer-lines">{lines}</div>')


def test_warm_renderer_measures_real_page_height(
    expected: list[dict[str, object]], actual: list[dict[str, object]]) -> None:
  expected_height = next(item for item in expected if item["name"] == "page-height")
  actual_height = next(item for item in actual if item["name"] == "page-height")
  if not expected_height["passed"] or expected_height != actual_height:
    raise AssertionError(f"real Chrome height differs: expected={expected_height!r}, actual={actual_height!r}")
  if not re.match(r"\d+ px(?: \(target 2000\)|: )", str(expected_height["detail"])):
    raise AssertionError(f"real Chrome returned no positive page height: {expected_height!r}")


def assert_probe_cleanup(page: pathlib.Path) -> None:
  probes = list(page.parent.glob(".page-height-probe-*.html"))
  if probes:
    raise AssertionError(f"page-height probes remain beside {page.name}: {probes}")


def difference_count(expected: list[dict[str, object]], actual: list[dict[str, object]]) -> int:
  return sum(left != right for left, right in zip(expected, actual)) + abs(len(expected) - len(actual))


def main() -> int:
  if len(sys.argv) != 3:
    raise SystemExit("usage: check_differential.py FIXTURE_BIN CHROME_BIN")
  fixture = pathlib.Path(sys.argv[1]).resolve()
  chrome = str(pathlib.Path(sys.argv[2]).resolve())
  data = ROOT / "tests/data"
  with tempfile.TemporaryDirectory(prefix="artifact-differential-") as temporary:
    scratch = pathlib.Path(temporary)
    missing_chrome = str(scratch / "missing-chrome")
    failing_chrome = write_failing_chrome(scratch / "chrome-fails.sh")
    base = plan_page()
    cases: list[tuple[str, pathlib.Path, str]] = [
        ("plan_move2-direct-kill_v10", data / "plan_move2-direct-kill_v10.html", chrome),
        ("plan_move2-direct-kill_v11", data / "plan_move2-direct-kill_v11.html", chrome),
    ]
    synthetic = [
        ("template-pass", base, chrome),
        ("style-verbatim", base.replace("</style>", "body{outline:0}\n</style>", 1), chrome),
        ("sections-numbered", base.replace('<span class="n">6</span>', '<span class="n">9</span>', 1), chrome),
        ("foot-present", base.replace('<div class="foot"><p>How to respond.</p></div>', "", 1), chrome),
        (
            "fork-open-shape",
            plan_page(
                extra=
                '<section><h2>Context fork</h2><div class="fork"><details class="details-layer"><p>Reason.</p></details></div></section>'
            ),
            chrome,
        ),
        (
            "fork-explainer",
            plan_page(
                extra=
                '<section><h2>Context fork</h2><div class="fork"><p class="q"><span class="fn">Q1</span> Question?</p>'
                '<p class="rec">Use option A.</p><p class="trade">Option B is slower.</p>'
                '<details class="details-layer"><summary>Why?</summary></details></div></section>'),
            chrome,
        ),
        ("goal-budget", plan_page("x" * 241), chrome),
        ("page-height", base, missing_chrome),
        ("warm-renderer-real-page-height", real_page_with_many_lines(), chrome),
        ("ordinal-named", plan_page(extra="<p>Plan 3 has no identifying content name.</p>"), chrome),
        ("byte-integrity", plan_page().replace("Ship the fix.", "Ship\tthe fix."), chrome),
        ("style-count", without_style(base), chrome),
        (
            "span-n-repeated",
            base.replace('<span class="n">1</span>', '<span class="n">1</span><span class="n">2</span>', 1), chrome),
        ("span-n-not-integer", base.replace('<span class="n">1</span>', '<span class="n">A</span>', 1), chrome),
        ("span-n-empty", base.replace('<span class="n">1</span>', '<span class="n"></span>', 1), chrome),
        (
            "sections-six-exactly",
            base.replace(
                '<div class="foot">',
                '<section class="plan-section"><h2><span class="n">7</span> Extra</h2><p>Extra</p></section>'
                '<div class="foot">', 1), chrome),
        (
            "fork-explainer-no-layer",
            plan_page(
                extra=
                '<section><h2>Context fork</h2><div class="fork"><p class="q"><span class="fn">Q1</span> Question?</p>'
                '<p class="rec">Use option A.</p><p class="trade">Option B is slower.</p></div></section>'),
            chrome,
        ),
        ("goal-no-section", base.replace("Problem / Goal</h2>", "Goals</h2>", 1), chrome),
        ("page-height-required", base, ""),
        ("page-height-launch-failed", base, failing_chrome),
        ("ordinal-gloss-after-list-marker", plan_page(extra="<p>2. Plan 3: ship the fix.</p>"), chrome),
    ]
    for name, source, chrome_for_page in synthetic:
      cases.append((name, write_page(scratch, f"{name}.html", source), chrome_for_page))

    differences = 0
    for name, page, chrome_for_page in cases:
      expected = outcomes_for_python(page, chrome_for_page)
      assert_probe_cleanup(page)
      actual = outcomes_for_rust(fixture, page, chrome_for_page)
      assert_probe_cleanup(page)
      if name == "warm-renderer-real-page-height":
        test_warm_renderer_measures_real_page_height(expected, actual)
      count = difference_count(expected, actual)
      differences += count
      if count:
        print(f"DIFFERENCE {name}: expected={expected!r} actual={actual!r}", file=sys.stderr)

    print(f"DIFFERENTIAL: {differences} differences across {len(cases)} pages")
    control_page = next(page for name, page, _ in cases if name == "template-pass")
    control_chrome = chrome
    expected = outcomes_for_python(control_page, control_chrome)
    assert_probe_cleanup(control_page)
    control = copy.deepcopy(outcomes_for_rust(fixture, control_page, control_chrome))
    assert_probe_cleanup(control_page)
    control[0]["detail"] = "intentional control change"
    control_differences = difference_count(expected, control)
    print(
        f"CONTROL: {control_differences} difference" if control_differences ==
        1 else f"CONTROL: {control_differences} differences")
    return int(differences != 0 or control_differences != 1)


if __name__ == "__main__":
  raise SystemExit(main())

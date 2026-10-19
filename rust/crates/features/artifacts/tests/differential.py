"""Compare Python and Rust plan annotations over seeded edits to artifact genre templates."""

import argparse
import json
import random
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))

from src.features.artifacts import plan_diff

TEMPLATES = (
    "plan_template.html",
    "debug_template.html",
    "explain_template.html",
    "sitrep_template.html",
    "memory_report_template.html",
)
PAIR_COUNT = 200
SEED = 20261018


def _genre_template(name: str) -> str:
  path = ROOT / "prompts" / name
  assert path.is_file(), f"missing artifact genre template: {path}"
  assert Path(plan_diff.__file__).resolve().is_relative_to(ROOT), "Python differ imported outside this worktree"
  return path.read_text(encoding="utf-8")


def _insert_fixture(page: str, section: str) -> str:
  for closing_tag in ("</main>", "</body>"):
    index = page.rfind(closing_tag)
    if index >= 0:
      return page[:index] + section + page[index:]
  return page + section


def _section(index: int, token: str) -> str:
  return (
      f'<section class="plan-section" data-seed="{index}">'
      f'<h2>Materials {index}</h2>'
      f'<p class="summary">The system stores amber units for review.</p>'
      f'<p class="cjk">基準 計画 精度 {token}</p>'
      f'<p class="entity">Keep fish &amp; source near the <br><wbr>inspection point.</p>'
      f'<img alt="synthetic image" src="/synthetic.png"><input value="ready"><hr>'
      '<area><base><col><embed><link><meta><param><source><track>'
      f'<table><tbody><tr><td>status</td><td>prior {index}</td></tr>'
      f'<tr><td>owner</td><td>reviewer</td></tr></tbody></table>'
      f'<details class="details-layer"><summary>Prior version</summary><p>Retain the audit note.</p></details>'
      "</section>\n")


def _edited_section(section: str, index: int) -> str:
  rng = random.Random(SEED + index)
  mode = index % 8
  inserted = rng.choice(("reserved", "tracked", "limited", "verified"))
  if mode == 0:
    return section.replace("amber units", f"amber {inserted} units", 1)
  if mode == 1:
    return section.replace("amber units", "units", 1)
  if mode == 2:
    return section.replace("amber", rng.choice(("cobalt", "violet", "silver")), 1)
  if mode == 3:
    return section.replace("基準", "更新", 1)
  if mode == 4:
    return section.replace(f"prior {index}", f"current {index}", 1).replace(
        '<tr><td>owner</td><td>reviewer</td></tr>',
        f'<tr><td>owner</td><td>reviewer {inserted}</td></tr>',
        1,
    )
  if mode == 5:
    return section.replace("Prior version", "Revised version",
                           1).replace('<p>Retain the audit note.</p>', '<p>Retain the updated audit note.</p>', 1)
  if mode == 6:
    return section.replace('class="summary"', 'class="summary revised"', 1)
  return section.replace("&amp;", "&lt;",
                         1).replace("<br><wbr>", "<br><wbr><img alt=\"marker\" src=\"/marker.png\">", 1)


def _pairs() -> list[tuple[str, str]]:
  templates = [_genre_template(name) for name in TEMPLATES]
  pairs = []
  for index in range(PAIR_COUNT):
    template = templates[index % len(templates)]
    section = _section(index, f"seed-{random.Random(SEED + index).randrange(1_000_000)}")
    base = _insert_fixture(template, section)
    new = _insert_fixture(template, _edited_section(section, index))
    pairs.append((base, new))
  return pairs


def _write_pairs(directory: Path, pairs: list[tuple[str, str]]) -> None:
  directory.mkdir()
  for index, (base, new) in enumerate(pairs):
    path = directory / f"pair-{index:03}.json"
    path.write_text(json.dumps({"base": base, "new": new}, ensure_ascii=False), encoding="utf-8")


def _python_outputs(directory: Path, pairs: list[tuple[str, str]]) -> None:
  directory.mkdir()
  for index, (base, new) in enumerate(pairs):
    output = plan_diff.annotate(base, new)
    (directory / f"pair-{index:03}.html").write_text(output, encoding="utf-8")


def _difference_count(left: Path, right: Path) -> int:
  left_files = {path.name for path in left.iterdir() if path.is_file()}
  right_files = {path.name for path in right.iterdir() if path.is_file()}
  if left_files != right_files:
    raise AssertionError(f"output filenames differ: {left_files ^ right_files}")
  return sum((left / name).read_bytes() != (right / name).read_bytes() for name in sorted(left_files))


def main() -> None:
  parser = argparse.ArgumentParser()
  parser.add_argument(
      "--runner",
      type=Path,
      default=ROOT / "rust/target/release/artifact-plan-diff",
      help="release binary built by cargo-slot.sh build --release",
  )
  args = parser.parse_args()
  if not args.runner.is_file():
    raise FileNotFoundError(f"differential runner is missing: {args.runner}")
  pairs = _pairs()
  scratch = Path.home() / "scripts" / "20261018_artifacts_view_differential"
  scratch.mkdir(parents=True, exist_ok=True)
  with tempfile.TemporaryDirectory(prefix="run-", dir=scratch) as temporary:
    root = Path(temporary)
    inputs = root / "inputs"
    python_output = root / "python-output"
    rust_output = root / "rust-output"
    _write_pairs(inputs, pairs)
    _python_outputs(python_output, pairs)
    rust_output.mkdir()
    subprocess.run([str(args.runner), str(inputs), str(rust_output)], cwd=ROOT, check=True)
    differences = _difference_count(python_output, rust_output)
    if differences:
      first_name = next(
          name for name in sorted(path.name for path in python_output.iterdir())
          if (python_output / name).read_bytes() != (rust_output / name).read_bytes()
      )
      expected = (python_output / first_name).read_bytes()
      actual = (rust_output / first_name).read_bytes()
      first_offset = next(index for index, pair in enumerate(zip(expected, actual)) if pair[0] != pair[1])
      print(f"first mismatch: {first_name} byte={first_offset}")
      print(f"Python: {expected[max(0, first_offset - 120):first_offset + 200]!r}")
      print(f"Rust:   {actual[max(0, first_offset - 120):first_offset + 200]!r}")
      raise AssertionError(f"Python and Rust differ on {differences} of {PAIR_COUNT} page pairs")

    control_output = root / "control-rust-output"
    shutil.copytree(rust_output, control_output)
    with (control_output / "pair-000.html").open("ab") as output:
      output.write(b"planted differential control")
    control_differences = _difference_count(python_output, control_output)
    if control_differences != 1:
      raise AssertionError(f"the planted control difference produced {control_differences} mismatches")

  print(f"pairs={PAIR_COUNT} python_rust_differences={differences} control_planted_differences={control_differences}")


if __name__ == "__main__":
  main()

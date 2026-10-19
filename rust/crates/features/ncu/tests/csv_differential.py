"""Compare the native CSV parser with Python's `_parse_with_csv` over the canned `ncu` outputs.

Each case in `canned/cases.json` is parsed twice. The Python side calls `ncu_parsing._parse_with_csv` with its `ncu`
run replaced by the canned outcome. The Rust side runs the `csv_probe` example, which feeds the same bytes to
`charliebot_ncu::csv_import::parse`. Results are compared as canonical JSON, so an integer and a float differ.

Run from the repo root after `cargo-slot.sh build --release --example csv_probe` in `rust/`:

  uv run python rust/crates/features/ncu/tests/csv_differential.py [--plant]

`--plant` changes one value on the Python side first. That control run must report exactly one difference.
"""

import argparse
import collections
import json
import pathlib
import subprocess
import sys
from unittest import mock

TESTS = pathlib.Path(__file__).resolve().parent
CANNED = TESTS / "canned"
REPO = TESTS.parents[4]
PROBE = REPO / "rust/target/release/examples/csv_probe"

sys.path.insert(0, str(REPO))
from src.features.ncu import ncu_parsing  # noqa: E402


def _read(path: pathlib.Path) -> str:
  """The file's text with no newline translation, the same bytes the probe reads."""
  with open(path, encoding="utf-8", newline="") as handle:
    return handle.read()


def _canned_run(canned_case: dict):
  """A stand-in for `ncu_parsing._run_ncu_csv_import` that returns the case's canned outcome."""

  def run(abspath: str) -> subprocess.CompletedProcess[str]:
    match canned_case["outcome"]:
      case "completed":
        stdout = _read(CANNED / canned_case["stdout_file"])
        code = 0 if canned_case["success"] else 1
        return subprocess.CompletedProcess(["ncu"], code, stdout, canned_case["stderr"])
      case "not_found":
        raise FileNotFoundError(2, "No such file or directory", "ncu")
      case "timeout":
        raise subprocess.TimeoutExpired(["ncu", "--import", abspath], 120)
      case other:
        raise ValueError(f"unknown outcome {other!r} in {canned_case['name']}")

  return run


def _python_result(canned_case: dict) -> dict:
  """`_parse_with_csv`'s result for one canned case, or the error it raises."""
  abspath = str(CANNED / f"{canned_case['name']}.ncu-rep")
  with mock.patch.object(ncu_parsing, "_run_ncu_csv_import", _canned_run(canned_case)):
    try:
      report = ncu_parsing._parse_with_csv(abspath)
    except ncu_parsing.NcuParseError as exc:
      return {"status": "report_error", "message": str(exc)}
    except Exception as exc:  # Recorded and compared with the Rust error's kind, not swallowed.
      kind = f"{type(exc).__module__}.{type(exc).__qualname__}".removeprefix("builtins.")
      return {"status": "internal_error", "kind": kind, "message": str(exc)}
  return {"status": "ok", "report": report}


def _probe_results() -> dict:
  """The `csv_probe` result for every case."""
  if not PROBE.exists():
    sys.exit("csv_probe is not built: run `cargo-slot.sh build --release --example csv_probe` in rust/")
  command = [str(PROBE), str(CANNED / "cases.json")]
  proc = subprocess.run(command, capture_output=True, encoding="utf-8", check=False, cwd=REPO)
  if proc.returncode != 0:
    sys.exit(f"csv_probe exited {proc.returncode}:\n{proc.stderr}")
  return json.loads(proc.stdout)


def _canonical(value: object) -> str:
  return json.dumps(value, sort_keys=True)


def _short(value: object) -> str:
  text = repr(value)
  return text if len(text) <= 160 else text[:157] + "..."


def _first_difference(left: object, right: object, path: str) -> str | None:
  """Where two results first differ, as a path and both values, or None when they are equal."""
  if isinstance(left, dict) and isinstance(right, dict):
    for key in sorted(set(left) | set(right)):
      if key not in left or key not in right:
        return f"{path}.{key}: missing on the {'rust' if key in left else 'python'} side"
      nested = _first_difference(left[key], right[key], f"{path}.{key}")
      if nested is not None:
        return nested
    return None
  if isinstance(left, list) and isinstance(right, list):
    if len(left) != len(right):
      return f"{path}: python has {len(left)} items, rust has {len(right)}"
    for index, (left_item, right_item) in enumerate(zip(left, right)):
      nested = _first_difference(left_item, right_item, f"{path}[{index}]")
      if nested is not None:
        return nested
    return None
  if type(left) is not type(right) or left != right:
    return f"{path}: python {_short(left)}, rust {_short(right)}"
  return None


def _summary(results: dict) -> str:
  statuses = collections.Counter(result["status"] for result in results.values())
  counts = ", ".join(f"{status} {count}" for status, count in sorted(statuses.items()))
  return f"{len(results)} cases ({counts})"


def main() -> int:
  parser = argparse.ArgumentParser(description="Compare the native CSV parser with Python's _parse_with_csv.")
  parser.add_argument(
      "--plant",
      action="store_true",
      help="change one Python value first; the run must then report exactly one difference",
  )
  args = parser.parse_args()

  canned_cases = json.loads(_read(CANNED / "cases.json"))["cases"]
  python = {case["name"]: _python_result(case) for case in canned_cases}
  if args.plant:
    python["two_kernels"]["report"]["kernels"][0]["name"] += " (planted)"
  rust = _probe_results()

  print(f"command: {PROBE.relative_to(REPO)} {(CANNED / 'cases.json').relative_to(REPO)}")
  print(f"python: {_summary(python)}")
  print(f"rust:   {_summary(rust)}")
  differences = []
  for name in sorted(set(python) | set(rust)):
    if name not in python or name not in rust:
      differences.append(f"{name}: missing on the {'rust' if name in python else 'python'} side")
    elif _canonical(python[name]) != _canonical(rust[name]):
      differences.append(f"{name}: {_first_difference(python[name], rust[name], 'result')}")
  for difference in differences:
    print(f"DIFF {difference}")
  print(f"differences: {len(differences)}")
  expected = 1 if args.plant else 0
  return 0 if len(differences) == expected else 1


if __name__ == "__main__":
  sys.exit(main())

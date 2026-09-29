#!/usr/bin/env python3
"""The perf sweep's block runner: executes docs/perf_baseline.md's collectors verbatim.

The doc is the single home of the collector commands; this script adds only the
execution contract every round re-implemented ad hoc (three consecutive rounds
left their own extraction harnesses in /tmp): units run in doc order, one block's
``export K=v`` stdout lines feed the same unit's later blocks (the builder→consumer
pairs), each block is bounded at 600 s, and every exported scratch path still on
disk when the run ends is removed — whatever the exit path. Callers rely on: exit
status 0 only when every executed block exited 0, the per-block ``rc=`` lines in
the output, and the preflight stopping the sweep on failure (the doc's own rule).
"""

from __future__ import annotations

import argparse
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DOC = ROOT / "docs" / "perf_baseline.md"

# A unit heading names its metric: `M<digits>`, optional lowercase slug words,
# then ` — `. The looser `M<digits>`-then-dash shape also matches wrapped prose
# lines (the M57/M70/M72 repairs-called-out continuation), so the slug words are
# load-bearing.
UNIT_HEADING = re.compile(r"^M\d+(?: [a-z][a-z0-9-]*)* — ")
BLOCK_TIMEOUT_S = 600


def parse_collectors(doc: Path) -> tuple[list[str], list[tuple[str, list[str]]]]:
  """Split the doc's collector section into (preamble blocks, units).

  The preamble holds the fenced blocks before the first unit heading (the
  checkout preflight); a unit is one heading plus every fenced ```bash block
  under it, in doc order. A unit with no blocks is a retired collector's
  standing note and runs nothing.
  """
  lines = doc.read_text(encoding="utf-8").splitlines()
  start = next(i for i, line in enumerate(lines) if line == "## Collector commands")
  end = next(i for i, line in enumerate(lines) if line == "## Sampling history")
  preamble: list[str] = []
  units: list[tuple[str, list[str]]] = []
  current: list[str] = preamble  # where the open fence's body lands: preamble until the first heading
  in_fence = False
  body: list[str] = []
  for line in lines[start:end]:
    stripped = line.strip()
    if in_fence:
      if stripped == "```":
        current.append("\n".join(body))
        in_fence = False
      else:
        body.append(line)
    elif stripped == "```bash":
      in_fence = True
      body = []
    elif UNIT_HEADING.match(line):
      current = []
      units.append((line.split(" — ")[0].strip(), current))
  if in_fence:
    raise ValueError(f"{doc}: unterminated ```bash block in the collector section")
  return preamble, units


class SweepRunner:
  """Executes one doc's collector units in order; the instance carries the run's state.

  Blocks run with the checkout as cwd (the ``$PWD``-defaulting blocks read the
  checkout under test) and the invoking environment passed through, so a
  caller's ``CHECKOUT`` reaches the blocks unchanged. A unit's builder block
  exports its consumer's inputs on stdout; the exported absolute paths are the
  builder's scratch homes, which live only for their consumer — the exit-path
  sweep removes any that survive.
  """

  def __init__(self, doc: Path, from_unit: str | None) -> None:
    self._doc = doc
    self._from_unit = from_unit
    self._scratch: list[str] = []
    self._failed: list[str] = []

  def run(self) -> int:
    preamble, units = parse_collectors(self._doc)
    blocks_total = len(preamble) + sum(len(blocks) for _, blocks in units)
    print(f"perf-sweep: {blocks_total} blocks over {len(units)} units from {self._doc}", flush=True)
    status = 0
    try:
      for index, body in enumerate(preamble):
        rc, _, _ = self._run_block("preflight", index, len(preamble), body, dict(os.environ))
        if rc != 0:
          print("perf-sweep: preflight failed — the sweep stops, the round reports its metrics unmeasured", flush=True)
          return 1
      start = self._start_index(units)
      for index, (label, blocks) in enumerate(units):
        if index < start:
          continue
        if not blocks:
          print(f"=== {label}: no blocks (retired collector's note), skipped", flush=True)
          continue
        if not self._run_unit(label, blocks):
          continue
        status = 1
    finally:
      self._sweep_scratch()
      if self._failed:
        print(f"perf-sweep: FAILED units: {', '.join(self._failed)}", flush=True)
    return status

  def _run_unit(self, label: str, blocks: list[str]) -> bool:
    """Run one unit's blocks in order; the builder's exports feed the consumer's env."""
    unit_env = dict(os.environ)
    failed = False
    for index, body in enumerate(blocks):
      rc, _, _ = self._run_block(label, index, len(blocks), body, unit_env)
      if rc != 0 and label not in self._failed:
        self._failed.append(label)
        failed = True
    return failed

  def _run_block(self, label: str, index: int, total: int, body: str, env: dict[str, str]) -> tuple[int, float, str]:
    print(f"=== {label} block {index + 1}/{total}", flush=True)
    started = time.monotonic()
    try:
      proc = subprocess.run(
          ["bash", "-c", body], cwd=ROOT, env=env, capture_output=True, text=True, timeout=BLOCK_TIMEOUT_S, check=False)
      out = proc.stdout + proc.stderr
      rc = proc.returncode
    except subprocess.TimeoutExpired as exc:  # the bound is the collector's; the finding is the round's
      out = _as_text(exc.stdout) + _as_text(exc.stderr)
      rc = 124
    seconds = time.monotonic() - started
    if out.strip():
      print(out, flush=True)
    print(f"--- {label} block {index + 1}/{total} rc={rc} {seconds:.1f}s", flush=True)
    self._absorb_exports(out, env)
    return rc, seconds, out

  def _absorb_exports(self, out: str, env: dict[str, str]) -> None:
    """Apply the block's ``export K=v`` stdout lines to the unit env; record exported paths."""
    for line in out.splitlines():
      if not line.startswith("export "):
        continue
      for token in shlex.split(line[len("export "):]):
        key, sep, value = token.partition("=")
        if not sep or not key:
          raise ValueError(f"unparseable export line: {line!r}")
        env[key] = value
        if value.startswith("/"):
          self._scratch.append(value)

  def _sweep_scratch(self) -> None:
    """Remove every exported scratch path still on disk, loudly; a removal failure must not pass."""
    problems: list[str] = []
    for value in dict.fromkeys(self._scratch):
      path = Path(value)
      if not path.exists():
        continue
      try:
        shutil.rmtree(path)
        print(f"perf-sweep: swept leftover scratch {value}", flush=True)
      except OSError as exc:
        problems.append(f"{value}: {exc}")
    if problems:
      raise RuntimeError(f"scratch sweep failed: {'; '.join(problems)}")

  def _start_index(self, units: list[tuple[str, list[str]]]) -> int:
    if self._from_unit is None:
      return 0
    if self._from_unit.isdigit():
      index = int(self._from_unit)
      if index >= len(units):
        raise SystemExit(f"--from {index}: {self._doc} carries {len(units)} units")
      return index
    for index, (label, _) in enumerate(units):
      if label.startswith(self._from_unit):
        return index
    raise SystemExit(f"--from {self._from_unit}: no unit label starts with it")


def _as_text(data: object) -> str:
  if data is None:
    return ""
  return data.decode(errors="replace") if isinstance(data, bytes) else str(data)


def main() -> int:
  parser = argparse.ArgumentParser(description="Run docs/perf_baseline.md's collector units verbatim, in doc order.")
  parser.add_argument("--doc", type=Path, default=DEFAULT_DOC, help="collector doc (default: this repo's)")
  parser.add_argument(
      "--from",
      dest="from_unit",
      default=None,
      help="unit index or label prefix to start from (a killed sweep's tail re-run)")
  args = parser.parse_args()
  return SweepRunner(args.doc, args.from_unit).run()


if __name__ == "__main__":
  sys.exit(main())

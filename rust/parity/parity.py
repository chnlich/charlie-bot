"""Parity harness: replay scripted requests against the Python server and a candidate, and report every difference.

Usage (from the worktree root):

  uv run python rust/parity/parity.py --python-vs-python [--stage NAME ...]
  uv run python rust/parity/parity.py --rust-bin rust/target/release/<server-binary> [--stage NAME ...]

For each scenario the harness builds two identical synthetic homes with fixture.py, starts the baseline
(always the Python server) and the candidate on them as real processes, and sends every step to both. After
each step it compares the normalized responses and the files each step added, removed or changed in each tree.
It prints one line per step and a summary per route template, and exits 1 on any difference or any write
outside the trees. See README.md.
"""

import argparse
import collections
import concurrent.futures
import dataclasses
import http.client
import json
import os
import pathlib
import shutil
import sys
import tempfile
import urllib.parse

import compare
import guard
import normalize
import scenario_file
import servers

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
PARITY_DIR = pathlib.Path(__file__).resolve().parent
PYTHON = REPO_ROOT / ".venv" / "bin" / "python"
FIXTURE = PARITY_DIR / "fixture.py"
REPORT_PATH = PARITY_DIR / "out" / "last-report.txt"
REQUEST_TIMEOUT_SECONDS = 30.0
# Each side's tree directory. The names have one length, so the two tree roots do too: the homes embed their
# root in file content, and a file's size (and every ETag, memo revision or offset derived from it) matches
# only when the roots are equally long.
TREE_NAMES = {"baseline": "base", "candidate": "cand"}
# The rename --tamper applies to the candidate home: a deliberate mismatch that shows the harness failing.
TAMPER_SESSION = "atlas"
TAMPER_NAME = "Atlas planning (tampered)"


@dataclasses.dataclass
class StepResult:
  scenario: str
  name: str
  route: str
  statuses: tuple[int | None, int | None]
  lines: list[str]
  files: str
  responses: list[compare.Response | None]

  @property
  def passed(self) -> bool:
    return not self.lines


class Side:
  """One server under comparison: its tree, process, normalization context and captures."""

  def __init__(self, label: str, argv: list[str], tree: servers.Tree, port: int, log_dir: pathlib.Path) -> None:
    self.label = label
    self.tree = tree
    self.server = servers.ServerProcess(label, argv, tree, port, REPO_ROOT, log_dir)
    self.ctx = normalize.Context(tree_root=str(tree.root))
    self.captures: dict[str, object] = {}


def _preflight(rust_bin: pathlib.Path | None) -> None:
  for path, what in ((PYTHON, "the worktree venv (run `uv run python --version` once)"),
                     (pathlib.Path("/usr/bin/strace"), "strace")):
    if not os.access(path, os.X_OK):
      raise SystemExit(f"parity: missing {what}: {path}")
  if rust_bin is not None and not os.access(rust_bin, os.X_OK):
    raise SystemExit(f"parity: --rust-bin is not an executable file: {rust_bin}")


def _build_fixtures(sides: list[Side], log_dir: pathlib.Path) -> dict:
  """Generate both homes in parallel and check they match; return the manifest."""

  def build(side: Side) -> None:
    servers.run_traced(
        [str(PYTHON), str(FIXTURE), "--manifest",
         str(side.tree.root / "fixture-manifest.json")],
        tree=side.tree,
        repo_root=REPO_ROOT,
        trace_log=log_dir / f"{side.tree.label}.fixture.strace",
        log_path=log_dir / f"{side.tree.label}.fixture.log")

  with concurrent.futures.ThreadPoolExecutor(len(sides)) as pool:
    for future in [pool.submit(build, side) for side in sides]:
      future.result()
  manifests = [json.loads((side.tree.root / "fixture-manifest.json").read_text(encoding="utf-8")) for side in sides]
  if manifests[0] != manifests[1]:
    raise RuntimeError("the two fixture manifests differ: the fixture generator is not deterministic")
  snapshots = [compare.snapshot(side.tree.root, servers.HARNESS_ENTRIES) for side in sides]
  drift = compare.tree_diff(snapshots[0], snapshots[1], sides[0].ctx, sides[1].ctx)
  if drift:
    raise RuntimeError(
        "the two fixture homes differ: the fixture generator is not deterministic:\n  " +
        "\n  ".join(compare.capped(drift)))
  return manifests[0]


def _tamper(side: Side, manifest: dict) -> None:
  """Rename one session in this side's home, behind the server's back; the file keeps its pinned times."""
  metadata_path = side.tree.home / "sessions" / manifest["sessions"][TAMPER_SESSION] / "metadata.json"
  times = metadata_path.stat()
  metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
  metadata["name"] = TAMPER_NAME
  metadata_path.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
  os.utime(metadata_path, ns=(times.st_atime_ns, times.st_mtime_ns))


def _request(side: Side, step: scenario_file.Step, manifest: dict) -> compare.Response:
  path = scenario_file.substitute(step.path, manifest, side.captures)
  query = scenario_file.substitute(step.query, manifest, side.captures)
  headers = scenario_file.substitute(step.headers, manifest, side.captures)
  if step.auth == "bearer":
    headers["Authorization"] = f"Bearer {servers.ACCESS_KEY}"
  elif step.auth == "cookie":
    headers["Cookie"] = f"charliebot_access_key={servers.ACCESS_KEY}"
  elif step.auth != "none":
    raise ValueError(f"unknown auth mode {step.auth!r}")
  body = None
  if step.has_json:
    body = json.dumps(scenario_file.substitute(step.json_body, manifest, side.captures)).encode()
    headers.setdefault("Content-Type", "application/json")
  url = path + ("?" + urllib.parse.urlencode(query) if query else "")
  connection = http.client.HTTPConnection(servers.HOST, side.server.port, timeout=REQUEST_TIMEOUT_SECONDS)
  try:
    connection.request(step.method, url, body=body, headers=headers)
    response = connection.getresponse()
    return compare.Response(response.status, response.getheaders(), response.read())
  finally:
    connection.close()


def _capture(side: Side, step: scenario_file.Step, response: compare.Response) -> list[str]:
  """Store this side's captures; a capture its response lacks is a difference."""
  lines = []
  for name, field in step.capture.items():
    try:
      side.captures[name] = scenario_file.extract(response.json(), field)
    except (scenario_file.ScenarioError, json.JSONDecodeError) as error:
      side.captures.pop(name, None)
      lines.append(f"capture {name} ({side.label}): {error}")
  return lines


def _run_step(
    scenario: scenario_file.Scenario, sides: list[Side], step: scenario_file.Step, manifest: dict,
    before: list[compare.Snapshot]) -> tuple[StepResult, list[compare.Snapshot]]:
  lines: list[str] = []
  responses: list[compare.Response | None] = []
  for side in sides:
    try:
      responses.append(_request(side, step, manifest))
    except scenario_file.ScenarioError as error:
      # A capture this side's earlier response lacked (check_references vouched for every manifest reference):
      # that difference is already reported.
      lines.append(f"request not sent to {side.label}: {error}")
      responses.append(None)
  baseline, candidate = responses
  for side, response in zip(sides, responses, strict=True):
    if response is not None and step.expect_status is not None and response.status != step.expect_status:
      lines.append(f"{side.label} status {response.status}, scenario expects {step.expect_status}")
  if baseline is not None and candidate is not None:
    lines.extend(compare.response_diff(step.route, baseline, candidate, sides[0].ctx, sides[1].ctx))
  after = compare.settle_all([side.tree.root for side in sides], servers.HARNESS_ENTRIES)
  deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
  lines.extend(compare.delta_diff(deltas[0], deltas[1], sides[0].ctx, sides[1].ctx))
  for side, response in zip(sides, responses, strict=True):
    if response is not None:
      lines.extend(_capture(side, step, response))
  statuses = (baseline.status if baseline else None, candidate.status if candidate else None)
  files = compare.describe_delta(deltas[0])
  return StepResult(scenario.name, step.name, step.route, statuses, lines, files, responses), after


def _save_responses(log_dir: pathlib.Path, number: int, sides: list[Side], result: StepResult) -> None:
  """Keep both raw responses of a differing step next to the server logs."""
  for side, response in zip(sides, result.responses, strict=True):
    if response is not None:
      head = [f"HTTP {response.status}", *(f"{name}: {value}" for name, value in response.headers), "", ""]
      (log_dir /
       f"step{number:03d}.{side.label}.response").write_bytes("\n".join(head).encode() + response.decoded_body())


def _run_scenario(
    scenario: scenario_file.Scenario, work_dir: pathlib.Path, candidate_argv: list[str], tamper: bool,
    guard_report: guard.Report) -> list[StepResult]:
  log_dir = work_dir / "logs"
  log_dir.mkdir(parents=True)
  ports = servers.pick_ports(2)
  python_argv = [str(PYTHON), "server.py"]
  sides = []
  for label, argv, port in (("baseline", python_argv, ports[0]), ("candidate", candidate_argv, ports[1])):
    tree = servers.make_tree(TREE_NAMES[label], work_dir / TREE_NAMES[label])
    servers.write_home_config(tree, port)
    sides.append(Side(label, argv, tree, port, log_dir))
  if len(str(sides[0].tree.root)) != len(str(sides[1].tree.root)):
    raise RuntimeError(f"tree roots differ in length: {sides[0].tree.root} {sides[1].tree.root}")
  manifest = _build_fixtures(sides, log_dir)
  scenario_file.check_references(scenario, manifest)
  if tamper:
    _tamper(sides[1], manifest)
  before = [compare.snapshot(side.tree.root, servers.HARNESS_ENTRIES) for side in sides]
  results: list[StepResult] = []
  try:
    for side in sides:
      side.server.start()
    for side in sides:
      side.server.wait_ready()
    after = compare.settle_all([side.tree.root for side in sides], servers.HARNESS_ENTRIES)
    deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
    results.append(
        StepResult(
            scenario.name, "server startup", "(startup)", (None, None),
            compare.delta_diff(deltas[0], deltas[1], sides[0].ctx, sides[1].ctx), compare.describe_delta(deltas[0]),
            []))
    before = after
    for number, step in enumerate(scenario.steps, start=1):
      result, before = _run_step(scenario, sides, step, manifest, before)
      results.append(result)
      if not result.passed:
        _save_responses(log_dir, number, sides, result)
    for side in sides:
      side.server.stop()
    after = [compare.snapshot(side.tree.root, servers.HARNESS_ENTRIES) for side in sides]
    deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
    results.append(
        StepResult(
            scenario.name, "server shutdown", "(shutdown)", (None, None),
            compare.delta_diff(deltas[0], deltas[1], sides[0].ctx, sides[1].ctx), compare.describe_delta(deltas[0]),
            []))
  finally:
    for side in sides:
      side.server.kill()
  for side in sides:
    for trace in (log_dir / f"{side.tree.label}.fixture.strace", side.server.trace_log):
      guard.scan(trace, (side.tree.root,), REPO_ROOT, guard_report)
  return results


def _format_step(result: StepResult) -> list[str]:
  statuses = "/".join("-" if status is None else str(status) for status in result.statuses)
  head = f"{'PASS' if result.passed else 'FAIL'}  {result.route:<58} {statuses:<8} {result.name}"
  if result.files:
    head += f"  [{result.files}]"
  return [head, *(f"      {line}" for line in result.lines)]


def _route_summary(results: list[StepResult]) -> list[str]:
  by_route: dict[str, list[StepResult]] = collections.defaultdict(list)
  for result in results:
    by_route[result.route].append(result)
  lines = []
  for route in sorted(by_route):
    group = by_route[route]
    passed = sum(result.passed for result in group)
    verdict = "ok  " if passed == len(group) else "DIFF"
    lines.append(f"{verdict}  {route:<58} {passed}/{len(group)} steps match")
  return lines


def main() -> int:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  mode = parser.add_mutually_exclusive_group(required=True)
  mode.add_argument("--python-vs-python", action="store_true", help="run the Python server as the candidate too")
  mode.add_argument("--rust-bin", type=pathlib.Path, help="the candidate server executable")
  parser.add_argument("--stage", action="append", help="run only scenarios of this stage (repeatable)")
  parser.add_argument(
      "--tamper",
      action="store_true",
      help=f"rename session {TAMPER_SESSION!r} in the candidate home: the run must fail")
  parser.add_argument("--keep", action="store_true", help="keep the temporary trees and logs after the run")
  args = parser.parse_args()
  rust_bin = args.rust_bin.resolve() if args.rust_bin else None
  _preflight(rust_bin)
  candidate_argv = [str(rust_bin)] if rust_bin else [str(PYTHON), "server.py"]
  scenarios = scenario_file.load_all(args.stage)
  # The servers run only on homes under /tmp, whatever TMPDIR says.
  scratch = pathlib.Path(tempfile.mkdtemp(prefix="charliebot-parity-", dir="/tmp"))
  guard_report = guard.Report()
  output: list[str] = []

  def emit(lines: list[str]) -> None:
    output.extend(lines)
    print("\n".join(lines), flush=True)

  results: list[StepResult] = []
  try:
    for index, scenario in enumerate(scenarios):
      emit([f"== [{scenario.stage}] {scenario.name} ({scenario.path.name})"])
      scenario_results = _run_scenario(
          scenario, scratch / f"{index:02d}-{scenario.name}", candidate_argv, args.tamper, guard_report)
      for result in scenario_results:
        lines = _format_step(result)
        output.extend(lines)
        print("\n".join([lines[0], *compare.capped(lines[1:])]), flush=True)
      results.extend(scenario_results)
  finally:
    failed = [result for result in results if not result.passed]
    clean = bool(results) and not failed and not guard_report.violations
    if args.keep or not clean:
      print(f"\ntrees and logs kept in {scratch}")
    else:
      shutil.rmtree(scratch)

  emit(["", "Routes:", *_route_summary(results)])
  emit(
      [
          "",
          (
              f"Outside-write guard: {len(guard_report.violations)} file changes outside the trees "
              f"({guard_report.traces} traces); {len(guard_report.refused)} writes refused by the sandbox"),
          *(f"  VIOLATION {change.describe()}" for change in guard_report.violations),
      ])
  if guard_report.refused:
    refused = collections.Counter(
        (change.syscall, os.path.dirname(change.path), change.outcome) for change in guard_report.refused)
    emit(
        [
            f"  refused {syscall} x{count} under {directory} ({errno})" for (syscall, directory,
                                                                             errno), count in sorted(refused.items())
        ])
  candidate = f"rust ({rust_bin})" if rust_bin else "python"
  verdict = "PASS" if not failed and not guard_report.violations else "FAIL"
  emit(
      [
          "",
          (
              f"{verdict}: {len(results) - len(failed)}/{len(results)} steps match; baseline python, "
              f"candidate {candidate}{'; candidate home tampered' if args.tamper else ''}"),
      ])
  REPORT_PATH.parent.mkdir(exist_ok=True)
  REPORT_PATH.write_text("\n".join(output) + "\n", encoding="utf-8")
  print(f"full report: {REPORT_PATH.relative_to(REPO_ROOT)}")
  return 0 if verdict == "PASS" else 1


if __name__ == "__main__":
  sys.exit(main())

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
import signal
import shutil
import sys
import tempfile
import time
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
  direction: str | None = None

  @property
  def passed(self) -> bool:
    return not self.lines


class Side:
  """One server under comparison: its tree, process, normalization context and captures."""

  def __init__(self, label: str, tree: servers.Tree, port: int) -> None:
    self.label = label
    self.tree = tree
    self.port = port
    self.server: servers.ServerProcess | None = None
    self.server_processes: list[servers.ServerProcess] = []
    self.ctx = normalize.Context(tree_root=str(tree.root))
    self.captures: dict[str, object] = {}

  def start_server(self, argv: list[str]) -> None:
    process_number = len(self.server_processes) + 1
    server = servers.ServerProcess(self.label, argv, self.tree, self.port, REPO_ROOT, process_number)
    server.start()
    self.server = server
    self.server_processes.append(server)

  def wait_ready(self) -> None:
    if self.server is None:
      raise RuntimeError(f"{self.label} has no server to wait for")
    self.server.wait_ready()


def _preflight(rust_bin: pathlib.Path | None) -> None:
  for path, what in ((PYTHON, "the worktree venv (run `uv run python --version` once)"),
                     (pathlib.Path("/usr/bin/strace"), "strace")):
    if not os.access(path, os.X_OK):
      raise SystemExit(f"parity: missing {what}: {path}")
  if rust_bin is not None and not os.access(rust_bin, os.X_OK):
    raise SystemExit(f"parity: --rust-bin is not an executable file: {rust_bin}")


def _build_fixtures(sides: list[Side]) -> dict:
  """Generate both homes in parallel and check they match; return the manifest."""

  def build(side: Side) -> None:
    servers.run_traced(
        [str(PYTHON), str(FIXTURE), "--manifest",
         str(side.tree.root / "fixture-manifest.json")],
        tree=side.tree,
        repo_root=REPO_ROOT,
        trace_log=side.tree.root / "logs" / f"{side.tree.label}.fixture.strace",
        log_path=side.tree.root / "logs" / f"{side.tree.label}.fixture.log")

  with concurrent.futures.ThreadPoolExecutor(len(sides)) as pool:
    for future in [pool.submit(build, side) for side in sides]:
      future.result()
  manifests = [json.loads((side.tree.root / "fixture-manifest.json").read_text(encoding="utf-8")) for side in sides]
  if manifests[0] != manifests[1]:
    raise RuntimeError("the two fixture manifests differ: the fixture generator is not deterministic")
  snapshots = [compare.snapshot(side.tree.root, servers.HARNESS_ENTRIES) for side in sides]
  known_uuids = normalize.fixture_uuids(
      [(path, entry.data) for path, entry in snapshots[0].items() if entry.kind == "file"])
  for side in sides:
    side.ctx.fixture_uuids = known_uuids
  drift = compare.tree_diff(snapshots[0], snapshots[1], sides[0].ctx, sides[1].ctx)
  if drift:
    raise RuntimeError(
        "the two fixture homes differ: the fixture generator is not deterministic:\n  " +
        "\n  ".join(compare.capped(drift)))
  return manifests[0]


def _spawns_path(tree: servers.Tree) -> pathlib.Path:
  return tree.root / "fake-agent" / "spawns.jsonl"


def _read_spawns(tree: servers.Tree) -> list[dict]:
  path = _spawns_path(tree)
  if not path.exists():
    return []
  return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _prepare_contexts(sides: list[Side], snapshots: list[compare.Snapshot]) -> None:
  for side, snapshot in zip(sides, snapshots, strict=True):
    extra_files = ()
    spawns_path = _spawns_path(side.tree)
    if spawns_path.exists():
      extra_files = (("fake-agent/spawns.jsonl", spawns_path.read_bytes()),)
    compare.prepare_context(snapshot, side.ctx, extra_files)


def _process_alive(record: dict) -> bool:
  identity = servers.process_identity(int(record["pid"]))
  return identity is not None and identity[1] == record["pid_start"] and identity[0] != "Z"


def _event_condition(side: Side, condition: dict, manifest: dict) -> bool:
  resolved = scenario_file.substitute(condition, manifest, side.captures)
  events_path = side.tree.home / "sessions" / resolved["session"] / "data" / "chat_events.jsonl"
  if not events_path.exists():
    return False
  raw_lines = events_path.read_bytes().splitlines(keepends=True)
  events = [json.loads(line) for line in raw_lines if line.endswith(b"\n") and line.strip()]
  expected_fields = resolved["fields"]
  matches = [
      event for event in events if event.get("type") == resolved["type"] and all(
          field in event and not compare.json_diff(event[field], expected)
          for field, expected in expected_fields.items())
  ]
  return len(matches) >= resolved["count"]


def _file_field_condition(side: Side, condition: dict, manifest: dict) -> bool:
  resolved = scenario_file.substitute(condition, manifest, side.captures)
  path = side.tree.home / resolved["path"]
  if os.path.commonpath((str(path.resolve()), str(side.tree.home.resolve()))) != str(side.tree.home.resolve()):
    raise RuntimeError(f"wait path escapes {side.tree.home}: {resolved['path']}")
  if not path.is_file():
    return False
  value = scenario_file.extract(json.loads(path.read_text(encoding="utf-8")), resolved["field"])
  return not compare.json_diff(value, resolved["equals"])


def _at_gate_condition(side: Side, name: str) -> bool:
  agent_dir = side.tree.root / "fake-agent"
  marker = agent_dir / f"at-{name}"
  if not marker.is_file():
    return False
  index = int(marker.read_text(encoding="utf-8"))
  record = next((spawn for spawn in _read_spawns(side.tree) if spawn["index"] == index), None)
  if record is None:
    raise RuntimeError(f"gate {name!r} names missing fake-agent start {index} in {side.label}")
  stdout_path = pathlib.Path(record["stdout_path"])
  cursor_path = stdout_path.with_name("agent.raw.cursor")
  if not stdout_path.is_file() or not cursor_path.is_file():
    return False
  try:
    cursor = int(cursor_path.read_text(encoding="utf-8").strip())
  except ValueError:
    return False
  return cursor == stdout_path.stat().st_size


def _condition_met(side: Side, until: dict, manifest: dict) -> bool:
  if "event" in until:
    return _event_condition(side, until["event"], manifest)
  if "file_field" in until:
    try:
      return _file_field_condition(side, until["file_field"], manifest)
    except (FileNotFoundError, scenario_file.ScenarioError):
      return False
  if "at_gate" in until:
    return _at_gate_condition(side, until["at_gate"])
  raise ValueError(f"unknown wait condition: {until}")


def _wait_for_sides(sides: list[Side], step: scenario_file.Step, manifest: dict) -> list[str]:
  if step.until is None or step.timeout_seconds is None:
    raise RuntimeError(f"wait step {step.name!r} has no validated condition or timeout")
  deadline = time.monotonic() + step.timeout_seconds
  pending = {side.label for side in sides}
  while pending:
    pending = {side.label for side in sides if not _condition_met(side, step.until, manifest)}
    if not pending:
      return []
    if time.monotonic() >= deadline:
      break
    time.sleep(0.02)
  return [f"{side} did not meet the wait condition within {step.timeout_seconds}s" for side in sorted(pending)]


def _write_fake_agent_script(sides: list[Side], script: dict | None) -> None:
  if script is None:
    return
  contents = json.dumps(script, ensure_ascii=False, indent=2) + "\n"
  for side in sides:
    (side.tree.root / "fake-agent" / "script.json").write_text(contents, encoding="utf-8")


def _tamper(side: Side, manifest: dict) -> None:
  """Rename one session in this side's home, behind the server's back; the file keeps its pinned times."""
  metadata_path = side.tree.home / "sessions" / manifest["sessions"][TAMPER_SESSION] / "metadata.json"
  times = metadata_path.stat()
  metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
  metadata["name"] = TAMPER_NAME
  metadata_path.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
  os.utime(metadata_path, ns=(times.st_atime_ns, times.st_mtime_ns))


def _request(side: Side, step: scenario_file.Step, manifest: dict) -> compare.Response:
  if side.server is None:
    raise RuntimeError(f"{side.label} has no running server")
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


def _check_agent_after_stop(side: Side, expected: str, starts: list[dict], pending_indices: set[int]) -> list[str]:
  lines = []
  for record in starts:
    if not record["scripted"] or record["index"] not in pending_indices:
      continue
    exit_exists = (side.tree.root / "fake-agent" / f"exit-{record['index']}").is_file()
    alive = _process_alive(record)
    satisfied = {"alive": alive, "exited": exit_exists, "terminated": not alive and not exit_exists}[expected]
    if not satisfied:
      lines.append(f"{side.label} fake-agent start {record['index']} did not meet {expected!r}: {record}")
  return lines


def _run_handover_step(
    scenario: scenario_file.Scenario,
    sides: list[Side],
    step: scenario_file.Step,
    manifest: dict,
    before: list[compare.Snapshot],
    handover_argv: list[list[str]],
) -> tuple[StepResult, list[compare.Snapshot]]:
  if step.agent_after_stop is None:
    raise RuntimeError(f"handover step {step.name!r} has no validated agent_after_stop value")
  starts_before_stop = [_read_spawns(side.tree) for side in sides]
  pending_before_stop = [
      {
          record["index"]
          for record in starts
          if record["scripted"] and not (side.tree.root / "fake-agent" / f"exit-{record['index']}").exists()
      }
      for side, starts in zip(sides, starts_before_stop, strict=True)
  ]
  for side in sides:
    if side.server is None:
      raise RuntimeError(f"{side.label} has no server to hand over")
    side.server.stop_server()
  lines = [
      line for side, starts, pending in zip(sides, starts_before_stop, pending_before_stop, strict=True)
      for line in _check_agent_after_stop(side, step.agent_after_stop, starts, pending)
  ]
  for side, argv in zip(sides, handover_argv, strict=True):
    side.start_server(argv)
  for side in sides:
    side.wait_ready()
  after = compare.settle_all([side.tree.root for side in sides], servers.HARNESS_ENTRIES)
  _prepare_contexts(sides, after)
  deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
  lines.extend(compare.delta_diff(deltas[0], deltas[1], sides[0].ctx, sides[1].ctx))
  result = StepResult(scenario.name, step.name, step.route, (None, None), lines, compare.describe_delta(deltas[0]), [])
  return result, after


def _run_step(
    scenario: scenario_file.Scenario, sides: list[Side], step: scenario_file.Step, manifest: dict,
    before: list[compare.Snapshot], handover_argv: list[list[str]]) -> tuple[StepResult, list[compare.Snapshot]]:
  if step.kind == "handover":
    return _run_handover_step(scenario, sides, step, manifest, before, handover_argv)
  lines: list[str] = []
  responses: list[compare.Response | None] = []
  baseline = candidate = None
  if step.kind == "http":
    for side in sides:
      try:
        responses.append(_request(side, step, manifest))
      except scenario_file.ScenarioError as error:
        lines.append(f"request not sent to {side.label}: {error}")
        responses.append(None)
    baseline, candidate = responses
    for side, response in zip(sides, responses, strict=True):
      if response is not None and step.expect_status is not None and response.status != step.expect_status:
        lines.append(f"{side.label} status {response.status}, scenario expects {step.expect_status}")
  elif step.kind == "wait":
    lines.extend(_wait_for_sides(sides, step, manifest))
  elif step.kind == "gate":
    if step.open is None:
      raise RuntimeError(f"gate step {step.name!r} has no validated open name")
    for side in sides:
      (side.tree.root / "fake-agent" / f"open-{step.open}").write_text("open\n", encoding="utf-8")
  else:
    raise ValueError(f"unknown scenario step kind {step.kind!r}")
  after = compare.settle_all([side.tree.root for side in sides], servers.HARNESS_ENTRIES)
  _prepare_contexts(sides, after)
  if baseline is not None and candidate is not None:
    lines.extend(compare.response_diff(step.route, baseline, candidate, sides[0].ctx, sides[1].ctx))
  deltas = None
  if not step.defer_files:
    deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
    lines.extend(compare.delta_diff(deltas[0], deltas[1], sides[0].ctx, sides[1].ctx))
  if step.kind == "http":
    for side, response in zip(sides, responses, strict=True):
      if response is not None:
        lines.extend(_capture(side, step, response))
  statuses = (baseline.status if baseline else None, candidate.status if candidate else None)
  files = compare.describe_delta(deltas[0]) if deltas is not None else ""
  next_before = before if step.defer_files else after
  return StepResult(scenario.name, step.name, step.route, statuses, lines, files, responses), next_before


def _save_responses(log_dir: pathlib.Path, number: int, sides: list[Side], result: StepResult) -> None:
  """Keep both raw responses of a differing step next to the server logs."""
  for side, response in zip(sides, result.responses):
    if response is not None:
      head = [f"HTTP {response.status}", *(f"{name}: {value}" for name, value in response.headers), "", ""]
      (log_dir /
       f"step{number:03d}.{side.label}.response").write_bytes("\n".join(head).encode() + response.decoded_body())


def _cleanup_scenario(sides: list[Side]) -> tuple[list[str], list[dict]]:
  lines = []
  for side in sides:
    if side.server is not None:
      try:
        side.server.stop_server()
      except RuntimeError as error:
        lines.append(f"{side.label} server stop: {error}")
  survivors = [record for side in sides for record in _read_spawns(side.tree) if _process_alive(record)]
  for record in survivors:
    identity = servers.process_identity(int(record["pid"]))
    if identity is not None and identity[1] == record["pid_start"] and identity[0] != "Z":
      try:
        os.kill(int(record["pid"]), signal.SIGKILL)
      except ProcessLookupError:
        pass
    lines.append(f"fake agent remained alive at scenario end and was killed: {record}")
  for side in sides:
    for server in side.server_processes:
      try:
        server.wait_trace()
      except RuntimeError as error:
        lines.append(f"{side.label} strace cleanup: {error}")
  return lines, survivors


def _spawns_diff(sides: list[Side], expect_starts: int | None) -> list[str]:
  records = [_read_spawns(side.tree) for side in sides]
  lines = []
  if expect_starts is not None:
    for side, starts in zip(sides, records, strict=True):
      if len(starts) != expect_starts:
        lines.append(f"{side.label} has {len(starts)} fake-agent starts; scenario expects {expect_starts}")
  normalized = [
      [
          normalize.normalize_json(record, normalize.file_rules("fake-agent/spawns.jsonl"), side.ctx)
          for record in starts
      ]
      for side, starts in zip(sides, records, strict=True)
  ]
  lines.extend(f"fake-agent spawns {line}" for line in compare.json_diff(normalized[0], normalized[1]))
  return lines


def _run_scenario(
    scenario: scenario_file.Scenario,
    work_dir: pathlib.Path,
    candidate_first_argv: list[str],
    candidate_handover_argv: list[str],
    direction: str | None,
    tamper: bool,
    guard_report: guard.Report,
) -> list[StepResult]:
  log_dir = work_dir / "logs"
  log_dir.mkdir(parents=True)
  ports = servers.pick_ports(2)
  python_argv = [str(PYTHON), "server.py"]
  sides = []
  for label, port in (("baseline", ports[0]), ("candidate", ports[1])):
    tree = servers.make_tree(TREE_NAMES[label], work_dir / TREE_NAMES[label])
    servers.write_home_config(tree, port)
    sides.append(Side(label, tree, port))
  if len(str(sides[0].tree.root)) != len(str(sides[1].tree.root)):
    raise RuntimeError(f"tree roots differ in length: {sides[0].tree.root} {sides[1].tree.root}")
  manifest = _build_fixtures(sides)
  scenario_file.check_references(scenario, manifest)
  _write_fake_agent_script(sides, scenario.fake_agent)
  if tamper:
    _tamper(sides[1], manifest)
  before = [compare.snapshot(side.tree.root, servers.HARNESS_ENTRIES) for side in sides]
  results: list[StepResult] = []
  try:
    sides[0].start_server(python_argv)
    sides[1].start_server(candidate_first_argv)
    for side in sides:
      side.wait_ready()
    after = compare.settle_all([side.tree.root for side in sides], servers.HARNESS_ENTRIES)
    _prepare_contexts(sides, after)
    deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
    results.append(
        StepResult(
            scenario.name, "server startup", "(startup)", (None, None),
            compare.delta_diff(deltas[0], deltas[1], sides[0].ctx, sides[1].ctx), compare.describe_delta(deltas[0]),
            []))
    before = after
    for number, step in enumerate(scenario.steps, start=1):
      handover_argv = [python_argv, candidate_handover_argv]
      result, before = _run_step(scenario, sides, step, manifest, before, handover_argv)
      results.append(result)
      if not result.passed:
        _save_responses(log_dir, number, sides, result)
  finally:
    cleanup_lines, _ = _cleanup_scenario(sides)
    after = [compare.snapshot(side.tree.root, servers.HARNESS_ENTRIES) for side in sides]
    _prepare_contexts(sides, after)
    deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
    cleanup_lines.extend(_spawns_diff(sides, scenario.expect_starts))
    results.append(
        StepResult(
            scenario.name, "server shutdown and fake-agent cleanup", "(shutdown)", (None, None),
            [*cleanup_lines, *compare.delta_diff(deltas[0], deltas[1], sides[0].ctx, sides[1].ctx)],
            compare.describe_delta(deltas[0]), []))
  for side in sides:
    trace_logs = [side.tree.root / "logs" / f"{side.tree.label}.fixture.strace"]
    trace_logs.extend(server.trace_log for server in side.server_processes)
    for trace in trace_logs:
      guard.scan(trace, (side.tree.root,), REPO_ROOT, guard_report)
  if direction is not None:
    results = [dataclasses.replace(result, direction=direction) for result in results]
  return results


def _format_step(result: StepResult) -> list[str]:
  statuses = "/".join("-" if status is None else str(status) for status in result.statuses)
  head = f"{'PASS' if result.passed else 'FAIL'}  {result.route:<58} {statuses:<8} {result.name}"
  if result.direction is not None:
    head += f"  [{result.direction}]"
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
      run_plans: list[tuple[str | None, list[str], list[str]]]
      if scenario.directions:
        if args.python_vs_python:
          run_plans = [("python-to-python", [str(PYTHON), "server.py"], [str(PYTHON), "server.py"])]
        else:
          run_plans = [
              (direction, [str(PYTHON), "server.py"], candidate_argv) if direction == "python-to-candidate" else
              (direction, candidate_argv, [str(PYTHON), "server.py"]) for direction in scenario.directions
          ]
      else:
        run_plans = [(None, candidate_argv, candidate_argv)]
      for run_number, (direction, candidate_first, candidate_handover) in enumerate(run_plans, start=1):
        heading = f"== [{scenario.stage}] {scenario.name} ({scenario.path.name})"
        if direction is not None:
          heading += f" direction={direction}"
        emit([heading])
        suffix = f"-{direction}" if direction is not None else ""
        scenario_dir = scratch / f"{index:02d}-{scenario.name}-{run_number:02d}{suffix}"
        scenario_results = _run_scenario(
            scenario, scenario_dir, candidate_first, candidate_handover, direction, args.tamper, guard_report)
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

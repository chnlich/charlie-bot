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
import base64
import collections
import concurrent.futures
import contextlib
import dataclasses
import hashlib
import hmac
import http.client
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import urllib.parse

import compare
import fakes
import guard
import normalize
import scenario_file
import scratch
import servers
from fakes import authority
from websockets import exceptions
from websockets.sync import client

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
PARITY_DIR = pathlib.Path(__file__).resolve().parent
PYTHON = REPO_ROOT / ".venv" / "bin" / "python"
FIXTURE = PARITY_DIR / "fixture.py"
REPORT_PATH = PARITY_DIR / "out" / "last-report.txt"
REQUEST_TIMEOUT_SECONDS = 30.0
# A stopped fake agent's process can take a moment to exit after its signal; only "terminated" waits that long.
AGENT_TERMINATION_TIMEOUT_SECONDS = 5.0
# Each side's tree directory. The names have one length, so the two tree roots do too: the homes embed their
# root in file content, and a file's size (and every ETag, memo revision or offset derived from it) matches
# only when the roots are equally long.
TREE_NAMES = {"baseline": "base", "candidate": "cand"}
# The rename --tamper applies to the candidate home: a deliberate mismatch that shows the harness failing.
TAMPER_SESSION = "atlas"
TAMPER_NAME = "Atlas planning (tampered)"
# The headless Chrome that scenarios with `headless_chrome: true` link into each tree at bin/headless-chrome.
HEADLESS_CHROME_BIN = (
    pathlib.Path.home() / ".cache/ms-playwright/chromium_headless_shell-1228/chrome-linux/headless_shell")
# A fake agent's process group must be gone this long after its SIGKILL. A zombie member keeps the group alive until
# its parent reaps it, and the harness reaps only the fake leaders it adopted.
FAKE_GROUP_EXIT_TIMEOUT_SECONDS = 10.0
# A fake agent writes its exit marker before it leaves the process table. The shutdown closes its stdin, so the marker
# can land in the same second as the server stop, and the cleanup waits this long for such a start to exit.
FAKE_EXIT_GRACE_SECONDS = 5.0


@dataclasses.dataclass
class StepResult:
  scenario: str
  name: str
  route: str
  statuses: tuple[int | str | None, int | str | None]
  lines: list[str]
  files: str
  responses: list[compare.Response | None]
  direction: str | None = None
  notes: list[str] = dataclasses.field(default_factory=list)

  @property
  def passed(self) -> bool:
    return not self.lines


class Side:
  """One server under comparison: its tree, process, normalization context and captures."""

  def __init__(self, label: str, tree: servers.Tree, port: int, strace: pathlib.Path) -> None:
    self.label = label
    self.tree = tree
    self.port = port
    self.strace = strace
    self.server: servers.ServerProcess | None = None
    self.server_processes: list[servers.ServerProcess] = []
    self.processes: dict[str, subprocess.Popen[bytes]] = {}
    self.ctx = normalize.Context(tree_root=str(tree.root))
    self.captures: dict[str, object] = {}
    self.fake_set: fakes.FakeSet | None = None
    self.ca_file: pathlib.Path | None = None

  def start_server(self, argv: list[str]) -> None:
    process_number = len(self.server_processes) + 1
    proxy_url = self.fake_set.proxy_url if self.fake_set is not None else None
    server = servers.ServerProcess(
        self.label,
        argv,
        self.tree,
        self.port,
        REPO_ROOT,
        process_number,
        self.strace,
        proxy_url=proxy_url,
        ca_file=self.ca_file)
    server.start()
    self.server = server
    self.server_processes.append(server)

  def wait_ready(self) -> None:
    if self.server is None:
      raise RuntimeError(f"{self.label} has no server to wait for")
    self.server.wait_ready()


def _preflight(rust_bin: pathlib.Path | None, strace: pathlib.Path, tmux: pathlib.Path) -> None:
  for path, what in ((PYTHON, "the worktree venv (run `uv run python --version` once)"), (strace, "strace"),
                     (tmux, "tmux (provide it with --tmux PATH)")):
    if not path.is_file() or not os.access(path, os.X_OK):
      raise SystemExit(f"parity: missing {what}: {path}")
  if rust_bin is not None and not os.access(rust_bin, os.X_OK):
    raise SystemExit(f"parity: --rust-bin is not an executable file: {rust_bin}")


def _build_fixtures(sides: list[Side], extras: tuple[str, ...]) -> dict:
  """Generate both homes in parallel and check they match; return the manifest."""

  def build(side: Side) -> None:
    servers.run_traced(
        [
            str(PYTHON),
            str(FIXTURE), "--manifest",
            str(side.tree.root / "fixture-manifest.json"), *(["--extras", ",".join(extras)] if extras else [])
        ],
        tree=side.tree,
        repo_root=REPO_ROOT,
        trace_log=side.tree.root / "logs" / f"{side.tree.label}.fixture.strace",
        log_path=side.tree.root / "logs" / f"{side.tree.label}.fixture.log",
        strace=side.strace)

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


def _substitute(side: Side, value: object, manifest: dict) -> object:
  return scenario_file.substitute(value, manifest, side.captures, str(side.tree.root), str(side.tree.home))


def _event_field_rules(
    scenario: scenario_file.Scenario,
    sides: list[Side],
    manifest: dict,
) -> tuple[tuple[compare.EventFieldRule, compare.EventFieldRule], ...]:
  rules = []
  rule_name = scenario_file.EVENT_FIELD_RULE_NAME
  for raw_rule in scenario.comparison_rules:
    if rule_name not in raw_rule:
      continue
    raw = raw_rule[rule_name]
    try:
      baseline = _substitute(sides[0], raw, manifest)
      candidate = _substitute(sides[1], raw, manifest)
    except scenario_file.UnresolvedReferenceError:
      continue

    def build_rule(value: dict) -> compare.EventFieldRule:
      event = value["event"]
      return compare.EventFieldRule(
          event_type=event["type"],
          match_fields=event["match_fields"],
          field=value["field"],
          allowed_baseline_values=tuple(value["allowed_baseline_values"]),
          required_candidate_value=value["required_candidate_value"],
      )

    rules.append((build_rule(baseline), build_rule(candidate)))
  return tuple(rules)


def _adjacent_order_rules(
    scenario: scenario_file.Scenario,
    side: Side,
    manifest: dict,
) -> list[tuple[compare.EventSelector, compare.EventSelector]]:
  rules = []
  rule_name = scenario_file.ADJACENT_ORDER_RULE_NAME
  for raw_rule in scenario.comparison_rules:
    if rule_name not in raw_rule:
      continue
    try:
      raw = _substitute(side, raw_rule[rule_name], manifest)
    except scenario_file.UnresolvedReferenceError:
      continue
    rules.append(
        tuple(
            compare.EventSelector(event_type=raw[key]["type"], match_fields=raw[key]["match_fields"])
            for key in ("first", "second")))
  return rules


def _event_type_stream_rules(
    scenario: scenario_file.Scenario,
    side: Side,
    manifest: dict,
) -> list[tuple[str, tuple[str, ...]]]:
  rules = []
  rule_name = scenario_file.EVENT_STREAM_RULE_NAME
  for raw_rule in scenario.comparison_rules:
    if rule_name not in raw_rule:
      continue
    try:
      raw = _substitute(side, raw_rule[rule_name], manifest)
    except scenario_file.UnresolvedReferenceError:
      continue
    path = f"home/sessions/{raw['session']}/data/chat_events.jsonl"
    rules.append((path, tuple(raw["event_types"])))
  return rules


def _delta_diff_lines(
    scenario: scenario_file.Scenario,
    sides: list[Side],
    manifest: dict,
    deltas: list[compare.Delta],
    candidate_is_python: bool,
) -> list[str]:
  """Every difference between the two trees' step deltas, after each side's adjacent pairs are written in order."""
  ordered = []
  for side, delta in zip(sides, deltas, strict=True):
    for first, second in _adjacent_order_rules(scenario, side, manifest):
      delta = compare.order_adjacent_pair(delta, first, second)
    for path, event_types in _event_type_stream_rules(scenario, side, manifest):
      delta = compare.order_event_type_streams_in_delta(delta, path, event_types)
    ordered.append(delta)
  return compare.delta_diff(
      ordered[0], ordered[1], sides[0].ctx, sides[1].ctx, _event_field_rules(scenario, sides, manifest),
      candidate_is_python)


def _prepare_contexts(
    sides: list[Side],
    snapshots: list[compare.Snapshot],
    event_type_stream_rules: list[list[tuple[str, tuple[str, ...]]]] | None = None,
) -> None:
  rules_by_side = event_type_stream_rules or [[] for _ in sides]
  for side, snapshot, side_rules in zip(sides, snapshots, rules_by_side, strict=True):
    spawns_path = _spawns_path(side.tree)
    spawn_records = _read_spawns(side.tree) if spawns_path.exists() else []
    _seed_spawn_session_ids(snapshot, spawn_records, side.ctx)
    _seed_spawn_run_ids(snapshot, spawn_records, side.ctx)
    comparable = dict(snapshot)
    for path, event_types in side_rules:
      entry = comparable.get(path)
      if entry is not None and entry.kind == "file":
        comparable[path] = compare.Entry(entry.kind, compare.order_event_type_streams(entry.data, event_types))
    extra_files = ()
    fake_log = side.tree.root / "logs" / "fakes.jsonl"
    if fake_log.exists():
      extra_files += (("logs/fakes.jsonl", fake_log.read_bytes()),)
    compare.prepare_context(comparable, side.ctx, extra_files)
    if spawns_path.exists():
      grouped = _spawn_groups(spawn_records, side.ctx)
      grouped_data = b"".join(json.dumps(record).encode() + b"\n" for group in grouped for record in group)
      normalize.prepare_snapshot([("fake-agent/spawns.jsonl", "file", grouped_data)], side.ctx)


def _seed_spawn_session_ids(snapshot: compare.Snapshot, records: list[dict], ctx: normalize.Context) -> None:
  """Give each spawned session an alias that names its task parent and its place among that parent's spawns.

  The group is a hash of the parent's stable name: its fixture id, its alias when the parent is itself a spawn, or
  its name for any other session the scenario created, such as a cron task's bound manager. A session with no task
  parent is in the "root" group. The position counts the group's spawns in first-launch order, so no order across
  groups reaches an alias. A cron firing's leaf session id derives from the firing's wall-clock stamp, so the two
  trees' ids differ; the alias replaces each id before the tree numbers the minted ids.
  """
  parents: dict[str, str | None] = {}
  for record in records:
    claims = record["run_token_claims"]
    if claims is None:
      continue
    session_id = claims["session_id"]
    if not isinstance(session_id, str) or not normalize.is_uuid(session_id):
      continue
    if session_id in ctx.fixture_uuids or session_id in ctx.stable_uuids or session_id in parents:
      continue
    metadata = snapshot.get(f"home/sessions/{session_id}/metadata.json")
    if metadata is None:
      raise RuntimeError(f"fake-agent spawn session {session_id!r} has no metadata.json in the home snapshot")
    parents[session_id] = json.loads(metadata.data)["task_parent_id"]

  siblings: collections.Counter[str | None] = collections.Counter()
  places: dict[str, int] = {}
  for session_id, parent in parents.items():
    siblings[parent] += 1
    places[session_id] = siblings[parent]
  aliases: dict[str, str] = {}
  parent_names: dict[str, str] = {}

  def parent_key(parent: str) -> str:
    if parent in ctx.fixture_uuids:
      return parent
    if parent in parents:
      return alias_of(parent)
    if parent not in parent_names:
      metadata = snapshot.get(f"home/sessions/{parent}/metadata.json")
      if metadata is None:
        raise RuntimeError(f"task parent {parent!r} of a spawned session has no metadata.json in the home snapshot")
      name = json.loads(metadata.data)["name"]
      if name in parent_names.values():
        raise RuntimeError(f"two task parents are named {name!r}, so their spawns would share one alias group")
      parent_names[parent] = name
    return f"name:{parent_names[parent]}"

  def alias_of(session_id: str) -> str:
    if session_id not in aliases:
      parent = parents[session_id]
      group = "root" if parent is None else hashlib.sha256(parent_key(parent).encode()).hexdigest()[:12]
      aliases[session_id] = f"<spawn-session-{group}-{places[session_id]}>"
    return aliases[session_id]

  for session_id in parents:
    ctx.stable_uuids[session_id] = alias_of(session_id)


def _seed_spawn_run_ids(snapshot: compare.Snapshot, records: list[dict], ctx: normalize.Context) -> None:
  """Give run ids stable names from their session's launch order before tree paths introduce them."""
  session_paths = {
      "/".join(parts[:3])
      for path in snapshot
      if (parts := path.split("/")) and len(parts) >= 3 and parts[:2] == ["home", "sessions"]
  }
  normalize.prepare_snapshot(
      [(path, "dir", b"") for path in sorted(session_paths, key=lambda p: normalize.path_sort_key(p, ctx))], ctx)
  for group in _spawn_groups(records, ctx):
    for launch_number, record in enumerate(group, start=1):
      claims = record["run_token_claims"]
      if claims is None:
        continue
      run_id = claims["run_id"]
      if not isinstance(run_id, str) or not run_id:
        raise RuntimeError(f"fake-agent spawn has an invalid run claim: {claims!r}")
      if run_id in ctx.fixture_uuids:
        continue
      session_key = normalize.normalize_text(claims["session_id"], ctx)
      session_hash = hashlib.sha256(session_key.encode()).hexdigest()
      alias = f"<spawn-run-{session_hash}-{launch_number}>"
      existing = ctx.stable_uuids.get(run_id)
      if existing is not None and existing != alias:
        raise RuntimeError(f"fake-agent run {run_id!r} changed launch group from {existing!r} to {alias!r}")
      ctx.stable_uuids[run_id] = alias


def _spawn_groups(records: list[dict], ctx: normalize.Context) -> list[list[dict]]:
  """Group fake-agent starts by their known session id, preserving launch order within each session."""
  groups: dict[str, list[dict]] = collections.defaultdict(list)
  for record in records:
    claims = record["run_token_claims"]
    if claims is None:
      session_key = ""
    else:
      session_id = claims["session_id"]
      if not isinstance(session_id, str) or not session_id:
        raise RuntimeError(f"fake-agent spawn has an invalid session claim: {claims!r}")
      known_before = session_id in ctx.fixture_uuids or session_id in ctx.minted_uuids or (
          session_id in ctx.stable_uuids)
      session_key = normalize.normalize_text(session_id, ctx)
      if not known_before and session_key != session_id:
        raise RuntimeError(f"fake-agent spawn session {session_id!r} is absent from the home snapshot")
    groups[session_key].append(record)
  return [groups[key] for key in sorted(groups)]


def _process_alive(record: dict) -> bool:
  identity = servers.process_identity(int(record["pid"]))
  return identity is not None and identity[1] == record["pid_start"] and identity[0] != "Z"


def _event_condition(side: Side, condition: dict, manifest: dict) -> bool:
  resolved = _substitute(side, condition, manifest)
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
  resolved = _substitute(side, condition, manifest)
  path = side.tree.home / resolved["path"]
  if os.path.commonpath((str(path.resolve()), str(side.tree.home.resolve()))) != str(side.tree.home.resolve()):
    raise RuntimeError(f"wait path escapes {side.tree.home}: {resolved['path']}")
  if not path.is_file():
    return False
  value = scenario_file.extract(json.loads(path.read_text(encoding="utf-8")), resolved["field"])
  return not compare.json_diff(value, resolved["equals"])


def _tree_path_missing_condition(side: Side, condition: dict, manifest: dict) -> bool:
  resolved = _substitute(side, condition, manifest)
  path = pathlib.Path(resolved["path"])
  if not path.is_absolute():
    path = side.tree.root / path
  tree_root = side.tree.root.resolve()
  resolved_path = path.resolve()
  if os.path.commonpath((str(resolved_path), str(tree_root))) != str(tree_root):
    raise RuntimeError(f"wait path escapes {tree_root}: {resolved['path']}")
  return not resolved_path.exists()


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
  if "tree_path_missing" in until:
    return _tree_path_missing_condition(side, until["tree_path_missing"], manifest)
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


def _server_logs(sides: list[Side]) -> dict[str, pathlib.Path]:
  """Each side's log for the server process the harness last started on that side."""
  for side in sides:
    if side.server is None:
      raise RuntimeError(f"{side.label} has no running server")
  return {side.label: side.server.log_path for side in sides}


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


def _agent_run_token(claims: dict) -> str:
  """A run token for *claims*, signed with the synthetic access key the server verifies it against."""
  payload = json.dumps(
      {
          "session_id": claims["session"],
          "run_id": claims["run"],
          "agent": "claude"
      },
      sort_keys=True,
      separators=(",", ":")).encode()
  payload_b64 = base64.urlsafe_b64encode(payload).rstrip(b"=").decode()
  signature = hmac.new(servers.ACCESS_KEY.encode(), payload_b64.encode(), hashlib.sha256).digest()
  return f"{payload_b64}.{base64.urlsafe_b64encode(signature).rstrip(b'=').decode()}"


def _request(side: Side, step: scenario_file.Step, manifest: dict) -> compare.Response:
  if side.server is None:
    raise RuntimeError(f"{side.label} has no running server")
  path = _substitute(side, step.path, manifest)
  query = _substitute(side, step.query, manifest)
  headers = _substitute(side, step.headers, manifest)
  if step.auth == "bearer":
    headers["Authorization"] = f"Bearer {servers.ACCESS_KEY}"
  elif step.auth == "cookie":
    headers["Cookie"] = f"charliebot_access_key={servers.ACCESS_KEY}"
  elif step.auth == "agent":
    headers["Authorization"] = f"Bearer {_agent_run_token(_substitute(side, step.agent, manifest))}"
  elif step.auth != "none":
    raise ValueError(f"unknown auth mode {step.auth!r}")
  body = None
  if step.has_json:
    body = json.dumps(_substitute(side, step.json_body, manifest)).encode()
    headers.setdefault("Content-Type", "application/json")
  elif step.body_file is not None:
    body = (scenario_file.DATA_DIR / step.body_file).read_bytes()
  elif step.multipart is not None:
    body = _multipart_body(step.multipart)
    headers.setdefault("Content-Type", f"multipart/form-data; boundary={scenario_file.MULTIPART_BOUNDARY}")
  url = path + ("?" + urllib.parse.urlencode(query) if query else "")
  connection = http.client.HTTPConnection(servers.HOST, side.server.port, timeout=REQUEST_TIMEOUT_SECONDS)
  try:
    connection.request(step.method, url, body=body, headers=headers)
    response = connection.getresponse()
    return compare.Response(response.status, response.getheaders(), response.read())
  finally:
    connection.close()


def _multipart_body(fields: dict) -> bytes:
  boundary = scenario_file.MULTIPART_BOUNDARY
  parts = []
  for name, value in fields.items():
    if isinstance(value, dict):
      file_path = scenario_file.DATA_DIR / value["file"]
      filename = value.get("filename", file_path.name)
      parts.extend(
          (
              f"--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n".encode(),
              f"Content-Type: {value['content_type']}\r\n\r\n".encode(),
              file_path.read_bytes(),
              b"\r\n",
          ))
    else:
      parts.extend(
          (
              f"--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n".encode(),
              str(value).encode("utf-8"),
              b"\r\n",
          ))
  parts.append(f"--{boundary}--\r\n".encode())
  return b"".join(parts)


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


def _agent_state_meets(side: Side, record: dict, expected: str) -> bool:
  exit_exists = (side.tree.root / "fake-agent" / f"exit-{record['index']}").is_file()
  alive = _process_alive(record)
  return {"alive": alive, "exited": exit_exists, "terminated": not alive and not exit_exists}[expected]


def _check_agent_after_stop(side: Side, expected: str, starts: list[dict], pending_indices: set[int]) -> list[str]:
  lines = []
  for record in starts:
    if not record["scripted"] or record["index"] not in pending_indices:
      continue
    satisfied = _agent_state_meets(side, record, expected)
    if expected == "terminated":
      deadline = time.monotonic() + AGENT_TERMINATION_TIMEOUT_SECONDS
      while not satisfied and time.monotonic() < deadline:
        time.sleep(servers.POLL_INTERVAL_SECONDS)
        satisfied = _agent_state_meets(side, record, expected)
    if not satisfied:
      lines.append(f"{side.label} fake-agent start {record['index']} did not meet {expected!r}: {record}")
  return lines


def _gate_markers(tree: servers.Tree) -> set[tuple[str, str]]:
  """The at-<name> gate markers of a tree's fake agents, as (file name, text) pairs."""
  agent_dir = tree.root / "fake-agent"
  return {(path.name, path.read_text(encoding="utf-8")) for path in agent_dir.glob("at-*") if path.is_file()}


def _unsettled_starts(side: Side, pending: set[int], markers_before: set[tuple[str, str]]) -> list[int]:
  """The pending starts that have neither exited nor written a gate marker of their own since the gate opened."""
  reached = {int(text) for _, text in _gate_markers(side.tree) - markers_before if text.isdigit()}
  agent_dir = side.tree.root / "fake-agent"
  return [index for index in sorted(pending) if index not in reached and not (agent_dir / f"exit-{index}").is_file()]


def _open_gate_while_stopped(sides: list[Side], gate: str, pending_by_side: list[set[int]],
                             timeout_seconds: float) -> list[str]:
  """Open *gate* in each fake-agent directory, then wait for each pending start to exit or reach a gate anew.

  A start that does neither before the timeout fails the step, and the line names its side and start index.
  """
  markers_before = [_gate_markers(side.tree) for side in sides]
  for side in sides:
    (side.tree.root / "fake-agent" / f"open-{gate}").write_text("open\n", encoding="utf-8")
  deadline = time.monotonic() + timeout_seconds
  while True:
    unsettled = [
        _unsettled_starts(side, pending, markers)
        for side, pending, markers in zip(sides, pending_by_side, markers_before, strict=True)
    ]
    if not any(unsettled):
      return []
    if time.monotonic() >= deadline:
      break
    time.sleep(0.02)
  return [
      f"{side.label} fake-agent start {index} neither exited nor reached a gate anew within {timeout_seconds}s "
      f"after open-{gate}" for side, starts in zip(sides, unsettled, strict=True) for index in starts
  ]


def _exit_lines(sides: list[Side], when: str) -> list[str]:
  """Name each server that has exited. Call it only while the harness means every server in *sides* to be running."""
  lines = []
  for side in sides:
    status = side.server.exit_status() if side.server is not None else None
    if status is not None:
      lines.append(f"{side.label} server exited with status {status} {when}; log {side.server.log_path}")
  return lines


def _owner_notes(sides: list[Side]) -> list[str]:
  return [f"{side.label} server owner pid {side.server.owner_pid} listens on port {side.port}" for side in sides]


def _browser_notes(label: str, stopped: int) -> list[str]:
  """The note for headless browsers a server left after SIGTERM; a server that left none adds no note."""
  if stopped == 0:
    return []
  return [f"{label} server left {stopped} headless browsers; the harness stopped them"]


def _recording_marks(sides: list[Side]) -> list[int]:
  return [side.fake_set.mark() if side.fake_set is not None else 0 for side in sides]


def _recording_diff_since(sides: list[Side], marks: list[int] | None = None, result_route: str = "") -> list[str]:
  marks = marks if marks is not None else [0, 0]
  recordings = [
      side.fake_set.records_since(mark) if side.fake_set is not None else []
      for side, mark in zip(sides, marks, strict=True)
  ]
  return compare.recording_diff(recordings[0], recordings[1], result_route, sides[0].ctx, sides[1].ctx)


def _settle_sides(sides: list[Side]) -> list[compare.Snapshot]:
  """Wait until both trees and in-memory fake recordings are quiet for one comparison interval."""
  deadline = time.monotonic() + compare.SETTLE_TIMEOUT_SECONDS
  previous: tuple[list[compare.Snapshot], list[list[dict]]] | None = None
  while time.monotonic() < deadline:
    try:
      snapshots = [compare.snapshot(side.tree.root, servers.HARNESS_ENTRIES) for side in sides]
    except compare.TreeChangingError:
      previous = None
      time.sleep(compare.SETTLE_QUIET_SECONDS)
      continue
    records = [side.fake_set.recorder.snapshot() if side.fake_set is not None else [] for side in sides]
    current = snapshots, records
    if current == previous:
      return snapshots
    previous = current
    time.sleep(compare.SETTLE_QUIET_SECONDS)
  raise RuntimeError(f"trees and fake recordings kept changing for {compare.SETTLE_TIMEOUT_SECONDS}s")


def _normalize_ws_frame(frame: dict, route: str, ctx: normalize.Context) -> dict:
  if "json" in frame:
    return {"json": normalize.normalize_json(frame["json"], normalize.ws_frame_rules(route), ctx)}
  if "text" in frame:
    return {"text": normalize.normalize_text(frame["text"], ctx)}
  return {"binary_base64": base64.b64encode(frame["binary"]).decode("ascii")}


def _wait_for_fake_handshake(side: Side, fake: str, mark: int, deadline: float) -> bool:
  if side.fake_set is None:
    raise RuntimeError(f"{side.label} has no fake services")
  while time.monotonic() < deadline:
    records = side.fake_set.records_since(mark)
    connections = set()
    for record in records:
      if record.get("kind") == "ws_handshake" and record.get("fake") == fake:
        connections.add(record["connection"])
    messages = [record for record in records if record.get("kind") == "ws_message" and record.get("fake") == fake]
    if any(message["connection"] in connections for message in messages):
      return True
    time.sleep(0.02)
  return False


def _close_outcome(error: exceptions.ConnectionClosed) -> str:
  """The WebSocket close a side's outcome records: its close code, or that the connection closed without one."""
  frame = error.rcvd if error.rcvd is not None else error.sent
  return "closed without a close frame" if frame is None else f"closed with code {frame.code}"


def _websocket_request(
    side: Side,
    step: scenario_file.Step,
    manifest: dict,
) -> tuple[int | str | None, list[dict], str, list[str]]:
  if side.server is None:
    raise RuntimeError(f"{side.label} has no running server")
  path = _substitute(side, step.path, manifest)
  query = _substitute(side, step.query, manifest)
  if step.auth == "token":
    query["token"] = servers.ACCESS_KEY
  uri = f"ws://{servers.HOST}:{side.server.port}{path}"
  if query:
    uri += "?" + urllib.parse.urlencode(query)
  deadline = time.monotonic() + step.timeout_seconds
  fake_mark = None
  if step.wait_for_fake_handshake is not None:
    if side.fake_set is None:
      raise RuntimeError(f"{side.label} has no fake services")
    fake_mark = side.fake_set.mark()
  lines: list[str] = []
  frames: list[dict] = []
  collected = ""
  try:
    socket = client.connect(
        uri,
        proxy=None,
        open_timeout=step.timeout_seconds,
        close_timeout=1,
        max_size=None,
    )
  except exceptions.InvalidStatus as error:
    status = error.response.status_code
    if step.expect_reject is None:
      lines.append(f"{side.label} WebSocket handshake rejected with HTTP {status}")
    return status, frames, collected, lines
  except TimeoutError:
    return None, frames, collected, [f"{side.label} WebSocket handshake timed out within {step.timeout_seconds}s"]
  except OSError as error:
    return None, frames, collected, [f"{side.label} WebSocket connect failed: {error}"]
  if step.expect_reject is not None:
    socket.close()
    return 101, frames, collected, [f"{side.label} WebSocket connected, expected HTTP {step.expect_reject} rejection"]
  with socket:
    if step.wait_for_fake_handshake is not None:
      if fake_mark is None:
        raise RuntimeError(f"{side.label} did not mark fake traffic before the WebSocket handshake")
      if not _wait_for_fake_handshake(side, step.wait_for_fake_handshake, fake_mark, deadline):
        lines.append(f"{side.label} did not reach the {step.wait_for_fake_handshake} WebSocket handshake")
        return 101, frames, collected, lines
    close: str | None = None
    try:
      for outgoing in step.send:
        resolved = _substitute(side, outgoing, manifest)
        if "json" in resolved:
          socket.send(json.dumps(resolved["json"], ensure_ascii=False))
        elif "text" in resolved:
          socket.send(resolved["text"])
        else:
          socket.send((scenario_file.DATA_DIR / resolved["binary_file"]).read_bytes())
    except exceptions.ConnectionClosed as error:
      close = _close_outcome(error)
    while close is None:
      if "frames" in step.until and len(frames) >= step.until["frames"]:
        break
      if "collected_contains" in step.until and step.until["collected_contains"] in collected:
        break
      try:
        received = socket.recv(timeout=max(0.0, deadline - time.monotonic()))
      except exceptions.ConnectionClosed as error:
        close = _close_outcome(error)
        if "close" not in step.until:
          lines.append(f"{side.label} WebSocket {close} before until condition {step.until}")
        break
      except TimeoutError:
        lines.append(f"{side.label} WebSocket receive timed out waiting for {step.until}")
        break
      if isinstance(received, bytes):
        frame = {"binary": received}
      else:
        try:
          frame = {"json": json.loads(received)}
        except json.JSONDecodeError:
          frame = {"text": received}
      frames.append(frame)
      if step.collect is not None and "json" in frame:
        document = frame["json"]
        if isinstance(document, dict) and all(
            document.get(key) == value for key, value in step.collect["where"].items()):
          try:
            value = scenario_file.extract(document, step.collect["field"])
          except scenario_file.ScenarioError:
            value = None
          if value is not None:
            if step.collect["decode"] == "base64":
              collected += base64.b64decode(value, validate=True).decode("utf-8")
            elif isinstance(value, str):
              collected += value
            else:
              raise ValueError(f"{side.label} collect field {step.collect['field']!r} is not text")
      if "collected_contains" in step.until and step.until["collected_contains"] in collected:
        break
  return (close if close is not None else 101), frames, collected, lines


def _run_handover_step(
    scenario: scenario_file.Scenario,
    sides: list[Side],
    step: scenario_file.Step,
    manifest: dict,
    before: list[compare.Snapshot],
    handover_argv: list[list[str]],
    candidate_is_python: bool,
) -> tuple[StepResult, list[compare.Snapshot]]:
  if step.agent_after_stop is None:
    raise RuntimeError(f"handover step {step.name!r} has no validated agent_after_stop value")
  record_marks = _recording_marks(sides)
  starts_before_stop = [_read_spawns(side.tree) for side in sides]
  pending_before_stop = [
      {
          record["index"]
          for record in starts
          if record["scripted"] and not (side.tree.root / "fake-agent" / f"exit-{record['index']}").exists()
      }
      for side, starts in zip(sides, starts_before_stop, strict=True)
  ]
  lines = _exit_lines(sides, f"before handover step {step.name!r} stopped it")
  browser_notes = []
  for side in sides:
    if side.server is None:
      raise RuntimeError(f"{side.label} has no server to hand over")
    browser_notes.extend(_browser_notes(side.label, side.server.stop_server()))
  if step.open_while_stopped is not None:
    if step.timeout_seconds is None:
      raise RuntimeError(f"handover step {step.name!r} has no validated timeout for open_while_stopped")
    lines.extend(_open_gate_while_stopped(sides, step.open_while_stopped, pending_before_stop, step.timeout_seconds))
  lines.extend(
      line for side, starts, pending in zip(sides, starts_before_stop, pending_before_stop, strict=True)
      for line in _check_agent_after_stop(side, step.agent_after_stop, starts, pending))
  for side, argv in zip(sides, handover_argv, strict=True):
    side.start_server(argv)
  for side in sides:
    side.wait_ready()
  after = _settle_sides(sides)
  _prepare_contexts(sides, after, [_event_type_stream_rules(scenario, side, manifest) for side in sides])
  deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
  lines.extend(_delta_diff_lines(scenario, sides, manifest, deltas, candidate_is_python))
  lines.extend(_recording_diff_since(sides, record_marks, step.route))
  lines.extend(_exit_lines(sides, f"during handover step {step.name!r}"))
  result = StepResult(
      scenario.name,
      step.name,
      step.route, (None, None),
      lines,
      compare.describe_delta(deltas[0]), [],
      notes=[*_owner_notes(sides), *browser_notes])
  return result, after


def _unresolved_result(
    scenario: scenario_file.Scenario, sides: list[Side], step: scenario_file.Step, manifest: dict) -> StepResult | None:
  """A failed result when a step's references do not resolve on some side, else None.

  The check runs before the step touches any server or fake, so a missing capture never leaves one side changed.
  """
  lines = []
  for side in sides:
    message = scenario_file.unresolved_reference(
        scenario, step, manifest, side.captures, str(side.tree.root), str(side.tree.home))
    if message is not None:
      lines.append(f"{side.label}: {message}")
  if not lines:
    return None
  return StepResult(scenario.name, step.name, step.route, (None, None), lines, "", [])


def _run_step(
    scenario: scenario_file.Scenario, sides: list[Side], step: scenario_file.Step, manifest: dict,
    before: list[compare.Snapshot], handover_argv: list[list[str]],
    candidate_is_python: bool) -> tuple[StepResult, list[compare.Snapshot]]:
  if step.kind == "handover":
    return _run_handover_step(scenario, sides, step, manifest, before, handover_argv, candidate_is_python)
  lines: list[str] = []
  responses: list[compare.Response | None] = []
  record_marks = _recording_marks(sides)
  baseline = candidate = None
  if step.kind == "http":
    for side in sides:
      try:
        responses.append(_request(side, step, manifest))
      except (OSError, http.client.HTTPException) as error:
        # A failed request fails this step. The exit check after the step names the status of a server that died.
        lines.append(f"request to {side.label} failed: {error}")
        responses.append(None)
    baseline, candidate = responses
    for side, response in zip(sides, responses, strict=True):
      if response is not None and step.expect_status is not None and response.status != step.expect_status:
        lines.append(f"{side.label} status {response.status}, scenario expects {step.expect_status}")
  elif step.kind == "wait":
    lines.extend(_wait_for_sides(sides, step, manifest))
  elif step.kind == "log_event":
    if step.log_event is None or step.timeout_seconds is None:
      raise RuntimeError(f"log_event step {step.name!r} has no validated event or timeout")
    lines.extend(
        servers.wait_for_log_events(_server_logs(sides), (step.log_event,), step.log_fields, step.timeout_seconds))
  elif step.kind == "gate":
    if step.open is None:
      raise RuntimeError(f"gate step {step.name!r} has no validated open name")
    for side in sides:
      (side.tree.root / "fake-agent" / f"open-{step.open}").write_text("open\n", encoding="utf-8")
  elif step.kind == "fake":
    for side in sides:
      if side.fake_set is None:
        raise RuntimeError(f"{side.label} has no declared fake services")
      missing = side.fake_set.wait_ready((step.fake,), fakes.FAKE_READY_TIMEOUT_SECONDS)
      if missing:
        lines.append(f"{side.label} fake {missing[0]} was not ready before step {step.name!r}")
      else:
        payload = _substitute(side, step.payload, manifest)
        side.fake_set.push(step.fake, step.action, payload)
  elif step.kind == "process":
    if step.payload is None or step.action not in ("start", "stop"):
      raise RuntimeError(f"process step {step.name!r} is not validated")
    process_id = step.payload["process_id"]
    owner = sides[0]
    if step.action == "start":
      if process_id in owner.processes:
        raise RuntimeError(f"synthetic process {process_id!r} is already running")
      process = subprocess.Popen(
          ["/bin/sleep", "60"],
          stdin=subprocess.DEVNULL,
          stdout=subprocess.DEVNULL,
          stderr=subprocess.DEVNULL,
          start_new_session=True)
      owner.processes[process_id] = process
      capture_name = next(iter(step.capture))
      for side in sides:
        side.captures[capture_name] = str(process.pid)
    else:
      process = owner.processes.pop(process_id)
      if step.timeout_seconds:
        time.sleep(step.timeout_seconds)
      process.terminate()
      process.wait(timeout=5)
  elif step.kind == "ws":
    outcomes = [_websocket_request(side, step, manifest) for side in sides]
    statuses = tuple(outcome[0] for outcome in outcomes)
    for outcome in outcomes:
      lines.extend(outcome[3])
    if step.expect_reject is not None:
      for side, status in zip(sides, statuses, strict=True):
        if status != step.expect_reject:
          lines.append(f"{side.label} WebSocket rejection status {status}, scenario expects {step.expect_reject}")
    elif isinstance(step.compare, dict):
      required = step.compare["collected_contains"]
      for side, outcome in zip(sides, outcomes, strict=True):
        lines.extend(
            f"{side.label} collected WebSocket text does not contain {text!r}" for text in required
            if text not in outcome[2])
    else:
      left_frames = [_normalize_ws_frame(frame, step.route, sides[0].ctx) for frame in outcomes[0][1]]
      right_frames = [_normalize_ws_frame(frame, step.route, sides[1].ctx) for frame in outcomes[1][1]]
      lines.extend(f"WebSocket {line}" for line in compare.json_diff(left_frames, right_frames))
    if statuses[0] != statuses[1]:
      lines.append(f"WebSocket outcome {sides[0].label} {statuses[0]}, {sides[1].label} {statuses[1]}")
  else:
    raise ValueError(f"unknown scenario step kind {step.kind!r}")
  after = _settle_sides(sides)
  _prepare_contexts(sides, after, [_event_type_stream_rules(scenario, side, manifest) for side in sides])
  lines.extend(_exit_lines(sides, f"during step {step.name!r}"))
  if baseline is not None and candidate is not None:
    if step.kind == "http" and step.http_compare == "status_and_shape":
      lines.extend(compare.status_and_shape_diff(step.route, baseline, candidate, sides[0].ctx, sides[1].ctx))
    else:
      lines.extend(
          compare.response_diff(
              step.route,
              baseline,
              candidate,
              sides[0].ctx,
              sides[1].ctx,
              event_field_rules=_event_field_rules(scenario, sides, manifest),
              candidate_is_python=candidate_is_python,
          ))
  lines.extend(_recording_diff_since(sides, record_marks, step.route))
  deltas = None
  if not step.defer_files:
    deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
    lines.extend(_delta_diff_lines(scenario, sides, manifest, deltas, candidate_is_python))
  if step.kind == "http":
    for side, response in zip(sides, responses, strict=True):
      if response is not None:
        lines.extend(_capture(side, step, response))
  statuses = (baseline.status if baseline else None, candidate.status if candidate else None)
  if step.kind == "ws":
    statuses = outcomes[0][0], outcomes[1][0]
  files = compare.describe_delta(deltas[0]) if deltas is not None else ""
  next_before = before if step.defer_files else after
  return StepResult(scenario.name, step.name, step.route, statuses, lines, files, responses), next_before


def _save_responses(log_dir: pathlib.Path, number: int, sides: list[Side], result: StepResult) -> None:
  """Keep both raw responses of a differing step next to the server logs."""
  for side, response in zip(sides, result.responses, strict=True):
    if response is not None:
      head = [f"HTTP {response.status}", *(f"{name}: {value}" for name, value in response.headers), "", ""]
      (log_dir /
       f"step{number:03d}.{side.label}.response").write_bytes("\n".join(head).encode() + response.decoded_body())


def _group_exists(pgid: int) -> bool:
  """Whether any process still belongs to the process group *pgid*. A zombie member counts until it is reaped."""
  try:
    os.killpg(pgid, 0)
  except ProcessLookupError:
    return False
  return True


def _reap_leader(pid: int, pid_start: str) -> None:
  """Reap a fake agent that this harness adopted, since a zombie keeps its process group alive for kill(2).

  Match the recorded start time first so a recycled pid cannot make the harness reap a different child.
  """
  identity = servers.process_identity(pid)
  if identity is None or identity[1] != pid_start:
    return
  with contextlib.suppress(ChildProcessError):
    os.waitpid(pid, os.WNOHANG)


def _kill_fake_group(label: str, record: dict) -> list[str]:
  """SIGKILL one fake-agent start and wait for it to be gone.

  When the start leads its own process group (pgid == pid), SIGKILL the whole group: a leader that still has the
  recorded pid_start is the start, so its group is the start's, and a gone leader keeps its group only while kill(2)
  still reaches that group. When it does not lead its group, the group also holds unrelated processes (the server
  that started it without setsid, or the harness itself), so the group is never signalled. A pid that no longer has
  the recorded pid_start has exited (a probe the server reaped, for one), so it gets no signal and no line. A pid that
  still has it gets SIGKILL alone, and the step gets a failure line that names the record.
  """
  pid = int(record["pid"])
  pgid = int(record["pgid"])
  if pgid != pid:
    start = servers.process_identity(pid)
    if start is None or start[1] != record["pid_start"]:
      return []
    with contextlib.suppress(ProcessLookupError):
      os.kill(pid, signal.SIGKILL)
    return [f"{label} fake agent {pid} does not lead its own process group (pgid {pgid} != pid {pid}): {record}"]
  leader = servers.process_identity(pid)
  if (leader is not None and leader[1] == record["pid_start"]) or (leader is None and _group_exists(pgid)):
    with contextlib.suppress(ProcessLookupError):
      os.killpg(pgid, signal.SIGKILL)
  deadline = time.monotonic() + FAKE_GROUP_EXIT_TIMEOUT_SECONDS
  while True:
    _reap_leader(pid, record["pid_start"])
    if not _group_exists(pgid):
      return []
    if time.monotonic() >= deadline:
      message = f"{label} fake-agent process group {pgid} remained {FAKE_GROUP_EXIT_TIMEOUT_SECONDS}s after SIGKILL"
      return [f"{message}: {record}"]
    time.sleep(servers.POLL_INTERVAL_SECONDS)


def _await_marked_exit(side: Side, record: dict) -> None:
  """Wait up to FAKE_EXIT_GRACE_SECONDS for a start that already wrote its exit marker to leave the process table."""
  if not (side.tree.root / "fake-agent" / f"exit-{record['index']}").is_file():
    return
  deadline = time.monotonic() + FAKE_EXIT_GRACE_SECONDS
  while _process_alive(record) and time.monotonic() < deadline:
    time.sleep(0.02)


def _cleanup_scenario(sides: list[Side]) -> tuple[list[str], list[str]]:
  lines = _exit_lines(sides, "before the shutdown step")
  notes = []
  for side in sides:
    if side.server is not None:
      try:
        notes.extend(_browser_notes(side.label, side.server.stop_server()))
      except RuntimeError as error:
        lines.append(f"{side.label} server stop: {error}")
        side.server.kill()
  for side in sides:
    for process_id, process in list(side.processes.items()):
      if process.poll() is None:
        process.terminate()
        try:
          process.wait(timeout=5)
        except subprocess.TimeoutExpired:
          process.kill()
          process.wait()
        lines.append(f"{side.label} synthetic process {process_id!r} needed cleanup at scenario end")
      del side.processes[process_id]
  for side in sides:
    if side.fake_set is not None:
      try:
        side.fake_set.stop()
      except RuntimeError as error:
        lines.append(f"{side.label} fake-service stop: {error}")
      for host, count in sorted(side.fake_set.refused_hosts().items()):
        notes.append(f"{side.label} fake proxy refused {host} x{count}")
  starts = [(side, record) for side in sides for record in _read_spawns(side.tree)]
  for side, record in starts:
    _await_marked_exit(side, record)
  for _, record in starts:
    if _process_alive(record):
      lines.append(f"fake agent remained alive at scenario end and was killed: {record}")
  for side, record in starts:
    lines.extend(_kill_fake_group(side.label, record))
  for side in sides:
    for server in side.server_processes:
      try:
        server.wait_trace()
      except RuntimeError as error:
        lines.append(f"{side.label} strace cleanup: {error}")
  return lines, notes


def _spawns_diff(sides: list[Side], expect_starts: int | None) -> list[str]:
  records = [_read_spawns(side.tree) for side in sides]
  lines = []
  if expect_starts is not None:
    for side, starts in zip(sides, records, strict=True):
      if len(starts) != expect_starts:
        lines.append(f"{side.label} has {len(starts)} fake-agent starts; scenario expects {expect_starts}")
  normalized = [
      [
          [
              {
                  key: value for key, value in normalize.normalize_json(
                      record, normalize.file_rules("fake-agent/spawns.jsonl"), side.ctx).items() if key != "index"
              } for record in group
          ] for group in _spawn_groups(starts, side.ctx)
      ] for side, starts in zip(sides, records, strict=True)
  ]
  lines.extend(f"fake-agent spawns {line}" for line in compare.json_diff(normalized[0], normalized[1]))
  return lines


def _check_headless_chrome(scenarios: list[scenario_file.Scenario], chrome: pathlib.Path) -> None:
  if not any(scenario.headless_chrome for scenario in scenarios):
    return
  if not chrome.is_file():
    raise SystemExit(f"parity: missing headless Chrome {chrome}; provide it with --headless-chrome PATH")


def _link_tmux(sides: list[Side], tmux: pathlib.Path) -> None:
  for side in sides:
    servers.link_tmux(side.tree, tmux)


def _link_headless_chrome(sides: list[Side], chrome: pathlib.Path) -> None:
  for side in sides:
    bin_dir = side.tree.root / "bin"
    bin_dir.mkdir()
    (bin_dir / "headless-chrome").symlink_to(chrome)


def _run_scenario(
    scenario: scenario_file.Scenario,
    work_dir: pathlib.Path,
    ports: list[int],
    candidate_first_argv: list[str],
    candidate_handover_argv: list[str],
    direction: str | None,
    tamper: bool,
    guard_report: guard.Report,
    certificates: authority.Certificates | None,
    strace: pathlib.Path,
    tmux: pathlib.Path,
    headless_chrome: pathlib.Path,
    candidate_is_python: bool,
) -> list[StepResult]:
  log_dir = work_dir / "logs"
  log_dir.mkdir(parents=True)
  python_argv = [str(PYTHON), "server.py"]
  sides = []
  for label, port in (("baseline", ports[0]), ("candidate", ports[1])):
    tree = servers.make_tree(TREE_NAMES[label], work_dir / TREE_NAMES[label])
    side = Side(label, tree, port, strace)
    if scenario.fakes:
      if certificates is None:
        raise RuntimeError("a scenario declares fakes but no run certificate was generated")
      side.ca_file = certificates.ca
    config_overlay = scenario_file.substitute(scenario.config, {}, {}, str(tree.root), str(tree.home))
    credentials_overlay = scenario_file.substitute(scenario.credentials, {}, {}, str(tree.root), str(tree.home))
    servers.write_home_config(tree, port, config_overlay, credentials_overlay)
    sides.append(side)
  if len(str(sides[0].tree.root)) != len(str(sides[1].tree.root)):
    raise RuntimeError(f"tree roots differ in length: {sides[0].tree.root} {sides[1].tree.root}")
  _link_tmux(sides, tmux)
  manifest = _build_fixtures(sides, scenario.fixture_extras)
  scenario_file.check_references(scenario, manifest)
  if scenario.headless_chrome:
    _link_headless_chrome(sides, headless_chrome)
  _write_fake_agent_script(sides, scenario.fake_agent)
  if tamper:
    _tamper(sides[1], manifest)
  before = [compare.snapshot(side.tree.root, servers.HARNESS_ENTRIES) for side in sides]
  results: list[StepResult] = []
  try:
    if scenario.fakes:
      if certificates is None:
        raise RuntimeError("fake services need a run-local certificate")
      for side in sides:
        side.fake_set = fakes.FakeSet(side.tree.root, scenario.fakes, certificates.leaf, certificates.private_key)
        side.fake_set.start()
    startup_marks = _recording_marks(sides)
    sides[0].start_server(python_argv)
    sides[1].start_server(candidate_first_argv)
    for side in sides:
      side.wait_ready()
    startup_lines = []
    for side in sides:
      if side.fake_set is not None:
        missing = side.fake_set.wait_ready(tuple(scenario.fakes), fakes.FAKE_READY_TIMEOUT_SECONDS)
        startup_lines.extend(f"{side.label} fake {name} did not become ready" for name in missing)
    after = _settle_sides(sides)
    _prepare_contexts(sides, after, [_event_type_stream_rules(scenario, side, manifest) for side in sides])
    deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
    startup_lines.extend(_delta_diff_lines(scenario, sides, manifest, deltas, candidate_is_python))
    startup_lines.extend(_recording_diff_since(sides, startup_marks, "(startup)"))
    startup_lines.extend(_exit_lines(sides, "during server startup"))
    results.append(
        StepResult(
            scenario.name,
            "server startup",
            "(startup)", (None, None),
            startup_lines,
            compare.describe_delta(deltas[0]), [],
            notes=_owner_notes(sides)))
    before = after
    for number, step in enumerate(scenario.steps, start=1):
      handover_argv = [python_argv, candidate_handover_argv]
      unresolved = _unresolved_result(scenario, sides, step, manifest)
      if unresolved is not None:
        results.append(unresolved)
        continue
      result, before = _run_step(scenario, sides, step, manifest, before, handover_argv, candidate_is_python)
      results.append(result)
      if not result.passed and result.responses:
        _save_responses(log_dir, number, sides, result)
  finally:
    shutdown_marks = _recording_marks(sides)
    cleanup_lines, cleanup_notes = _cleanup_scenario(sides)
    after = _settle_sides(sides)
    _prepare_contexts(sides, after, [_event_type_stream_rules(scenario, side, manifest) for side in sides])
    deltas = [compare.delta(old, new) for old, new in zip(before, after, strict=True)]
    cleanup_lines.extend(_recording_diff_since(sides, shutdown_marks, "(shutdown)"))
    cleanup_lines.extend(_spawns_diff(sides, scenario.expect_starts))
    results.append(
        StepResult(
            scenario.name,
            "server shutdown and fake-agent cleanup",
            "(shutdown)", (None, None), [
                *cleanup_lines,
                *_delta_diff_lines(scenario, sides, manifest, deltas, candidate_is_python),
            ],
            compare.describe_delta(deltas[0]), [],
            notes=cleanup_notes))
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
  return [head, *(f"      {line}" for line in result.lines), *(f"      note: {note}" for note in result.notes)]


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
      "--headless-chrome",
      type=pathlib.Path,
      default=HEADLESS_CHROME_BIN,
      help="headless Chrome binary that scenarios with headless_chrome: true link into each tree's bin/",
  )
  parser.add_argument(
      "--strace",
      type=pathlib.Path,
      default=servers.STRACE_BIN,
      help="strace binary that traces every process under test",
  )
  parser.add_argument(
      "--tmux",
      type=pathlib.Path,
      default=servers.TMUX_BIN,
      help="tmux executable that each tree's server resolves on its PATH",
  )
  parser.add_argument(
      "--tamper",
      action="store_true",
      help=f"rename session {TAMPER_SESSION!r} in the candidate home: the run must fail")
  parser.add_argument("--keep", action="store_true", help="keep the temporary trees and logs after the run")
  args = parser.parse_args()
  rust_bin = args.rust_bin.resolve() if args.rust_bin else None
  strace = args.strace.expanduser().resolve()
  tmux = args.tmux.expanduser().resolve()
  _preflight(rust_bin, strace, tmux)
  servers.become_child_subreaper()
  candidate_argv = [str(rust_bin)] if rust_bin else [str(PYTHON), "server.py"]
  scenarios = scenario_file.load_all(args.stage)
  headless_chrome = args.headless_chrome.expanduser().resolve()
  _check_headless_chrome(scenarios, headless_chrome)
  if any(scenario.fakes for scenario in scenarios) and not authority.OPENSSL.is_file():
    raise SystemExit(f"parity: fake services require {authority.OPENSSL}")
  guard_report = guard.Report()
  output: list[str] = []

  def emit(lines: list[str]) -> None:
    output.extend(lines)
    print("\n".join(lines), flush=True)

  results: list[StepResult] = []
  runs: list[dict] = []
  scratch_root: pathlib.Path | None = None
  try:
    # The servers run only on homes under /tmp, whatever TMPDIR says: a tree's tmux socket path must stay
    # within 107 bytes (servers.make_tree), and a long TMPDIR breaks that. The tree lives inside the cleanup
    # scope, so an exception between mkdtemp and the first run (for example in authority.generate) removes it too.
    scratch.sweep()
    scratch_root = pathlib.Path(tempfile.mkdtemp(prefix=scratch.SCRATCH_PREFIX, dir="/tmp"))
    scratch.record_owner(scratch_root)
    certificates = authority.generate(scratch_root / "certificates") if any(
        scenario.fakes for scenario in scenarios) else None
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
        # The short name keeps every tree's socket path under 107 bytes; RUNS_MANIFEST maps it back to the scenario.
        run_name = f"{index:02d}-{run_number:02d}"
        heading = f"== [{scenario.stage}] {scenario.name} ({scenario.path.name}) run {run_name}"
        if direction is not None:
          heading += f" direction={direction}"
        emit([heading])
        scenario_dir = scratch_root / run_name
        trees = {label: str(scenario_dir / name) for label, name in TREE_NAMES.items()}
        runs.append({"run": run_name, "scenario": scenario.name, "direction": direction, "trees": trees})
        # The ports stay reserved until both servers of this run have stopped.
        with servers.pick_ports(2) as ports:
          scenario_results = _run_scenario(
              scenario, scenario_dir, ports, candidate_first, candidate_handover, direction, args.tamper, guard_report,
              certificates, strace, tmux, headless_chrome, args.python_vs_python)
        for result in scenario_results:
          lines = _format_step(result)
          output.extend(lines)
          print("\n".join([lines[0], *compare.capped(lines[1:])]), flush=True)
        results.extend(scenario_results)
  finally:
    failed = [result for result in results if not result.passed]
    # A run in progress leaves sys.exc_info() set in its finally block, so an exception counts as unclean too.
    clean = bool(results) and not failed and not guard_report.violations and sys.exc_info()[0] is None
    if scratch_root is not None:
      scratch.finalize(scratch_root, args.keep, clean, runs)

  emit(["", "Routes:", *_route_summary(results)])
  # Each trace path once, in the order the guard met it.
  undecodable_traces = dict.fromkeys(guard_report.undecodable)
  undecodable_where = f" in {', '.join(undecodable_traces)}" if undecodable_traces else ""
  emit(
      [
          "",
          (
              f"Outside-write guard: {len(guard_report.violations)} file changes outside the trees "
              f"({guard_report.traces} traces); {len(guard_report.refused)} writes refused by the sandbox; "
              f"{len(guard_report.undecodable)} undecodable calls (strace printed ???){undecodable_where}"),
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

"""Scenario files: YAML documents under scenarios/, each tagged with the port stage its routes belong to.

A scenario names its stage and lists its steps. HTTP steps send requests to both servers. Wait steps observe
durable state, gate steps release fake agents, and handover steps restart a server on the same home.

Strings in a step may reference the fixture manifest as ``${sessions.atlas}``, ``${runs.notes_r2.run}`` or
``${missing_session}`` (``${missing_run}``), and an earlier step's capture as ``${capture.name}``. A step's route template is its
method and path with every reference replaced by a placeholder (``{session_id}``, ``{run_id}``, ...); the
report groups steps by it. A capture reads a value out of each server's own JSON response, so a cursor one
server issued goes back to that server.
"""

import dataclasses
import math
import pathlib
import re
from typing import Any

import yaml

SCENARIO_DIR = pathlib.Path(__file__).resolve().parent / "scenarios"
STEP_KINDS = ("http", "wait", "gate", "handover")
RESERVED_KINDS = ("ws",)
AUTH_MODES = ("bearer", "cookie", "none")
METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS")
HANDOVER_DIRECTIONS = ("python-to-candidate", "candidate-to-python")

_REFERENCE = re.compile(r"\$\{(?P<ref>[A-Za-z0-9_.]+)\}")
_SCENARIO_KEYS = {"stage", "name", "description", "steps", "fake_agent", "expect_starts", "directions"}
_HTTP_STEP_KEYS = {
    "kind", "name", "method", "path", "query", "headers", "json", "auth", "expect_status", "capture", "defer_files"
}
_WAIT_STEP_KEYS = {"kind", "name", "timeout_seconds", "until"}
_GATE_STEP_KEYS = {"kind", "name", "open"}
_HANDOVER_STEP_KEYS = {"kind", "name", "agent_after_stop"}


class ScenarioError(ValueError):
  """A scenario file that does not follow the format."""


@dataclasses.dataclass(frozen=True)
class Step:
  kind: str
  name: str
  method: str
  path: str
  query: dict[str, str]
  headers: dict[str, str]
  json_body: Any
  has_json: bool
  auth: str
  expect_status: int | None
  capture: dict[str, str]
  defer_files: bool
  timeout_seconds: float | None
  until: dict | None
  open: str | None
  agent_after_stop: str | None

  @property
  def route(self) -> str:
    """The route template: method and path with each reference as its placeholder."""
    if self.kind != "http":
      return f"({self.kind})"
    return f"{self.method} {_REFERENCE.sub(lambda match: _placeholder(match['ref']), self.path)}"


@dataclasses.dataclass(frozen=True)
class Scenario:
  path: pathlib.Path
  stage: str
  name: str
  steps: tuple[Step, ...]
  fake_agent: dict | None
  expect_starts: int | None
  directions: tuple[str, ...] | None


def _placeholder(ref: str) -> str:
  parts = ref.split(".")
  if parts[0] in ("sessions", "missing_session") or parts[-1] == "session":
    return "{session_id}"
  if parts[0] in ("runs", "missing_run"):
    return "{run_id}"
  if parts[0] == "inputs":
    return "{input_id}"
  if parts[0] == "capture":
    return "{" + parts[1] + "}"
  return "{" + parts[0] + "}"


def _string_map(value: Any, where: str) -> dict[str, str]:
  if not isinstance(value, dict):
    raise ScenarioError(f"{where} must be a mapping")
  mapped: dict[str, str] = {}
  for key, item in value.items():
    if not isinstance(key, str) or isinstance(item, (dict, list)) or item is None:
      raise ScenarioError(f"{where} maps names to scalar values; got {key!r}: {item!r}")
    mapped[key] = item if isinstance(item, str) else str(item).lower() if isinstance(item, bool) else str(item)
  return mapped


def _step(raw: Any, where: str) -> Step:
  if not isinstance(raw, dict):
    raise ScenarioError(f"{where} must be a mapping")
  if isinstance(raw.get("name"), str) and raw["name"]:
    where = f"{where} {raw['name']!r}"
  kind = raw.get("kind", "http")
  if not isinstance(kind, str):
    raise ScenarioError(f"{where}: step kind must be a string")
  if kind in RESERVED_KINDS:
    raise ScenarioError(f"{where}: step kind {kind!r} is reserved for a later stage and not implemented yet")
  if kind not in STEP_KINDS:
    raise ScenarioError(f"{where}: unknown step kind {kind!r}")
  allowed = {
      "http": _HTTP_STEP_KEYS,
      "wait": _WAIT_STEP_KEYS,
      "gate": _GATE_STEP_KEYS,
      "handover": _HANDOVER_STEP_KEYS,
  }[kind]
  unknown = set(raw) - allowed
  if unknown:
    raise ScenarioError(f"{where}: unknown keys {sorted(unknown)}")
  if not isinstance(raw.get("name"), str) or not raw["name"]:
    raise ScenarioError(f"{where}: 'name' is required and must be a non-empty string")
  method = ""
  path = ""
  query: dict[str, str] = {}
  headers: dict[str, str] = {}
  json_body = None
  has_json = False
  auth = "none"
  expect_status = None
  capture: dict[str, str] = {}
  defer_files = False
  timeout_seconds = None
  until = None
  open_name = None
  agent_after_stop = None
  if kind == "http":
    for required in ("method", "path"):
      if not isinstance(raw.get(required), str) or not raw[required]:
        raise ScenarioError(f"{where}: {required!r} is required and must be a non-empty string")
    method = raw["method"].upper()
    if method not in METHODS:
      raise ScenarioError(f"{where}: unknown method {raw['method']!r}")
    path = raw["path"]
    if not path.startswith("/") or "?" in path:
      raise ScenarioError(f"{where}: path must start with / and carry no query (use 'query')")
    auth = raw.get("auth", "bearer")
    if auth not in AUTH_MODES:
      raise ScenarioError(f"{where}: auth must be one of {AUTH_MODES}")
    expect_status = raw.get("expect_status")
    if expect_status is not None and (not isinstance(expect_status, int) or isinstance(expect_status, bool)):
      raise ScenarioError(f"{where}: expect_status must be an integer")
    defer_files = raw.get("defer_files", False)
    if not isinstance(defer_files, bool):
      raise ScenarioError(f"{where}: defer_files must be a boolean")
    query = _string_map(raw.get("query", {}), f"{where}.query")
    headers = _string_map(raw.get("headers", {}), f"{where}.headers")
    json_body = raw.get("json")
    has_json = "json" in raw
    capture = _string_map(raw.get("capture", {}), f"{where}.capture")
  elif kind == "wait":
    timeout_seconds = raw.get("timeout_seconds")
    valid_timeout_type = isinstance(timeout_seconds, (int, float)) and not isinstance(timeout_seconds, bool)
    try:
      finite_timeout = valid_timeout_type and math.isfinite(timeout_seconds)
    except OverflowError:
      finite_timeout = False
    if not finite_timeout or timeout_seconds <= 0:
      raise ScenarioError(f"{where}: 'timeout_seconds' is required and must be positive")
    until = _until(raw.get("until"), where)
  elif kind == "gate":
    open_name = raw.get("open")
    if not isinstance(open_name, str) or not open_name:
      raise ScenarioError(f"{where}: 'open' is required and must be a non-empty string")
  elif kind == "handover":
    agent_after_stop = raw.get("agent_after_stop")
    if agent_after_stop not in ("alive", "exited", "terminated"):
      raise ScenarioError(f"{where}: 'agent_after_stop' must be alive, exited or terminated")
  return Step(
      kind=kind,
      name=raw["name"],
      method=method,
      path=path,
      query=query,
      headers=headers,
      json_body=json_body,
      has_json=has_json,
      auth=auth,
      expect_status=expect_status,
      capture=capture,
      defer_files=defer_files,
      timeout_seconds=timeout_seconds,
      until=until,
      open=open_name,
      agent_after_stop=agent_after_stop,
  )


def _until(raw: Any, where: str) -> dict:
  if not isinstance(raw, dict) or len(raw) != 1:
    raise ScenarioError(f"{where}: 'until' must contain one condition")
  kind, condition = next(iter(raw.items()))
  if kind == "event":
    if not isinstance(condition, dict):
      raise ScenarioError(f"{where}: until.event must be a mapping")
    unknown = set(condition) - {"session", "type", "fields", "count"}
    if unknown:
      raise ScenarioError(f"{where}: until.event has unknown keys {sorted(unknown)}")
    for required in ("session", "type"):
      if not isinstance(condition.get(required), str) or not condition[required]:
        raise ScenarioError(f"{where}: until.event.{required} is required and must be a non-empty string")
    fields = condition.get("fields", {})
    if not isinstance(fields, dict) or any(not isinstance(key, str) for key in fields):
      raise ScenarioError(f"{where}: until.event.fields must map string names to JSON values")
    count = condition.get("count", 1)
    if not isinstance(count, int) or isinstance(count, bool) or count < 1:
      raise ScenarioError(f"{where}: until.event.count must be a positive integer")
    return {"event": {"session": condition["session"], "type": condition["type"], "fields": fields, "count": count}}
  if kind == "file_field":
    if not isinstance(condition, dict):
      raise ScenarioError(f"{where}: until.file_field must be a mapping")
    if set(condition) != {"path", "field", "equals"}:
      raise ScenarioError(f"{where}: until.file_field requires path, field and equals")
    path = condition["path"]
    field = condition["field"]
    if not isinstance(path, str) or not path or pathlib.PurePosixPath(path).is_absolute() or ".." in path.split("/"):
      raise ScenarioError(f"{where}: until.file_field.path must be a home-relative path without '..'")
    if not isinstance(field, str) or not field:
      raise ScenarioError(f"{where}: until.file_field.field must be a non-empty dotted field")
    return {"file_field": condition}
  if kind == "at_gate":
    if not isinstance(condition, str) or not condition:
      raise ScenarioError(f"{where}: until.at_gate must be a non-empty string")
    return {"at_gate": condition}
  raise ScenarioError(f"{where}: unknown until condition {kind!r}")


def _fake_agent(raw: Any, path: pathlib.Path) -> dict:
  if not isinstance(raw, dict) or set(raw) != {"starts"} or not isinstance(raw["starts"], list):
    raise ScenarioError(f"{path}: fake_agent must contain a starts list")
  for start_index, start in enumerate(raw["starts"], start=1):
    where = f"{path.name} fake_agent start {start_index}"
    if not isinstance(start, dict) or set(start) != {"name", "steps", "exit_code"}:
      raise ScenarioError(f"{where}: requires name, steps and exit_code")
    if not isinstance(start["name"], str) or not start["name"]:
      raise ScenarioError(f"{where}: name must be a non-empty string")
    if not isinstance(start["steps"], list):
      raise ScenarioError(f"{where}: steps must be a list")
    if not isinstance(start["exit_code"], int) or isinstance(start["exit_code"],
                                                             bool) or not 0 <= start["exit_code"] <= 255:
      raise ScenarioError(f"{where}: exit_code must be an integer from 0 to 255")
    for step_index, step in enumerate(start["steps"], start=1):
      step_where = f"{where} step {step_index}"
      if not isinstance(step, dict) or len(step) != 1:
        raise ScenarioError(f"{step_where}: requires one emit or gate key")
      if "emit" in step and isinstance(step["emit"], dict):
        continue
      if "gate" in step and isinstance(step["gate"], str) and step["gate"]:
        continue
      raise ScenarioError(f"{step_where}: emit must be an object or gate must be a non-empty string")
  return raw


def load(path: pathlib.Path) -> Scenario:
  raw = yaml.safe_load(path.read_text(encoding="utf-8"))
  if not isinstance(raw, dict):
    raise ScenarioError(f"{path}: a scenario is a mapping")
  unknown = set(raw) - _SCENARIO_KEYS
  if unknown:
    raise ScenarioError(f"{path}: unknown keys {sorted(unknown)}")
  for required in ("stage", "name"):
    if not isinstance(raw.get(required), str) or not raw[required]:
      raise ScenarioError(f"{path}: {required!r} is required and must be a non-empty string")
  steps = raw.get("steps")
  if not isinstance(steps, list) or not steps:
    raise ScenarioError(f"{path}: 'steps' must be a non-empty list")
  parsed_steps = tuple(_step(step, f"{path.name} step {index + 1}") for index, step in enumerate(steps))
  has_handover = any(step.kind == "handover" for step in parsed_steps)
  has_script = "fake_agent" in raw
  has_expectation = "expect_starts" in raw
  if has_script != has_expectation:
    raise ScenarioError(f"{path}: fake_agent and expect_starts must be provided together")
  fake_agent = _fake_agent(raw["fake_agent"], path) if has_script else None
  expect_starts = raw.get("expect_starts")
  if has_expectation and (not isinstance(expect_starts, int) or isinstance(expect_starts, bool) or expect_starts < 0):
    raise ScenarioError(f"{path}: expect_starts must be a non-negative integer")
  has_directions = "directions" in raw
  directions_raw = raw.get("directions")
  directions = None
  if has_directions:
    if not isinstance(directions_raw, list) or not directions_raw:
      raise ScenarioError(f"{path}: directions must be a non-empty list")
    if any(not isinstance(direction, str) or direction not in HANDOVER_DIRECTIONS for direction in directions_raw):
      raise ScenarioError(f"{path}: directions must be a subset of {HANDOVER_DIRECTIONS}")
    if len(set(directions_raw)) != len(directions_raw):
      raise ScenarioError(f"{path}: directions must not contain duplicates")
    directions = tuple(directions_raw)
  if has_handover and directions is None:
    raise ScenarioError(f"{path}: directions are required when a handover step is present")
  if not has_handover and has_directions:
    raise ScenarioError(f"{path}: directions are refused when no handover step is present")
  if (has_handover or
      any(step.kind == "gate" or (step.until and "at_gate" in step.until) for step in parsed_steps)) and not has_script:
    raise ScenarioError(f"{path}: fake_agent is required for gate and handover steps")
  return Scenario(
      path=path,
      stage=raw["stage"],
      name=raw["name"],
      steps=parsed_steps,
      fake_agent=fake_agent,
      expect_starts=expect_starts,
      directions=directions,
  )


def load_all(stages: list[str] | None) -> list[Scenario]:
  """Every scenario under SCENARIO_DIR, in file-name order; only the given stages when *stages* is set."""
  scenarios = [load(path) for path in sorted(SCENARIO_DIR.glob("*.yaml"))]
  known = {scenario.stage for scenario in scenarios}
  if stages:
    missing = sorted(set(stages) - known)
    if missing:
      raise ScenarioError(f"no scenario has stage {missing}; known stages: {sorted(known)}")
    scenarios = [scenario for scenario in scenarios if scenario.stage in stages]
  return scenarios


def _lookup(ref: str, manifest: dict, captures: dict[str, Any]) -> str:
  parts = ref.split(".")
  value: Any = captures if parts[0] == "capture" else manifest
  for part in parts[1:] if parts[0] == "capture" else parts:
    if not isinstance(value, dict) or part not in value:
      raise ScenarioError(f"unresolved reference ${{{ref}}}")
    value = value[part]
  if isinstance(value, (dict, list)) or value is None:
    raise ScenarioError(f"reference ${{{ref}}} names a {type(value).__name__}, not a scalar")
  return str(value)


def _strings(value: Any) -> list[str]:
  if isinstance(value, str):
    return [value]
  if isinstance(value, list):
    return [text for item in value for text in _strings(item)]
  if isinstance(value, dict):
    return [text for item in value.values() for text in _strings(item)]
  return []


def check_references(scenario: Scenario, manifest: dict) -> None:
  """Every manifest reference resolves, and every capture reference names a capture of an earlier step."""
  captured: set[str] = set()
  for step in scenario.steps:
    texts = _strings([step.path, step.query, step.headers, step.json_body, step.until])
    for match in (match for text in texts for match in _REFERENCE.finditer(text)):
      ref = match["ref"]
      if ref.startswith("capture."):
        if ref.split(".", 1)[1] not in captured:
          raise ScenarioError(f"{scenario.path.name} step {step.name!r}: ${{{ref}}} names no earlier capture")
      else:
        _lookup(ref, manifest, {})
    captured.update(step.capture)


def substitute(value: Any, manifest: dict, captures: dict[str, Any]) -> Any:
  """*value* with every ``${...}`` reference resolved, inside strings, lists and mappings."""
  if isinstance(value, str):
    return _REFERENCE.sub(lambda match: _lookup(match["ref"], manifest, captures), value)
  if isinstance(value, list):
    return [substitute(item, manifest, captures) for item in value]
  if isinstance(value, dict):
    return {key: substitute(item, manifest, captures) for key, item in value.items()}
  return value


def extract(document: Any, field: str) -> Any:
  """The value at dotted *field* of a JSON *document* (a list index is a number segment)."""
  value = document
  for part in field.split("."):
    if isinstance(value, list) and part.isdigit() and int(part) < len(value):
      value = value[int(part)]
    elif isinstance(value, dict) and part in value:
      value = value[part]
    else:
      raise ScenarioError(f"capture field {field!r} is absent from the response")
  return value

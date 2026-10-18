"""Scenario files: YAML documents under scenarios/, each tagged with the port stage its routes belong to.

A scenario names its stage and lists its steps. Each step has a ``kind``; ``http`` (the default) sends one
request to both servers. The kinds ``ws``, ``wait`` and ``handover`` are reserved for later stages and are
refused until a stage implements them.

Strings in a step may reference the fixture manifest as ``${sessions.atlas}``, ``${runs.notes_r2.run}`` or
``${missing_session}`` (``${missing_run}``), and an earlier step's capture as ``${capture.name}``. A step's route template is its
method and path with every reference replaced by a placeholder (``{session_id}``, ``{run_id}``, ...); the
report groups steps by it. A capture reads a value out of each server's own JSON response, so a cursor one
server issued goes back to that server.
"""

import dataclasses
import pathlib
import re
from typing import Any

import yaml

SCENARIO_DIR = pathlib.Path(__file__).resolve().parent / "scenarios"
STEP_KINDS = ("http",)
RESERVED_KINDS = ("ws", "wait", "handover")
AUTH_MODES = ("bearer", "cookie", "none")
METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS")

_REFERENCE = re.compile(r"\$\{(?P<ref>[A-Za-z0-9_.]+)\}")
_SCENARIO_KEYS = {"stage", "name", "description", "steps"}
_STEP_KEYS = {"kind", "name", "method", "path", "query", "headers", "json", "auth", "expect_status", "capture"}


class ScenarioError(ValueError):
  """A scenario file that does not follow the format."""


@dataclasses.dataclass(frozen=True)
class Step:
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

  @property
  def route(self) -> str:
    """The route template: method and path with each reference as its placeholder."""
    return f"{self.method} {_REFERENCE.sub(lambda match: _placeholder(match['ref']), self.path)}"


@dataclasses.dataclass(frozen=True)
class Scenario:
  path: pathlib.Path
  stage: str
  name: str
  steps: tuple[Step, ...]


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
  kind = raw.get("kind", "http")
  if kind in RESERVED_KINDS:
    raise ScenarioError(f"{where}: step kind {kind!r} is reserved for a later stage and not implemented yet")
  if kind not in STEP_KINDS:
    raise ScenarioError(f"{where}: unknown step kind {kind!r}")
  unknown = set(raw) - _STEP_KEYS
  if unknown:
    raise ScenarioError(f"{where}: unknown keys {sorted(unknown)}")
  for required in ("name", "method", "path"):
    if not isinstance(raw.get(required), str) or not raw[required]:
      raise ScenarioError(f"{where}: {required!r} is required and must be a non-empty string")
  method = raw["method"].upper()
  if method not in METHODS:
    raise ScenarioError(f"{where}: unknown method {raw['method']!r}")
  if not raw["path"].startswith("/") or "?" in raw["path"]:
    raise ScenarioError(f"{where}: path must start with / and carry no query (use 'query')")
  auth = raw.get("auth", "bearer")
  if auth not in AUTH_MODES:
    raise ScenarioError(f"{where}: auth must be one of {AUTH_MODES}")
  expect_status = raw.get("expect_status")
  if expect_status is not None and not isinstance(expect_status, int):
    raise ScenarioError(f"{where}: expect_status must be an integer")
  return Step(
      name=raw["name"],
      method=method,
      path=raw["path"],
      query=_string_map(raw.get("query", {}), f"{where}.query"),
      headers=_string_map(raw.get("headers", {}), f"{where}.headers"),
      json_body=raw.get("json"),
      has_json="json" in raw,
      auth=auth,
      expect_status=expect_status,
      capture=_string_map(raw.get("capture", {}), f"{where}.capture"),
  )


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
  return Scenario(
      path=path,
      stage=raw["stage"],
      name=raw["name"],
      steps=tuple(_step(step, f"{path.name} step {index + 1}") for index, step in enumerate(steps)),
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
    texts = _strings([step.path, step.query, step.headers, step.json_body])
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

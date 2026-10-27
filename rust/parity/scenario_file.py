"""Scenario files define the inputs and comparisons that make one server route reproducible.

HTTP and WebSocket steps drive both servers. Fake steps inject provider events, while wait, gate and handover
steps coordinate the scripted agent. A log_event step waits for an event each server writes to its log. References
name fixture values, side-local tree paths and prior captures.
"""

import contextlib
import dataclasses
import importlib
import math
import pathlib
import re
from typing import Any

import yaml

SCENARIO_DIR = pathlib.Path(__file__).resolve().parent / "scenarios"
DATA_DIR = pathlib.Path(__file__).resolve().parent / "data"
FIXTURE_EXTRAS_DIR = pathlib.Path(__file__).resolve().parent / "fixture_extras"
STEP_KINDS = ("http", "ws", "fake", "wait", "log_event", "gate", "handover", "process")
AUTH_MODES = ("bearer", "cookie", "none", "agent")
WS_AUTH_MODES = ("token", "none")
METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS")
HANDOVER_DIRECTIONS = ("python-to-candidate", "candidate-to-python")
FAKE_NAMES = ("slack", "discord", "gemini", "muse", "openai_compatible")
FAKE_ACTIONS = {"slack": ("socket_event",), "discord": ("dispatch",)}
WS_COMPARE_MODES = ("frames",)
HTTP_COMPARE_MODES = ("full", "status_and_shape")
SIGTERM_DISPOSITIONS = ("ignore", "default")
MULTIPART_BOUNDARY = "charliebot-parity-multipart-boundary-1792318577"

_REFERENCE = re.compile(r"\$\{(?P<ref>[A-Za-z0-9_.]+)\}")
_SCENARIO_KEYS = {
    "stage", "name", "description", "steps", "fake_agent", "expect_starts", "directions", "config", "credentials",
    "fakes", "fixture_extras", "headless_chrome", "comparison_rules"
}
_HTTP_STEP_KEYS = {
    "kind", "name", "method", "path", "query", "headers", "json", "body_file", "multipart", "auth", "agent",
    "expect_status", "compare", "capture", "defer_files"
}
_WS_STEP_KEYS = {
    "kind", "name", "path", "query", "auth", "send", "collect", "until", "timeout_seconds", "compare", "expect_reject",
    "wait_for_fake_handshake"
}
_FAKE_STEP_KEYS = {"kind", "name", "fake", "action", "payload"}
_WAIT_STEP_KEYS = {"kind", "name", "timeout_seconds", "until"}
_LOG_EVENT_STEP_KEYS = {"kind", "name", "event", "fields", "timeout_seconds"}
_GATE_STEP_KEYS = {"kind", "name", "open"}
_HANDOVER_STEP_KEYS = {"kind", "name", "agent_after_stop", "open_while_stopped", "timeout_seconds"}
_PROCESS_STEP_KEYS = {"kind", "name", "action", "process_id", "delay_seconds", "capture"}


class ScenarioError(ValueError):
  """A scenario file that does not follow the format."""


class UnresolvedReferenceError(ScenarioError):
  """A manifest or capture value this run does not hold. It is a property of the step that names it, not of the file."""

  def __init__(self, ref: str) -> None:
    super().__init__(f"unresolved reference ${{{ref}}}")
    self.capture = ref.split(".", 1)[1] if ref.startswith("capture.") else None


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
  body_file: str | None
  multipart: dict | None
  auth: str
  agent: dict | None
  expect_status: int | None
  http_compare: str | None
  capture: dict[str, str]
  defer_files: bool
  timeout_seconds: float | None
  until: dict | None
  open: str | None
  agent_after_stop: str | None
  open_while_stopped: str | None
  send: tuple[dict, ...]
  collect: dict | None
  compare: str | dict
  expect_reject: int | None
  wait_for_fake_handshake: str | None
  fake: str | None
  action: str | None
  payload: Any
  log_event: str | None
  log_fields: dict[str, str]

  @property
  def route(self) -> str:
    """The route template: method and path with each reference as its placeholder."""
    if self.kind == "http":
      return f"{self.method} {_REFERENCE.sub(lambda match: _placeholder(match['ref']), self.path)}"
    if self.kind == "ws":
      return f"WS {_REFERENCE.sub(lambda match: _placeholder(match['ref']), self.path)}"
    if self.kind == "fake":
      return f"FAKE {self.fake} {self.action}"
    return f"({self.kind})"


@dataclasses.dataclass(frozen=True)
class Scenario:
  path: pathlib.Path
  stage: str
  name: str
  steps: tuple[Step, ...]
  fake_agent: dict | None
  expect_starts: int | None
  directions: tuple[str, ...] | None
  config: dict
  credentials: dict
  fakes: dict[str, dict]
  fixture_extras: tuple[str, ...]
  headless_chrome: bool
  comparison_rules: tuple[dict[str, Any], ...]


def _placeholder(ref: str) -> str:
  parts = ref.split(".")
  if parts[0] == "tree":
    if len(parts) != 2 or parts[1] not in ("root", "home"):
      raise ScenarioError(f"unknown tree reference ${{{ref}}}")
    return "{tree}" if parts[1] == "root" else "{home}"
  if parts[0] == "extras":
    return "{" + ".".join(parts[1:]) + "}"
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


def _overlay_value(value: Any, where: str) -> Any:
  if isinstance(value, dict):
    if any(not isinstance(key, str) for key in value):
      raise ScenarioError(f"{where} mapping keys must be strings")
    return {key: _overlay_value(item, f"{where}.{key}") for key, item in value.items()}
  if isinstance(value, list):
    return [_overlay_value(item, f"{where}[]") for item in value]
  if value is None or isinstance(value, (str, bool, int)):
    return value
  if isinstance(value, float) and math.isfinite(value):
    return value
  raise ScenarioError(f"{where} must contain YAML scalar, list or mapping values")


def _overlay_mapping(value: Any, where: str) -> dict:
  if not isinstance(value, dict):
    raise ScenarioError(f"{where} must be a mapping")
  return _overlay_value(value, where)


def _data_file(value: Any, where: str) -> str:
  if (not isinstance(value, str) or not value or pathlib.PurePosixPath(value).is_absolute() or
      ".." in pathlib.PurePosixPath(value).parts):
    raise ScenarioError(f"{where} must be a file path under rust/parity/data")
  path = DATA_DIR / value
  if not path.is_file():
    raise ScenarioError(f"{where} names missing data file {value!r}")
  return value


def _positive_timeout(value: Any, where: str) -> float:
  valid_type = isinstance(value, (int, float)) and not isinstance(value, bool)
  try:
    finite = valid_type and math.isfinite(value)
  except OverflowError:
    finite = False
  if not finite or value <= 0:
    raise ScenarioError(f"{where} must be positive")
  return float(value)


def _collect(raw: Any, where: str) -> dict:
  if not isinstance(raw, dict) or set(raw) != {"where", "field", "decode"}:
    raise ScenarioError(f"{where}.collect requires where, field and decode")
  if not isinstance(raw["where"], dict) or any(not isinstance(key, str) for key in raw["where"]):
    raise ScenarioError(f"{where}.collect.where must map string fields to JSON values")
  if not isinstance(raw["field"], str) or not raw["field"]:
    raise ScenarioError(f"{where}.collect.field must be a non-empty dotted path")
  if raw["decode"] not in ("base64", "none"):
    raise ScenarioError(f"{where}.collect.decode must be base64 or none")
  return raw


def _ws_until(raw: Any, where: str) -> dict:
  if not isinstance(raw, dict) or len(raw) != 1:
    raise ScenarioError(f"{where}.until must contain one condition")
  kind, value = next(iter(raw.items()))
  if kind == "frames":
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
      raise ScenarioError(f"{where}.until.frames must be a non-negative integer")
  elif kind == "close":
    if value is not True:
      raise ScenarioError(f"{where}.until.close must be true")
  elif kind == "collected_contains":
    if not isinstance(value, str) or not value:
      raise ScenarioError(f"{where}.until.collected_contains must be a non-empty string")
  else:
    raise ScenarioError(f"{where}.until has unknown condition {kind!r}")
  return {kind: value}


def _ws_send(raw: Any, where: str) -> tuple[dict, ...]:
  if not isinstance(raw, list):
    raise ScenarioError(f"{where}.send must be a list")
  frames = []
  for index, frame in enumerate(raw, start=1):
    frame_where = f"{where}.send frame {index}"
    if not isinstance(frame, dict) or len(frame) != 1:
      raise ScenarioError(f"{frame_where} requires one json, text or binary_file key")
    kind, value = next(iter(frame.items()))
    if kind == "json":
      frames.append({kind: _overlay_value(value, frame_where)})
    elif kind == "text" and isinstance(value, str):
      frames.append({kind: value})
    elif kind == "binary_file":
      frames.append({kind: _data_file(value, frame_where)})
    else:
      raise ScenarioError(f"{frame_where} must be json, text or binary_file")
  return tuple(frames)


def _multipart(raw: Any, where: str) -> dict:
  if not isinstance(raw, dict) or not raw:
    raise ScenarioError(f"{where}.multipart must be a non-empty mapping")
  fields = {}
  for name, value in raw.items():
    field_where = f"{where}.multipart.{name}"
    if not isinstance(name, str) or not name:
      raise ScenarioError(f"{where}.multipart field names must be non-empty strings")
    if isinstance(value, dict):
      content_type = value.get("content_type")
      if set(value) not in ({"file", "content_type"}, {"file", "content_type", "filename"
                                                      }) or not isinstance(content_type, str) or not content_type:
        raise ScenarioError(f"{field_where} file value requires file and content_type")
      if "filename" in value and not isinstance(value["filename"], str):
        raise ScenarioError(f"{field_where}.filename must be a string")
      fields[name] = {"file": _data_file(value["file"], field_where), "content_type": value["content_type"]}
      if "filename" in value:
        fields[name]["filename"] = value["filename"]
    elif value is None or isinstance(value, (str, int, float, bool)):
      fields[name] = value
    else:
      raise ScenarioError(f"{field_where} must be a scalar or a file mapping")
  return fields


def _fake_seeds(raw: Any, path: pathlib.Path) -> dict[str, dict]:
  if not isinstance(raw, dict):
    raise ScenarioError(f"{path.name}.fakes must map fake names to seed mappings")
  seeds = {}
  for name, seed in raw.items():
    if not isinstance(name, str) or name not in FAKE_NAMES:
      raise ScenarioError(f"{path.name}.fakes has unknown fake {name!r}; expected {FAKE_NAMES}")
    if not isinstance(seed, dict):
      raise ScenarioError(f"{path.name}.fakes.{name} must be a seed mapping")
    module_name = f".fakes.{name}" if __package__ else f"fakes.{name}"
    fake_module = importlib.import_module(module_name, __package__ or None)
    try:
      fake_module.validate_seed(seed)
    except ValueError as error:
      raise ScenarioError(f"{path.name}.fakes.{name}: {error}") from error
    seeds[name] = _overlay_mapping(seed, f"{path.name}.fakes.{name}")
  return seeds


def _fixture_extras(raw: Any, path: pathlib.Path) -> tuple[str, ...]:
  if not isinstance(raw, list):
    raise ScenarioError(f"{path.name}.fixture_extras must be a list")
  names = []
  for name in raw:
    if not isinstance(name, str) or re.fullmatch(r"[a-z][a-z0-9_]*", name) is None:
      raise ScenarioError(f"{path.name}.fixture_extras entries must be module names")
    if not (FIXTURE_EXTRAS_DIR / f"{name}.py").is_file():
      raise ScenarioError(f"{path.name}.fixture_extras names missing module {name!r}")
    if name in names:
      raise ScenarioError(f"{path.name}.fixture_extras must not repeat {name!r}")
    names.append(name)
  return tuple(names)


EVENT_FIELD_RULE_NAME = "allowed_baseline_values_for_one_event_field"
ADJACENT_ORDER_RULE_NAME = "either_order_of_adjacent_events"
EVENT_STREAM_RULE_NAME = "compare_event_type_streams"
COMPARISON_RULE_NAMES = (EVENT_FIELD_RULE_NAME, ADJACENT_ORDER_RULE_NAME, EVENT_STREAM_RULE_NAME)


def _event_selector(raw: Any, where: str) -> dict[str, Any]:
  if not isinstance(raw, dict) or set(raw) != {"type", "match_fields"}:
    raise ScenarioError(f"{where} requires type and match_fields")
  if not isinstance(raw["type"], str) or not raw["type"]:
    raise ScenarioError(f"{where}.type must be a non-empty string")
  match_fields = raw["match_fields"]
  if not isinstance(match_fields, dict) or not match_fields or any(
      not isinstance(key, str) or not key for key in match_fields):
    raise ScenarioError(f"{where}.match_fields must map non-empty field names to JSON values")
  return {"type": raw["type"], "match_fields": _overlay_mapping(match_fields, f"{where}.match_fields")}


def _adjacent_order_rule(raw: Any, where: str) -> dict[str, Any]:
  if not isinstance(raw, dict) or set(raw) != {"first", "second"}:
    raise ScenarioError(f"{where} requires first and second")
  return {key: _event_selector(raw[key], f"{where}.{key}") for key in ("first", "second")}


def _comparison_rules(raw: Any, path: pathlib.Path) -> tuple[dict[str, Any], ...]:
  if raw is None:
    return ()
  if not isinstance(raw, list):
    raise ScenarioError(f"{path.name}.comparison_rules must be a list")
  rules = []
  for index, item in enumerate(raw, start=1):
    where = f"{path.name}.comparison_rules[{index}]"
    if not isinstance(item, dict) or len(item) != 1 or next(iter(item)) not in COMPARISON_RULE_NAMES:
      raise ScenarioError(f"{where} must name one supported rule: {', '.join(COMPARISON_RULE_NAMES)}")
    rule_name, rule = next(iter(item.items()))
    if rule_name == ADJACENT_ORDER_RULE_NAME:
      rules.append({rule_name: _adjacent_order_rule(rule, f"{where}.{rule_name}")})
      continue
    if rule_name == EVENT_STREAM_RULE_NAME:
      stream_where = f"{where}.{rule_name}"
      if not isinstance(rule, dict) or set(rule) != {"session", "event_types"}:
        raise ScenarioError(f"{stream_where} requires session and event_types")
      session = rule["session"]
      if not isinstance(session, str) or not session:
        raise ScenarioError(f"{stream_where}.session must be a non-empty string")
      event_types = rule["event_types"]
      if (not isinstance(event_types, list) or not event_types or
          any(not isinstance(event_type, str) or not event_type for event_type in event_types)):
        raise ScenarioError(f"{stream_where}.event_types must be a non-empty list of event type names")
      if len(set(event_types)) != len(event_types):
        raise ScenarioError(f"{stream_where}.event_types must not repeat a type")
      rules.append({rule_name: {"session": session, "event_types": list(event_types)}})
      continue
    required = {"event", "field", "allowed_baseline_values", "required_candidate_value"}
    if not isinstance(rule, dict) or set(rule) != required:
      raise ScenarioError(
          f"{where}.{rule_name} requires event, field, allowed_baseline_values and required_candidate_value")
    event = _event_selector(rule["event"], f"{where}.{rule_name}.event")
    field = rule["field"]
    if not isinstance(field, str) or not field or field in event["match_fields"]:
      raise ScenarioError(f"{where}.{rule_name}.field must name one event field outside match_fields")
    allowed_values = rule["allowed_baseline_values"]
    if not isinstance(allowed_values, list) or not allowed_values:
      raise ScenarioError(f"{where}.{rule_name}.allowed_baseline_values must be a non-empty list")
    allowed_values = _overlay_value(allowed_values, f"{where}.{rule_name}.allowed_baseline_values")
    required_candidate_value = _overlay_value(
        rule["required_candidate_value"], f"{where}.{rule_name}.required_candidate_value")
    rules.append(
        {
            rule_name:
                {
                    "event": event,
                    "field": field,
                    "allowed_baseline_values": allowed_values,
                    "required_candidate_value": required_candidate_value,
                }
        })
  return tuple(rules)


def _step(raw: Any, where: str) -> Step:
  if not isinstance(raw, dict):
    raise ScenarioError(f"{where} must be a mapping")
  if isinstance(raw.get("name"), str) and raw["name"]:
    where = f"{where} {raw['name']!r}"
  kind = raw.get("kind", "http")
  if not isinstance(kind, str):
    raise ScenarioError(f"{where}: step kind must be a string")
  if kind not in STEP_KINDS:
    raise ScenarioError(f"{where}: unknown step kind {kind!r}")
  allowed = {
      "http": _HTTP_STEP_KEYS,
      "ws": _WS_STEP_KEYS,
      "fake": _FAKE_STEP_KEYS,
      "wait": _WAIT_STEP_KEYS,
      "log_event": _LOG_EVENT_STEP_KEYS,
      "gate": _GATE_STEP_KEYS,
      "handover": _HANDOVER_STEP_KEYS,
      "process": _PROCESS_STEP_KEYS,
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
  body_file = None
  multipart_fields = None
  auth = "none"
  expect_status = None
  http_compare = None
  capture: dict[str, str] = {}
  defer_files = False
  timeout_seconds = None
  until = None
  open_name = None
  agent = None
  agent_after_stop = None
  open_while_stopped = None
  send: tuple[dict, ...] = ()
  collect = None
  compare_mode: str | dict = "frames"
  expect_reject = None
  wait_for_fake_handshake = None
  fake_name = None
  action = None
  payload = None
  log_event: str | None = None
  log_fields: dict[str, str] = {}
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
    agent = _agent_claims(raw["agent"], where) if "agent" in raw else None
    if (auth == "agent") != (agent is not None):
      raise ScenarioError(f"{where}: agent is required with auth: agent, and only then")
    expect_status = raw.get("expect_status")
    if expect_status is not None and (not isinstance(expect_status, int) or isinstance(expect_status, bool)):
      raise ScenarioError(f"{where}: expect_status must be an integer")
    if "compare" in raw:
      http_compare = raw["compare"]
      if http_compare != "status_and_shape":
        raise ScenarioError(f"{where}.compare must be status_and_shape")
      if expect_status is None or expect_status < 400:
        raise ScenarioError(f"{where}.compare requires an expect_status of at least 400")
    defer_files = raw.get("defer_files", False)
    if not isinstance(defer_files, bool):
      raise ScenarioError(f"{where}: defer_files must be a boolean")
    query = _string_map(raw.get("query", {}), f"{where}.query")
    headers = _string_map(raw.get("headers", {}), f"{where}.headers")
    json_body = raw.get("json")
    has_json = "json" in raw
    body_kinds = [key for key in ("json", "body_file", "multipart") if key in raw]
    if len(body_kinds) > 1:
      raise ScenarioError(f"{where}: choose at most one of json, body_file and multipart")
    if "body_file" in raw:
      body_file = _data_file(raw["body_file"], f"{where}.body_file")
    if "multipart" in raw:
      multipart_fields = _multipart(raw["multipart"], where)
    capture = _string_map(raw.get("capture", {}), f"{where}.capture")
    compare_mode = raw.get("compare", "full")
    if compare_mode not in HTTP_COMPARE_MODES:
      raise ScenarioError(f"{where}.compare must be one of {HTTP_COMPARE_MODES}")
    if compare_mode == "status_and_shape" and (expect_status is None or expect_status < 400):
      raise ScenarioError(f"{where}.compare requires an expect_status of at least 400")
  elif kind == "ws":
    path = raw.get("path", "")
    if not isinstance(path, str) or not path.startswith("/ws/") or "?" in path:
      raise ScenarioError(f"{where}: path must start with /ws/ and carry no query (use query)")
    query = _string_map(raw.get("query", {}), f"{where}.query")
    auth = raw.get("auth", "token")
    if auth not in WS_AUTH_MODES:
      raise ScenarioError(f"{where}: auth must be one of {WS_AUTH_MODES}")
    send = _ws_send(raw.get("send", []), where)
    collect = _collect(raw["collect"], where) if "collect" in raw else None
    until = _ws_until(raw.get("until"), where)
    timeout_seconds = _positive_timeout(raw.get("timeout_seconds"), f"{where}.timeout_seconds")
    wait_for_fake_handshake = raw.get("wait_for_fake_handshake")
    if wait_for_fake_handshake is not None and (not isinstance(wait_for_fake_handshake, str) or
                                                wait_for_fake_handshake not in FAKE_NAMES):
      raise ScenarioError(f"{where}.wait_for_fake_handshake must name a supported fake")
    compare_mode = raw.get("compare", "frames")
    if isinstance(compare_mode, dict):
      if set(compare_mode) != {"collected_contains"} or not isinstance(compare_mode["collected_contains"], list) or any(
          not isinstance(item, str) or not item for item in compare_mode["collected_contains"]):
        raise ScenarioError(f"{where}.compare must be frames or collected_contains with non-empty strings")
      if collect is None:
        raise ScenarioError(f"{where}.compare collected_contains requires collect")
    elif compare_mode not in WS_COMPARE_MODES:
      raise ScenarioError(f"{where}.compare must be frames or collected_contains")
    if "collected_contains" in until and collect is None:
      raise ScenarioError(f"{where}.until collected_contains requires collect")
    if "expect_reject" in raw:
      expect_reject = raw["expect_reject"]
      if not isinstance(expect_reject, int) or isinstance(expect_reject, bool) or not 400 <= expect_reject <= 599:
        raise ScenarioError(f"{where}.expect_reject must be an HTTP error status")
      if send:
        raise ScenarioError(f"{where}: expect_reject cannot send frames")
      if wait_for_fake_handshake is not None:
        raise ScenarioError(f"{where}: expect_reject cannot wait for a fake handshake")
  elif kind == "fake":
    fake_name = raw.get("fake")
    action = raw.get("action")
    if not isinstance(fake_name, str) or fake_name not in FAKE_ACTIONS or action not in FAKE_ACTIONS.get(fake_name, ()):
      raise ScenarioError(f"{where}: fake and action must name a supported inbound action")
    payload = raw.get("payload")
    if not isinstance(payload, dict):
      raise ScenarioError(f"{where}: payload must be a mapping")
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
  elif kind == "log_event":
    event = raw.get("event")
    if not isinstance(event, str) or not event:
      raise ScenarioError(f"{where}: 'event' is required and must be a non-empty string")
    log_event = event
    log_fields = _string_map(raw.get("fields", {}), f"{where}.fields")
    timeout_seconds = _positive_timeout(raw.get("timeout_seconds"), f"{where}.timeout_seconds")
  elif kind == "gate":
    open_name = raw.get("open")
    if not isinstance(open_name, str) or not open_name:
      raise ScenarioError(f"{where}: 'open' is required and must be a non-empty string")
  elif kind == "handover":
    agent_after_stop = raw.get("agent_after_stop")
    if agent_after_stop not in ("alive", "exited", "terminated"):
      raise ScenarioError(f"{where}: 'agent_after_stop' must be alive, exited or terminated")
    if "open_while_stopped" in raw:
      open_while_stopped = raw["open_while_stopped"]
      if not isinstance(open_while_stopped, str) or not open_while_stopped:
        raise ScenarioError(f"{where}: 'open_while_stopped' must be a non-empty string")
      if "timeout_seconds" not in raw:
        raise ScenarioError(f"{where}: 'timeout_seconds' is required with open_while_stopped")
      timeout_seconds = _positive_timeout(raw["timeout_seconds"], f"{where}.timeout_seconds")
    elif "timeout_seconds" in raw:
      raise ScenarioError(f"{where}: 'timeout_seconds' applies only with open_while_stopped")
  elif kind == "process":
    action = raw.get("action")
    if action not in ("start", "stop"):
      raise ScenarioError(f"{where}: process action must be start or stop")
    process_id = raw.get("process_id")
    if not isinstance(process_id, str) or not process_id:
      raise ScenarioError(f"{where}: process_id must be a non-empty string")
    delay_seconds = raw.get("delay_seconds", 0)
    valid_delay_type = isinstance(delay_seconds, (int, float)) and not isinstance(delay_seconds, bool)
    try:
      valid_delay = valid_delay_type and math.isfinite(delay_seconds) and delay_seconds >= 0
    except OverflowError:
      valid_delay = False
    if not valid_delay or (action == "start" and delay_seconds != 0):
      raise ScenarioError(f"{where}: delay_seconds must be nonnegative and only applies to stop")
    timeout_seconds = float(delay_seconds)
    payload = {"process_id": process_id}
    if action == "start":
      capture = _string_map(raw.get("capture", {}), f"{where}.capture")
      if len(capture) != 1 or next(iter(capture.values())) != "pid":
        raise ScenarioError(f"{where}: start requires one capture mapping to 'pid'")
    elif raw.get("capture", {}):
      raise ScenarioError(f"{where}: stop cannot capture process fields")
  return Step(
      kind=kind,
      name=raw["name"],
      method=method,
      path=path,
      query=query,
      headers=headers,
      json_body=json_body,
      has_json=has_json,
      body_file=body_file,
      multipart=multipart_fields,
      auth=auth,
      agent=agent,
      expect_status=expect_status,
      http_compare=http_compare,
      capture=capture,
      defer_files=defer_files,
      timeout_seconds=timeout_seconds,
      until=until,
      open=open_name,
      agent_after_stop=agent_after_stop,
      open_while_stopped=open_while_stopped,
      send=send,
      collect=collect,
      compare=compare_mode,
      expect_reject=expect_reject,
      wait_for_fake_handshake=wait_for_fake_handshake,
      fake=fake_name,
      action=action,
      payload=payload,
      log_event=log_event,
      log_fields=log_fields,
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
  if kind == "tree_path_missing":
    if not isinstance(condition, dict) or set(condition) != {"path"}:
      raise ScenarioError(f"{where}: until.tree_path_missing requires path")
    path = condition["path"]
    if not isinstance(path, str) or not path or pathlib.PurePosixPath(path).is_absolute() or ".." in path.split("/"):
      raise ScenarioError(f"{where}: until.tree_path_missing.path must stay under the scenario tree")
    return {"tree_path_missing": condition}
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
        raise ScenarioError(f"{step_where}: requires one emit, gate or sigterm key")
      if "emit" in step and isinstance(step["emit"], dict):
        continue
      if "gate" in step and isinstance(step["gate"], str) and step["gate"]:
        continue
      if "sigterm" in step and step["sigterm"] in SIGTERM_DISPOSITIONS:
        continue
      raise ScenarioError(
          f"{step_where}: emit must be an object or gate must be a non-empty string; "
          f"sigterm must be one of {SIGTERM_DISPOSITIONS}")
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
  config_overlay = _overlay_mapping(raw.get("config", {}), f"{path.name}.config")
  credentials_overlay = _overlay_mapping(raw.get("credentials", {}), f"{path.name}.credentials")
  if "charliebot" in credentials_overlay:
    raise ScenarioError(f"{path.name}.credentials refuses the charliebot key")
  for overlay_name, overlay in (("config", config_overlay), ("credentials", credentials_overlay)):
    for text in _strings(overlay):
      for match in _REFERENCE.finditer(text):
        if match["ref"] not in ("tree.root", "tree.home"):
          raise ScenarioError(f"{path.name}.{overlay_name} only accepts ${{tree.root}} and ${{tree.home}} references")
  fakes = _fake_seeds(raw.get("fakes", {}), path)
  fixture_extras = _fixture_extras(raw.get("fixture_extras", []), path)
  comparison_rules = _comparison_rules(raw.get("comparison_rules"), path)
  headless_chrome = raw.get("headless_chrome", False)
  if not isinstance(headless_chrome, bool):
    raise ScenarioError(f"{path.name}.headless_chrome must be a boolean")
  handovers = [step for step in parsed_steps if step.kind == "handover"]
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
  if handovers and directions is None:
    raise ScenarioError(f"{path}: step {handovers[0].name!r} is a handover, so directions are required")
  if not handovers and has_directions:
    raise ScenarioError(f"{path}: directions are refused when no handover step is present")
  needs_script = [
      step for step in parsed_steps if step.kind in ("gate", "handover") or (step.until and "at_gate" in step.until)
  ]
  if needs_script and not has_script:
    raise ScenarioError(f"{path}: step {needs_script[0].name!r} needs a fake_agent script")
  declared_gates = {
      script_step["gate"]
      for start in (fake_agent["starts"] if fake_agent is not None else [])
      for script_step in start["steps"]
      if "gate" in script_step
  }
  for handover in (step for step in parsed_steps if step.kind == "handover" and step.open_while_stopped is not None):
    if handover.open_while_stopped not in declared_gates:
      raise ScenarioError(
          f"{path}: step {handover.name!r} opens gate {handover.open_while_stopped!r}, which no fake_agent start "
          "declares")
  for step in parsed_steps:
    if step.kind == "fake" and step.fake not in fakes:
      raise ScenarioError(f"{path.name} step {step.name!r}: fake {step.fake!r} is not declared in fakes")
    if step.wait_for_fake_handshake is not None and step.wait_for_fake_handshake not in fakes:
      raise ScenarioError(
          f"{path.name} step {step.name!r}: fake {step.wait_for_fake_handshake!r} is not declared in fakes")
  return Scenario(
      path=path,
      stage=raw["stage"],
      name=raw["name"],
      steps=parsed_steps,
      fake_agent=fake_agent,
      expect_starts=expect_starts,
      directions=directions,
      config=config_overlay,
      credentials=credentials_overlay,
      fakes=fakes,
      fixture_extras=fixture_extras,
      headless_chrome=headless_chrome,
      comparison_rules=comparison_rules,
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


def _lookup(ref: str, manifest: dict, captures: dict[str, Any], tree_root: str, tree_home: str) -> str:
  parts = ref.split(".")
  if parts[0] == "tree":
    values = {"root": tree_root, "home": tree_home}
    if len(parts) != 2 or parts[1] not in values:
      raise ScenarioError(f"unknown reference ${{{ref}}}")
    return values[parts[1]]
  value: Any = captures if parts[0] == "capture" else manifest
  for part in parts[1:] if parts[0] == "capture" else parts:
    if not isinstance(value, dict) or part not in value:
      raise UnresolvedReferenceError(ref)
    value = value[part]
  if isinstance(value, (dict, list)) or value is None:
    raise ScenarioError(f"reference ${{{ref}}} names a {type(value).__name__}, not a scalar")
  return str(value)


def _strings(value: Any) -> list[str]:
  if isinstance(value, str):
    return [value]
  if isinstance(value, (list, tuple)):
    return [text for item in value for text in _strings(item)]
  if isinstance(value, dict):
    return [text for item in value.values() for text in _strings(item)]
  return []


def check_references(scenario: Scenario, manifest: dict) -> None:
  """Every capture reference names a capture of an earlier step, and every reference is well formed.

  A manifest reference the manifest lacks is not an error of the file: unresolved_reference reports it on the step
  that holds it, when that step runs.
  """
  captured: set[str] = set()
  for step in scenario.steps:
    texts = _strings(
        [
            step.path, step.query, step.headers, step.json_body, step.multipart, step.send, step.collect, step.until,
            step.payload
        ])
    for match in (match for text in texts for match in _REFERENCE.finditer(text)):
      ref = match["ref"]
      if ref.startswith("capture."):
        if ref.split(".", 1)[1] not in captured:
          raise ScenarioError(f"{scenario.path.name} step {step.name!r}: ${{{ref}}} names no earlier capture")
      elif ref.startswith("tree."):
        if ref not in ("tree.root", "tree.home"):
          raise ScenarioError(f"{scenario.path.name} step {step.name!r}: unknown tree reference ${{{ref}}}")
      else:
        with contextlib.suppress(UnresolvedReferenceError):  # the step that holds the reference fails when it runs
          _lookup(ref, manifest, {}, "", "")
    captured.update(step.capture)


def substitute(value: Any, manifest: dict, captures: dict[str, Any], tree_root: str, tree_home: str) -> Any:
  """*value* with every ``${...}`` reference resolved, inside strings, lists and mappings."""
  if isinstance(value, str):
    return _REFERENCE.sub(lambda match: _lookup(match["ref"], manifest, captures, tree_root, tree_home), value)
  if isinstance(value, list):
    return [substitute(item, manifest, captures, tree_root, tree_home) for item in value]
  if isinstance(value, dict):
    return {key: substitute(item, manifest, captures, tree_root, tree_home) for key, item in value.items()}
  return value


def _agent_claims(raw: Any, where: str) -> dict:
  """The run an agent token is signed for: its session and run ids, each a literal or a reference."""
  if not isinstance(raw, dict) or set(raw) != {"session", "run"}:
    raise ScenarioError(f"{where}: agent needs exactly the keys session and run")
  for key in ("session", "run"):
    if not isinstance(raw[key], str) or not raw[key]:
      raise ScenarioError(f"{where}.agent.{key} must be a non-empty string")
  return dict(raw)


def unresolved_reference(
    scenario: Scenario, step: Step, manifest: dict, captures: dict[str, Any], tree_root: str,
    tree_home: str) -> str | None:
  """The message of the first reference in *step*'s inputs that does not resolve here, or None when all of them do.

  A capture this side never recorded names the nearest earlier step that captures it: that step did not record
  it on this side. The inputs are the values a step's runner substitutes. Any other reference error still raises.
  """
  inputs = [
      step.path,
      step.query,
      step.headers,
      step.json_body,
      step.multipart,
      list(step.send),
      step.collect,
      step.until,
      step.payload,
      step.agent,
  ]
  try:
    substitute(inputs, manifest, captures, tree_root, tree_home)
  except UnresolvedReferenceError as error:
    if error.capture is None:
      return str(error)
    return f"{error}: step {_capture_producer(scenario, step, error.capture)!r} should have captured it"
  return None


def _capture_producer(scenario: Scenario, step: Step, name: str) -> str:
  """The name of the nearest step before *step* that captures *name*; check_references ensures one exists."""
  position = next(index for index, candidate in enumerate(scenario.steps) if candidate is step)
  return next(earlier.name for earlier in reversed(scenario.steps[:position]) if name in earlier.capture)


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

"""The first router program's decision: ask Jev whether a task is routine or design work.

``charliebot router`` answers one backend-choice question per process. CharlieBot starts it, writes one
JSON object on stdin and reads one back on stdout:

  {"role": "implement", "text": "<the work>", "candidates": [{"id": "claude-opus", "cooling_until": null}]}
  {"backends": ["claude-opus"], "reason": "design p=0.86"}

Design work gets the ``strong`` ids that are also candidates, in ``strong`` order; routine work gets an
empty list, which tells CharlieBot to keep using its pool. ``reason`` always reports the design
probability. ``<charliebot home>/router.yaml`` is read on every call, so an edit takes effect at the next
choice; the Jev key comes from the ``credentials.yaml`` section that ``jev.credential`` names.

The caller bounds one routing call at 25 s, which is why one attempt is bounded at 5 s and the single
retry after an HTTP 429 waits 10 s: two attempts plus the wait fit inside that budget.
"""

import json
import pathlib
import time
from typing import NamedTuple

import yaml

from src.infra import credentials, home, timeouts, yaml_utils

ROUTER_FILENAME = "router.yaml"

# A thread's text keeps only this many trailing characters: a thread grows without bound, Jev's input cap does not.
_THREAD_TEXT_LIMIT = 20_000

_DESIGN = "design"
_ROUTINE = "routine"
_DESIGN_THRESHOLD = 0.5


class RouterError(Exception):
  """A failure the CLI reports on stderr before exiting non-zero."""


class RouterConfig(NamedTuple):
  """One validated ``router.yaml``: where Jev lives, the rule text, and the strong backends in order."""
  url: str
  model: str
  credential: str
  instructions: str
  routine: str
  design: str
  strong: tuple[str, ...]


class RouteRequest(NamedTuple):
  """One decoded stdin object."""
  role: str
  text: str
  candidate_ids: tuple[str, ...]


def config_path() -> pathlib.Path:
  """The router's own configuration file, ``<charliebot home>/router.yaml``."""
  return home.charliebot_home_dir() / ROUTER_FILENAME


def _field_error(path: pathlib.Path, field: str, expectation: str) -> RouterError:
  return RouterError(f"{path}: {field} must be {expectation}")


def _required_text(root: dict, field: str, path: pathlib.Path) -> str:
  value = root.get(field)
  if not isinstance(value, str) or not value.strip():
    raise _field_error(path, field, "a non-empty string")
  return value.strip()


def _required_mapping(root: dict, field: str, path: pathlib.Path) -> dict:
  value = root.get(field)
  if not isinstance(value, dict):
    raise _field_error(path, field, "a mapping")
  return value


def _strong_backends(root: dict, path: pathlib.Path) -> tuple[str, ...]:
  value = root.get("strong")
  if not isinstance(value, list) or not value:
    raise _field_error(path, "strong", "a non-empty list of backend ids")
  strong: list[str] = []
  for index, backend in enumerate(value):
    if not isinstance(backend, str) or not backend.strip():
      raise _field_error(path, f"strong[{index}]", "a backend id")
    strong.append(backend.strip())
  return tuple(strong)


def load_config(path: pathlib.Path) -> RouterConfig:
  """Read and validate one ``router.yaml``; a missing file or a malformed field names the file and the field."""
  try:
    document = yaml_utils.load_yaml(path, default=None)
  except (OSError, yaml.YAMLError) as error:
    raise RouterError(f"{path}: {error}") from error
  if document is None:
    if not path.exists():
      raise RouterError(f"{path}: missing; the router reads its Jev address, rule text and strong list from it")
    raise _field_error(path, "the document", "a mapping with jev, instructions, routine, design and strong")
  if not isinstance(document, dict):
    raise _field_error(path, "the document", "a mapping with jev, instructions, routine, design and strong")
  jev = _required_mapping(document, "jev", path)
  return RouterConfig(
      url=_required_text(jev, "url", path),
      model=_required_text(jev, "model", path),
      credential=_required_text(jev, "credential", path),
      instructions=_required_text(document, "instructions", path),
      routine=_required_text(document, "routine", path),
      design=_required_text(document, "design", path),
      strong=_strong_backends(document, path),
  )


def read_request(raw: str) -> RouteRequest:
  """Decode one stdin object; a thread's text keeps only its last ``_THREAD_TEXT_LIMIT`` characters."""
  try:
    document = json.loads(raw)
  except json.JSONDecodeError as error:
    raise RouterError(f"stdin: not one JSON object: {error}") from error
  if not isinstance(document, dict):
    raise RouterError("stdin: the request must be one JSON object")
  role = document.get("role")
  if not isinstance(role, str) or not role.strip():
    raise RouterError("stdin.role: must be a non-empty string")
  text = document.get("text")
  if not isinstance(text, str):
    raise RouterError("stdin.text: must be a string")
  candidates = document.get("candidates")
  if not isinstance(candidates, list):
    raise RouterError("stdin.candidates: must be a list of {id, cooling_until} objects")
  candidate_ids: list[str] = []
  for index, candidate in enumerate(candidates):
    identifier = candidate.get("id") if isinstance(candidate, dict) else None
    if not isinstance(identifier, str) or not identifier.strip():
      raise RouterError(f"stdin.candidates[{index}].id: must be a non-empty string")
    candidate_ids.append(identifier)
  if role == "thread":
    text = text[-_THREAD_TEXT_LIMIT:]
  return RouteRequest(role=role, text=text, candidate_ids=tuple(candidate_ids))


def _jev_body(config: RouterConfig, text: str) -> dict:
  """Jev's choice request: one question named ``kind`` with the routine and design criteria."""
  return {
      "model": config.model,
      "state": text,
      "questions":
          {
              "kind":
                  {
                      "type": "choice",
                      "instructions": config.instructions,
                      "criteria": {
                          _ROUTINE: config.routine,
                          _DESIGN: config.design,
                      },
                  },
          },
  }


def _post(url: str, body: dict, api_key: str) -> dict:
  """POST one Jev request, waiting out a single HTTP 429; every other failure raises :class:`RouterError`."""
  import httpx

  headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"}
  try:
    with httpx.Client(timeout=timeouts.ROUTER_JEV_REQUEST_TIMEOUT) as client:
      response = client.post(url, json=body, headers=headers)
      if response.status_code == 429:
        time.sleep(timeouts.ROUTER_JEV_RATE_LIMIT_WAIT)
        response = client.post(url, json=body, headers=headers)
  except httpx.HTTPError as error:
    raise RouterError(f"{url}: request failed: {error}") from error
  if response.status_code != 200:
    raise RouterError(f"{url}: HTTP {response.status_code}: {response.text}")
  try:
    payload = response.json()
  except ValueError as error:
    raise RouterError(f"{url}: the response body is not JSON: {response.text}") from error
  if not isinstance(payload, dict):
    raise RouterError(f"{url}: the response is not one JSON object: {response.text}")
  return payload


def _answer_design_probability(url: str, payload: dict) -> float:
  """The ``answers.kind.probabilities.design`` number of one Jev response."""
  answer = payload.get("answers")
  question = answer.get("kind") if isinstance(answer, dict) else None
  probabilities = question.get("probabilities") if isinstance(question, dict) else None
  value = probabilities.get(_DESIGN) if isinstance(probabilities, dict) else None
  if isinstance(value, bool) or not isinstance(value, (int, float)):
    raise RouterError(f"{url}: the answer has no answers.kind.probabilities.design number: {json.dumps(payload)}")
  return float(value)


def design_probability(config: RouterConfig, text: str) -> float:
  """Ask Jev once (one 429 retry) how likely *text* is design work."""
  try:
    api_key = str(credentials.get_credentials().require(config.credential, "api_key"))
  except ValueError as error:
    raise RouterError(str(error)) from error
  return _answer_design_probability(config.url, _post(config.url, _jev_body(config, text), api_key))


def choose(config: RouterConfig, request: RouteRequest, probability: float) -> tuple[list[str], str]:
  """The backends this request gets, in order, and the one-line reason for them."""
  if probability <= _DESIGN_THRESHOLD:
    return [], f"{_ROUTINE} p={probability:.2f}"
  candidates = set(request.candidate_ids)
  return [backend for backend in config.strong if backend in candidates], f"{_DESIGN} p={probability:.2f}"


def route(raw: str) -> dict:
  """Answer one stdin request: ``{"backends": [...], "reason": "..."}``."""
  config = load_config(config_path())
  request = read_request(raw)
  backends, reason = choose(config, request, design_probability(config, request.text))
  return {"backends": backends, "reason": reason}

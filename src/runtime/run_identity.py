"""Run-identity refusal: the one active-Run predicate and its run-scoped CLI read.

The predicate and its two refusal reasons serve both identity consumers (the API
caller-identity dependency and the CLI's run-scoped query resolution). They live
 apart from :mod:`src.runtime.runs` because the CLI resolves the audience in a
fresh process: importing runs prices the pydantic model stack (~120 ms of the
run-scoped ``memory query`` wall), and the predicate reads three record fields
and one durable fact type — none of it needs a model at runtime.
"""

from __future__ import annotations

import json
import pathlib
import types
from typing import TYPE_CHECKING

from src.infra import event_types as ET
from src.infra import ndjson

if TYPE_CHECKING:
  from src.infra import models

RUN_IDENTITY_UNKNOWN_DETAIL = "run token does not reference an active run"
RUN_IDENTITY_NOT_LAUNCHED_DETAIL = "run token references a run that has not launched"

# The session metadata filename under sessions/<id>/ (runs.py re-exports it as
# METADATA_NAME); runs.py cannot be imported here for it — see the module docstring.
SESSION_METADATA_NAME = "metadata.json"


def run_identity_refusal(run: models.RunRecord | None, events: list[dict]) -> str | None:
  """Why *run* is not an active, launched Run for caller identity, or None.

    The one predicate both identity consumers share: the API caller-identity
    dependency and the CLI's run-scoped query resolution. A run token stands
    only for a registered Run without a terminal fact whose launch identity
    (pid, pid_start) is pinned.
    """
  if run is None:
    return RUN_IDENTITY_UNKNOWN_DETAIL
  for event in events:
    if event.get("type") == ET.RUN_FINISHED and event.get("run_id") == run.id:
      return RUN_IDENTITY_UNKNOWN_DETAIL
  if run.pid is None or run.pid_start is None:
    return RUN_IDENTITY_NOT_LAUNCHED_DETAIL
  return None


def read_run_identity_sync(path: pathlib.Path) -> tuple[str, int | None, str | None] | None:
  """The (id, pid, pid_start) fields the refusal predicate reads, or None when absent.

  Skips and raises exactly like ``RunStore.read_run_sync``: a missing file
  answers None (the caller's unknown-run refusal), an unreadable or id-less
  file raises the same ``RuntimeError``. The record schema itself is not
  re-validated here — ``RunRecord`` validation at the write path is the only
  producer contract, and re-running it would price the model stack this module
  keeps off the run-scoped CLI path.
  """
  if not path.is_file():
    return None
  try:
    record = json.loads(path.read_text(encoding="utf-8"))
    run_id = record["id"]
    if not isinstance(run_id, str):
      raise ValueError("run id must be a string")
  except (OSError, ValueError, KeyError, TypeError) as e:
    raise RuntimeError(f"run metadata unreadable at {path}: {e}") from e
  return run_id, record.get("pid"), record.get("pid_start")


def run_scoped_refusal(sessions_dir: pathlib.Path, session_id: str, run_id: str) -> str | None:
  """The caller-identity refusal for one run token, read without the model stack.

  The run-scoped CLI shape of :func:`run_identity_refusal`: the run record's
  identity fields come from :func:`read_run_identity_sync` and the terminal-fact
  scan parses only the lines a head-provable type filter keeps (the predicate
  reads ``run_finished`` facts alone; a line whose head proves another type
  never reaches the parser, and a foreign leading key — the control events'
  id-first form — falls through and parses). The verdict equals the full
  corpus's: every fact the predicate reads rides a line the filter keeps.
  """
  session_dir = sessions_dir / session_id
  record = read_run_identity_sync(session_dir / "data" / "runs" / run_id / "metadata.json")
  if record is None:
    return RUN_IDENTITY_UNKNOWN_DETAIL
  run_id_read, pid, pid_start = record
  run = types.SimpleNamespace(id=run_id_read, pid=pid, pid_start=pid_start)
  events = ndjson.parse_ndjson_events(
      session_dir / "data" / "chat_events.jsonl",
      log_event=ndjson.PARSE_SKIP_LOG_EVENT,
      log_fields={},
      parse_filter=ndjson.type_line_filter(frozenset({ET.RUN_FINISHED})))
  return run_identity_refusal(run, events)  # type: ignore[arg-type]

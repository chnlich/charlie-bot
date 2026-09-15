"""Session-task-tree migration: inventory, manifest, conversion, apply, rollback.

This module owns the migration portion of approved plan 1 v4 (sections 4.1/4.2,
the eleven-row migration table). The executable recipe is the CLI family
(``charliebot session-tree migrate --dry-run|--apply|--rollback``, see
:mod:`src.cli.session_tree`); this module holds every decision and every write.

What it converts, per the approved table:

1. Ordinary and old-PM sessions become manager nodes **with their original
   id**, root by default; conversations, plans, attachments, backend bindings,
   TUI kind, Slack origins and historical references stay in place.
2. Ordinary worker threads become child worker tasks with work Runs; stable
   ids derive from (owner session, thread id); original git/worktree/log
   references are kept at their recorded paths.
3. ``review_of`` chains become review Runs of the original worker; missing
   targets, cycles and inconsistent associations are unresolved.
4. Improve loops become one worker with ordered iteration Runs, associated via
   loop state + goal + branch + repo + time-window evidence (never a
   description prefix alone); the old loop state file stays the controller's
   configuration/progress source.
5. Cron firing chains become one worker with ordered scheduled_step Runs
   keyed by (chain_root, step_index); every old thread address resolves to its
   exact Run through the alias file.
6. Scheduled sessions, cron configuration and delayed triggers resolve to
   stable manager ids with explicit ``session_id`` bindings; archived
   scheduled sessions import paused. Nothing fires.
7. Ordinary forks stay independent (copy provenance, never decomposition); an
   Elone chain canonicalizes only on explicit successor evidence, with every
   old id kept as an alias to the tail.
8. Archive preferences and failure/cancellation evidence are kept. Old
   archived/completed status alone never proves delivery: completed import
   requires the plan's evidence (success result, required review success,
   implement landing); everything else stays open (hidden when it was
   archived) with an explicit unproven-delivery report.
9. Group/project labels and ledger survive as history; old public rule bodies
   import once into immutable prompt storage within their old proven scope;
   ambiguous PM-supplement prose is an unresolved review item.
10. Old event types, timestamps and body bytes are history; ``task_imported``
    lands only after all of a node's conversion facts are durable, and only
    explicitly proven unhandled inputs enter ``pending_inputs``. No USER event
    is reissued and no authorization window is created.
11. Slack origins, plan/file links and TUI references keep resolving; nothing
    external is sent and silence is never read as completion.

Historical manager execution logs (``data/master_runs``) are covered too: a
proven actual turn (raw log with a final result, or the recorded in-flight
turn's own completed raw log) becomes a ``manager_turn`` Run on the same
logical manager with its input correlation, while an uncertain log stays
explicitly identified historical evidence.

Input-handling correlation is fact-based, never heuristic: a USER input is
*handled* only when a MASTER_DONE names its exact event id (or the recorded
in-flight turn naming it provably completed with a final result). A later
unrelated master output acknowledges nothing. A confirmed unhandled input is
one no manager activity could have consumed (no later manager turn evidence at
all); anything else is unresolved.

Durability contract: apply runs under the home writer fence
(:mod:`src.core.home_writer_fence`), refuses unresolved conversions, source or
converter-code hash drift, target collisions and unproven quiescence before
its first replacement, backs up and hash-verifies every original before
mutation, re-checks drift at the mutation boundary, appends durable receipts
per product, and stays idempotent per product so an interrupted apply resumes
from the same manifest without duplicating nodes, facts or aliases. Rollback
restores originals and removes only migration-owned unchanged products; it
refuses once any product no longer matches its receipt (a new-system write).
There is no global transaction and no claim of one.
"""

from __future__ import annotations

import asyncio
import hashlib
import os
import re
import shutil
import subprocess
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING

import orjson
from pydantic import BaseModel, ConfigDict, Field

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.control_events import (
    TASK_ID_NAMESPACE,
    stable_child_report_id,
    stable_close_event_id,
    stable_run_id,
    stable_task_id,
)
from src.core.home_writer_fence import (
    FenceHolder,
    HomeWriterActiveError,
    acquire_home_writer_fence,
    probe_writer_fence,
)
from src.core.json_utils import atomic_write_text
from src.core.models import (
    PendingTrigger,
    RunRecord,
    SessionMetadata,
    TaskSpec,
    TaskType,
    ThreadMetadata,
    ThreadStatus,
    ensure_utc,
)
from src.core.runs import (
    DATA_DIR_NAME,
    IMPROVE_ITERATION_PREFIX,
    MASTER_RUNS_DIR_NAME,
    RAW_LOG_NAME,
    leftover_holders_for,
    raw_completion_time,
    scan_stdout_holders,
)
from src.core.session_aliases import ALIASES_FILE_NAME, SessionAliasStore
from src.core.sessions import _TRANSIENT_METADATA_FIELDS, SessionManager
from src.core.task_sessions import (
    PROMPT_BODIES_DIR_NAME,
    TaskTreeManager,
)
from src.core.threads import THREADS_DIR_NAME

if TYPE_CHECKING:
    pass

MIGRATION_STATE_DIR_NAME = "session_tree_migration"
MANIFEST_SCHEMA_VERSION = 1
RECEIPTS_FILE_NAME = "receipts.ndjson"

# Conversion-influencing code: the module that derives every product. Apply
# refuses a manifest built by a different converter body.
CONVERTER_MODULE_PATHS = ("src/core/session_tree_migration.py",)

_WORKER_NODE_REQUEST_PREFIX = "thread"
_MANAGER_TURN_REQUEST_PREFIX = "manager-turn"
_IMPROVE_ITERATION_RE = re.compile(
    re.escape(IMPROVE_ITERATION_PREFIX) + r"\s+(\d+)/(\d+)")
# Direction markers a PM supplement may carry for unambiguous classification;
# anything else is ambiguous prose and stays an unresolved review item.
_PM_TASK_GOAL_MARKER = "task-goal:"
_PM_NODE_RULES_MARKER = "node-rules:"


# ---------------------------------------------------------------------------
# Errors
# ---------------------------------------------------------------------------


class MigrationError(RuntimeError):
  """A migration command failed; the CLI prints the message and exits 1."""


class MigrationRefused(MigrationError):
  """Apply/rollback refused before or during mutation; inputs are intact."""

  def __init__(self, reason: str, *, details: list[str] | None = None) -> None:
    self.details = details or []
    super().__init__(reason)


class ManifestDriftError(MigrationRefused):
  """The source no longer matches the manifest's hash binding."""


# ---------------------------------------------------------------------------
# Manifest records
# ---------------------------------------------------------------------------


class SourceFileRecord(BaseModel):
  """One conversion input file, bound by content hash (home-relative path)."""
  model_config = ConfigDict(extra="forbid")

  path: str
  sha256: str
  size: int


class MappingEntry(BaseModel):
  """One source item's disposition: its target, or its explicit non-target."""
  model_config = ConfigDict(extra="forbid")

  source_kind: str
  source_id: str
  target_session_id: str | None = None
  target_run_id: str | None = None
  disposition: str
  reason: str | None = None
  detail: dict = Field(default_factory=dict)


class UnresolvedEntry(BaseModel):
  """One item a human must judge before apply; apply refuses while nonempty."""
  model_config = ConfigDict(extra="forbid")

  source_kind: str
  source_id: str
  reason: str
  refs: list[str] = Field(default_factory=list)


class ProductReceipt(BaseModel):
  """One created/replaced/appended/removed path and its verified hashes."""
  model_config = ConfigDict(extra="forbid")

  path: str  # home-relative
  kind: str  # created | replaced | appended | removed | moved_from
  pre_sha256: str | None = None
  post_sha256: str | None = None
  backup: str | None = None  # backup dir-relative path of the original bytes


class MigrationManifest(BaseModel):
  """The reviewable migration manifest (plan 4.1's product, plus receipts).

  ``source_sha`` binds every conversion input; ``mappings`` accounts for the
  whole input set; ``unresolved`` names what a human must judge; receipts and
  ``rollback_refs`` are filled by apply and make retry and rollback durable.
  """
  model_config = ConfigDict(extra="forbid")

  schema_version: int = MANIFEST_SCHEMA_VERSION
  created_at: datetime
  converter_code_sha256: str
  home_path: str  # informational; binding is by content hashes
  source_sha: str
  source_files: list[SourceFileRecord]
  mappings: list[MappingEntry]
  unresolved: list[UnresolvedEntry]
  created_files: list[str] = Field(default_factory=list)
  rollback_refs: list[str] = Field(default_factory=list)
  receipts: list[ProductReceipt] = Field(default_factory=list)
  applied_at: datetime | None = None
  rolled_back_at: datetime | None = None
  input_summary: dict = Field(default_factory=dict)


# ---------------------------------------------------------------------------
# Inventory scan (read-only)
# ---------------------------------------------------------------------------


def _sha256_bytes(data: bytes) -> str:
  return hashlib.sha256(data).hexdigest()


def _sha256_file(path: Path) -> str:
  digest = hashlib.sha256()
  with open(path, "rb") as f:
    for chunk in iter(lambda: f.read(1 << 20), b""):
      digest.update(chunk)
  return digest.hexdigest()


@dataclass
class TriggerInfo:
  path: Path
  rel_path: str
  trigger: PendingTrigger | None
  parse_error: str | None = None


@dataclass
class LoopInfo:
  owner_id: str
  loop_id: str
  dir: Path
  state_path: Path
  state: object | None = None  # improve_command.ImproveState
  parse_error: str | None = None
  report_files: list[str] = field(default_factory=list)


@dataclass
class ThreadInfo:
  owner_id: str
  meta: ThreadMetadata
  meta_path: Path
  data_dir: Path
  events_path: Path
  raw_log_path: Path | None


@dataclass
class ManagerTurnLog:
  owner_id: str
  dir_name: str
  dir: Path
  started_at: datetime | None
  raw_path: Path | None
  raw_sha256: str | None
  proven: bool
  outcome: str | None  # success | failed, from the log's final result event
  completed_at: datetime | None


@dataclass
class SessionInfo:
  id: str
  dir: Path
  meta: SessionMetadata | None
  meta_path: Path
  parse_error: str | None = None
  events: list[dict] = field(default_factory=list)
  event_parse_errors: list[str] = field(default_factory=list)
  chat_rel_path: str | None = None
  archive_rel_paths: list[str] = field(default_factory=list)
  threads: list[ThreadInfo] = field(default_factory=list)
  loops: list[LoopInfo] = field(default_factory=list)
  triggers: list[TriggerInfo] = field(default_factory=list)
  manager_turn_logs: list[ManagerTurnLog] = field(default_factory=list)


@dataclass
class CronConfigInfo:
  rel_path: str
  path: Path
  body: dict | None = None
  parse_error: str | None = None


@dataclass
class ProjectInfo:
  group: str
  dir: Path
  config: dict | None
  parse_error: str | None = None
  common_body: str | None = None
  common_body_rel_path: str | None = None
  supplement_body: str | None = None
  supplement_body_rel_path: str | None = None


@dataclass
class SourceSnapshot:
  """Everything one conversion decision reads, with per-file hashes."""
  home: Path
  files: dict[str, int] = field(default_factory=dict)  # rel path -> size
  hashes: dict[str, str] = field(default_factory=dict)  # rel path -> sha256
  sessions: dict[str, SessionInfo] = field(default_factory=dict)
  session_parse_errors: dict[str, str] = field(default_factory=dict)
  cron_configs: list[CronConfigInfo] = field(default_factory=list)
  legacy_cron_file: str | None = None
  projects: dict[str, ProjectInfo] = field(default_factory=dict)
  aliases_raw: dict | None = None
  aliases_rel_path: str | None = None
  unreadable: list[str] = field(default_factory=list)

  def add_file(self, path: Path, home: Path) -> str | None:
    """Hash one input file into the snapshot; returns its home-relative path.

    An absent file is legitimate (a session may never have written one) and is
    simply not an input; a file that exists but cannot be read is corruption
    and lands in ``unreadable``.
    """
    if not path.exists():
      return None
    try:
      data = path.read_bytes()
    except OSError as e:
      self.unreadable.append(f"{path}: {e}")
      return None
    rel = path.relative_to(home).as_posix()
    self.files[rel] = len(data)
    self.hashes[rel] = _sha256_bytes(data)
    return rel


def _read_ndjson_file(path: Path) -> tuple[list[dict], list[str]]:
  """Parse an NDJSON file, returning events plus per-line parse errors."""
  events: list[dict] = []
  errors: list[str] = []
  if not path.exists():
    return [], []  # a session that never wrote a log has no history to parse
  try:
    raw = path.read_bytes()
  except OSError as e:
    return [], [f"{path}: {e}"]
  for number, line in enumerate(raw.split(b"\n")):
    if not line.strip():
      continue
    try:
      event = orjson.loads(line)
    except ValueError as e:
      errors.append(f"{path.name}:{number + 1}: {e}")
      continue
    if isinstance(event, dict):
      events.append(event)
    else:
      errors.append(f"{path.name}:{number + 1}: not an object")
  return events, errors


def _archive_files(session_dir: Path) -> list[Path]:
  archives = session_dir / DATA_DIR_NAME / "archives"
  if not archives.is_dir():
    return []
  return sorted(archives.glob("chat_events.*.jsonl"))


def _scan_manager_turn_logs(home: Path, session: SessionInfo) -> None:
  """Every historical manager execution log under data/master_runs."""
  root = session.dir / DATA_DIR_NAME / MASTER_RUNS_DIR_NAME
  if not root.is_dir():
    return
  for child in sorted(root.iterdir()):
    if not child.is_dir():
      continue
    try:
      started_at = datetime.fromisoformat(child.name)
    except ValueError:
      started_at = None
    raw_path = child / RAW_LOG_NAME
    raw_sha: str | None = None
    outcome: str | None = None
    proven = False
    if raw_path.is_file():
      raw_sha = _sha256_file(raw_path)
      result = _final_result_event(raw_path)
      if result is not None:
        subtype = result.get("subtype")
        is_error = result.get("is_error")
        outcome = "failed" if (subtype not in (None, "success") or is_error not in (None, False)) else "success"
        proven = True
    session.manager_turn_logs.append(ManagerTurnLog(
        owner_id=session.id,
        dir_name=child.name,
        dir=child,
        started_at=started_at,
        raw_path=raw_path if raw_path.is_file() else None,
        raw_sha256=raw_sha,
        proven=proven,
        outcome=outcome,
        completed_at=_log_completion_time(raw_path) if raw_path.is_file() else None,
    ))


def _log_completion_time(raw_path: Path) -> datetime | None:
  """The turn's completion time, from evidence in this order.

  1. The log's own event timestamps (portable across copies).
  2. The raw file's final mtime — the runtime's own completion contract for
     raw logs (``src/agents/backends/base.py`` clamps injected event times to
     it), valid whenever the offline copy preserves mtimes.

  Returns None only when neither is available (no parseable event time and no
  stat), in which case the turn's end is simply not timestamped.
  """
  try:
    data = raw_path.read_bytes()
  except OSError:
    return raw_completion_time(raw_path)
  last: datetime | None = None
  for line in data.split(b"\n"):
    if not line.strip():
      continue
    try:
      event = orjson.loads(line)
    except ValueError:
      continue
    if not isinstance(event, dict):
      continue
    ts = event.get("timestamp")
    if not isinstance(ts, str):
      continue
    try:
      last = ensure_utc(datetime.fromisoformat(ts))
    except ValueError:
      continue
  if last is not None:
    return last
  return raw_completion_time(raw_path)


def _final_result_event(raw_path: Path) -> dict | None:
  """The last ``result`` event in a raw NDJSON log, or None (absent/unreadable)."""
  try:
    data = raw_path.read_bytes()
  except OSError:
    return None
  last: dict | None = None
  for line in data.split(b"\n"):
    if not line.strip():
      continue
    try:
      event = orjson.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict) and event.get("type") == ET.RESULT:
      last = event
  return last


def scan_source(cfg: CharlieBotConfig) -> SourceSnapshot:
  """Read the whole conversion input set of the selected home (read-only)."""
  home = cfg.charliebot_home
  snap = SourceSnapshot(home=home)
  sessions_dir = cfg.sessions_dir
  if not sessions_dir.is_dir():
    return snap

  for child in sorted(sessions_dir.iterdir()):
    if not child.is_dir() or child.is_symlink():
      if child.is_symlink():
        snap.unreadable.append(f"{child}: session directory is a symlink")
      continue
    if child.name.startswith(".task-") and child.name.endswith(".tmp"):
      continue  # an unpublished create's staging directory is never an input
    info = SessionInfo(id=child.name, dir=child, meta=None, meta_path=child / "metadata.json")
    snap.sessions[child.name] = info
    if not info.meta_path.is_file():
      snap.session_parse_errors[child.name] = f"metadata.json missing under {child}"
      continue
    if info.meta_path.is_symlink():
      snap.session_parse_errors[child.name] = f"{info.meta_path} is a symlink"
      continue
    rel = snap.add_file(info.meta_path, home)
    try:
      info.meta = SessionMetadata.model_validate_json(info.meta_path.read_text(encoding="utf-8"))
    except (ValueError, OSError) as e:
      snap.session_parse_errors[child.name] = f"metadata unreadable: {e}"
      continue
    info.chat_rel_path = snap.add_file(child / DATA_DIR_NAME / "chat_events.jsonl", home) or None
    events, errors = _read_ndjson_file(child / DATA_DIR_NAME / "chat_events.jsonl")
    info.events = events
    info.event_parse_errors = list(errors)
    for archive in _archive_files(child):
      rel = snap.add_file(archive, home)
      if rel:
        info.archive_rel_paths.append(rel)
      archive_events, archive_errors = _read_ndjson_file(archive)
      info.events.extend(archive_events)
      info.event_parse_errors.extend(archive_errors)
    info.events.sort(key=_event_sort_key)
    _scan_threads(snap, home, info)
    _scan_loops(snap, home, info)
    _scan_triggers(snap, home, info)
    _scan_manager_turn_logs(home, info)

  _scan_cron(cfg, snap)
  _scan_projects(snap)
  aliases_path = sessions_dir / ALIASES_FILE_NAME
  if aliases_path.is_file():
    snap.aliases_rel_path = snap.add_file(aliases_path, home)
    try:
      raw = orjson.loads(aliases_path.read_bytes())
    except ValueError as e:
      snap.unreadable.append(f"{aliases_path}: {e}")
    else:
      if isinstance(raw, dict):
        snap.aliases_raw = raw
      else:
        snap.unreadable.append(f"{aliases_path}: not a JSON object")
  return snap


def _event_sort_key(event: dict) -> tuple[str, str]:
  timestamp = str(event.get("timestamp") or "")
  return (timestamp, str(event.get("id") or ""))


def _scan_threads(snap: SourceSnapshot, home: Path, session: SessionInfo) -> None:
  threads_dir = session.dir / THREADS_DIR_NAME
  if not threads_dir.is_dir():
    return
  for child in sorted(threads_dir.iterdir()):
    if not child.is_dir():
      continue
    meta_path = child / "metadata.json"
    if meta_path.is_symlink():
      snap.unreadable.append(f"{meta_path}: thread metadata is a symlink")
      continue
    rel = snap.add_file(meta_path, home)
    if rel is None:
      continue
    try:
      meta = ThreadMetadata.model_validate_json(meta_path.read_text(encoding="utf-8"))
    except (ValueError, OSError) as e:
      snap.unreadable.append(f"{meta_path}: {e}")
      continue
    data_dir = child / DATA_DIR_NAME
    info = ThreadInfo(
        owner_id=session.id,
        meta=meta,
        meta_path=meta_path,
        data_dir=data_dir,
        events_path=data_dir / "events.jsonl",
        raw_log_path=data_dir / RAW_LOG_NAME if (data_dir / RAW_LOG_NAME).is_file() else None,
    )
    snap.add_file(info.events_path, home)
    if info.raw_log_path is not None:
      snap.add_file(info.raw_log_path, home)
    session.threads.append(info)


def _scan_loops(snap: SourceSnapshot, home: Path, session: SessionInfo) -> None:
  from src.core.improve_command import ImproveState

  loops_dir = session.dir / "loops"
  if not loops_dir.is_dir():
    return
  for child in sorted(loops_dir.iterdir()):
    if not child.is_dir():
      continue
    state_path = child / "state.json"
    info = LoopInfo(owner_id=session.id, loop_id=child.name, dir=child, state_path=state_path)
    if state_path.is_file():
      rel = snap.add_file(state_path, home)
      if rel is None:
        info.parse_error = f"{state_path}: unreadable"
      else:
        try:
          info.state = ImproveState.model_validate_json(state_path.read_text(encoding="utf-8"))
        except (ValueError, OSError) as e:
          info.parse_error = f"state.json unreadable: {e}"
          snap.unreadable.append(f"{state_path}: {e}")
    else:
      info.parse_error = f"{state_path}: missing"
    info.report_files = sorted(p.name for p in child.glob("iter_*.md") if p.is_file())
    for name in ("goal.md", "plan.md", *info.report_files):
      path = child / name
      if path.is_file():
        snap.add_file(path, home)
    session.loops.append(info)


def _scan_triggers(snap: SourceSnapshot, home: Path, session: SessionInfo) -> None:
  triggers_dir = session.dir / "triggers"
  if not triggers_dir.is_dir():
    return
  for child in sorted(triggers_dir.iterdir()):
    if not child.is_file() or child.suffix != ".json":
      continue
    if child.is_symlink():
      snap.unreadable.append(f"{child}: trigger file is a symlink")
      continue
    rel = snap.add_file(child, home)
    if rel is None:
      continue
    info = TriggerInfo(path=child, rel_path=rel, trigger=None)
    try:
      info.trigger = PendingTrigger.model_validate_json(child.read_text(encoding="utf-8"))
    except (ValueError, OSError) as e:
      info.parse_error = str(e)
      snap.unreadable.append(f"{child}: {e}")
    session.triggers.append(info)


def _scan_cron(cfg: CharlieBotConfig, snap: SourceSnapshot) -> None:
  cron_d = cfg.charliebot_home / "config.d" / "cron.d"
  if cron_d.is_dir():
    for child in sorted(cron_d.iterdir()):
      if not (child.is_file() and child.name.endswith(".yaml")) or child.is_symlink():
        continue
      info = CronConfigInfo(rel_path=child.relative_to(cfg.charliebot_home).as_posix(), path=child)
      rel = snap.add_file(child, cfg.charliebot_home)
      if rel is None:
        info.parse_error = "unreadable"
      else:
        try:
          from src.core.yaml_utils import load_yaml
          body = load_yaml(child)
        except Exception as e:  # yaml errors are the loader's per-file error class of problem
          info.parse_error = str(e)
          snap.unreadable.append(f"{child}: {e}")
        else:
          if isinstance(body, dict):
            info.body = body
          else:
            info.parse_error = "cron config is not a mapping"
            snap.unreadable.append(f"{child}: not a mapping")
      snap.cron_configs.append(info)
  legacy = cfg.charliebot_home / "config.d" / "cron.yaml"
  if legacy.exists():
    snap.legacy_cron_file = legacy.relative_to(cfg.charliebot_home).as_posix()


def _scan_projects(snap: SourceSnapshot) -> None:
  from src.core.project_config import PROJECT_CONFIG_FILENAME, PROJECTS_DIR_NAME

  projects_dir = snap.home / PROJECTS_DIR_NAME
  if not projects_dir.is_dir():
    return
  for child in sorted(projects_dir.iterdir()):
    if not child.is_dir():
      continue
    info = ProjectInfo(group=child.name, dir=child, config=None)
    config_path = child / PROJECT_CONFIG_FILENAME
    rel = snap.add_file(config_path, snap.home)
    if rel is None:
      info.parse_error = f"{config_path}: unreadable"
    else:
      try:
        from src.core.yaml_utils import load_yaml
        body = load_yaml(config_path)
      except Exception as e:
        info.parse_error = str(e)
        snap.unreadable.append(f"{config_path}: {e}")
      else:
        info.config = body if isinstance(body, dict) else None
        if info.config is None:
          info.parse_error = "project.yaml is not a mapping"
    if isinstance(info.config, dict):
      for key, attr in (("prompt_file", "common_body"), ("manager_prompt_file", "supplement_body")):
        value = info.config.get(key)
        if not isinstance(value, str) or not value.strip():
          continue
        body_path = child / value
        body_rel = snap.add_file(body_path, snap.home)
        if body_rel is None:
          info.parse_error = info.parse_error or f"{body_path}: unreadable"
          continue
        try:
          text = body_path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as e:
          info.parse_error = info.parse_error or f"{body_path}: {e}"
          continue
        setattr(info, attr, text)
        setattr(info, f"{attr}_rel_path", body_rel)
    snap.projects[child.name] = info


# ---------------------------------------------------------------------------
# Conversion plan (pure decisions derived from one snapshot)
# ---------------------------------------------------------------------------


@dataclass
class PromptBodyWrite:
  ref: str
  text: str


@dataclass
class FactWrite:
  """One control fact to append to a node's log (idempotent by event id)."""
  session_id: str
  event: dict
  actor: str = "system"


@dataclass
class RunProduct:
  record: RunRecord
  # Terminal fact fields (None = no terminal fact; only proven evidence imports one).
  outcome: str | None = None
  ended_at: datetime | None = None
  exit_code: int | None = None
  input_event_ids: list[str] = field(default_factory=list)


@dataclass
class WorkerNodeProduct:
  """One new worker task node, published atomically with its run records."""
  target_id: str
  owner_id: str
  original_owner_id: str
  metadata: SessionMetadata
  runs: list[RunProduct]
  task_imported: dict
  source_refs: list[str]
  task_closed: dict | None = None  # completed-import close fact (child nodes)
  child_report: dict | None = None  # delivery fact in the owner's log
  completion_thread: "ThreadInfo | None" = None  # the work thread the completed-import pass judges


@dataclass
class ManagerConversion:
  """One legacy session's conversion to a manager node (original id kept)."""
  session_id: str
  kind: str  # ordinary | pm | scheduled
  canonical_id: str
  metadata: SessionMetadata  # the converted metadata (replacement product)
  task_imported: dict
  pending_inputs: list[dict]
  manager_turn_runs: list[RunProduct]
  confirmed_inputs: dict[str, str]  # input event id -> confirming run id
  prompt_bodies: list[PromptBodyWrite]
  source_refs: list[str]
  unproven_delivery: str | None  # reason string when delivery stays unproven


@dataclass
class TriggerMove:
  old_rel_path: str
  new_rel_path: str
  trigger: PendingTrigger


@dataclass
class CronRewrite:
  rel_path: str
  original_text: str
  new_text: str


@dataclass
class ConversionPlan:
  """Everything apply will write, derived from one snapshot."""
  managers: list[ManagerConversion]
  workers: list[WorkerNodeProduct]
  prompt_bodies: list[PromptBodyWrite]
  alias_old_sessions: dict[str, str]
  alias_old_threads: dict[str, dict]
  cron_rewrites: list[CronRewrite]
  trigger_moves: list[TriggerMove]
  mappings: list[MappingEntry]
  unresolved: list[UnresolvedEntry]
  organization_pending: list[dict]
  input_summary: dict


def _run_id_for(owner_or_target: str, request_id: str) -> str:
  return stable_run_id(owner_or_target, request_id)


def _worker_request_id(original_owner_id: str, thread_id: str) -> str:
  return f"{_WORKER_NODE_REQUEST_PREFIX}:{original_owner_id}/{thread_id}"


def _manager_turn_request_id(log: ManagerTurnLog) -> str:
  return f"{_MANAGER_TURN_REQUEST_PREFIX}:{log.dir_name}"


def _iso(dt: datetime | None) -> str | None:
  return dt.astimezone(UTC).isoformat() if dt is not None else None


def canonical_successor_tail(sessions: dict[str, SessionInfo], start_id: str,
                             unresolved: list[UnresolvedEntry]) -> str:
  """The elone succession tail of *start_id*, or the id itself (no successor).

  Only an explicit ``successor_session_id`` edge moves the canonical entry;
  ``parent_session_id`` is copy provenance and never a succession edge. A
  missing successor or a cycle is recorded as unresolved.
  """
  seen: set[str] = set()
  current = start_id
  while True:
    if current in seen:
      unresolved.append(UnresolvedEntry(
          source_kind="elone_chain", source_id=start_id,
          reason=f"successor chain from {start_id} cycles at {current}",
          refs=[f"sessions/{current}/metadata.json"]))
      return current
    seen.add(current)
    info = sessions.get(current)
    if info is None or info.meta is None:
      unresolved.append(UnresolvedEntry(
          source_kind="elone_chain", source_id=start_id,
          reason=f"successor session {current} is missing or unreadable",
          refs=[f"sessions/{current}/metadata.json"]))
      return current
    nxt = info.meta.successor_session_id
    if not nxt:
      return current
    if nxt not in sessions:
      unresolved.append(UnresolvedEntry(
          source_kind="elone_chain", source_id=current,
          reason=f"successor_session_id {nxt} has no session record",
          refs=[f"sessions/{current}/metadata.json"]))
      return current
    current = nxt


def _user_round_activity(events: list[dict]) -> dict:
  """Correlation facts of one manager log's history.

  Returns the MASTER_DONE events that name an input (``input_event_id``), the
  recorded in-flight turn's naming (metadata is handled by the caller), and
  every proven manager turn start/end window.
  """
  done_by_input: dict[str, list[dict]] = {}
  done_events: list[dict] = []
  for event in events:
    if event.get("type") != ET.MASTER_DONE:
      continue
    done_events.append(event)
    named = event.get(ET.INPUT_EVENT_ID)
    if isinstance(named, str) and named:
      done_by_input.setdefault(named, []).append(event)
  return {"done_by_input": done_by_input, "done_events": done_events}


def _input_handled_by_done(events_by_id: dict[str, dict], done_by_input: dict[str, list[dict]],
                           input_id: str) -> dict | None:
  """The MASTER_DONE fact naming this exact input, if one exists."""
  named = done_by_input.get(input_id)
  if not named:
    return None
  return named[0]


def _turn_log_covering(events_by_id: dict[str, dict], log: ManagerTurnLog,
                       input_event: dict | None, named_id: str | None) -> bool:
  """Whether *log* is the proven turn that consumed the named input.

  The turn's own raw log carries the input's text (the old launch prompt was
  the message content), and the input must predate the turn's start. An empty
  input body proves nothing and is never correlated.
  """
  if log.raw_path is None or not log.proven:
    return False
  if input_event is None:
    return False
  content = input_event.get("content")
  if not isinstance(content, str) or not content.strip():
    return False
  if log.started_at is None or log.completed_at is None:
    return False
  ts = input_event.get("timestamp")
  if isinstance(ts, str):
    try:
      from src.core.models import ensure_utc
      stamp = ensure_utc(datetime.fromisoformat(ts))
      if stamp > log.started_at:
        return False  # the input was written after this turn launched
    except ValueError:
      pass
  try:
    return content.encode("utf-8") in log.raw_path.read_bytes()
  except OSError:
    return False


def _classify_inputs(info: SessionInfo, events: list[dict], meta: SessionMetadata,
                     plan_unresolved: list[UnresolvedEntry],
                     done_by_input: dict[str, list[dict]],
                     proven_logs: list[ManagerTurnLog],
                     master_run_record: object | None) -> tuple[list[dict], list[dict], list[dict]]:
  """The three-way input disposition of one manager log's history.

  Returns (pending, confirmed_pairs, uncertain) where confirmed_pairs is
  (input_id, correlated log dir_name) and uncertain entries become unresolved
  items with precise refs.
  """
  from src.core.models import MasterRunRecord

  pending: list[dict] = []
  confirmed: list[tuple[str, str]] = []
  uncertain: list[UnresolvedEntry] = []
  events_by_id = {str(e.get("id")): e for e in events if isinstance(e.get("id"), str)}
  in_flight_named: str | None = None
  in_flight_proven = False
  if isinstance(master_run_record, MasterRunRecord):
    in_flight_named = master_run_record.user_event_id
    if master_run_record.raw_log:
      raw_path = Path(master_run_record.raw_log)
      if raw_path.is_file() and _final_result_event(raw_path) is not None:
        in_flight_proven = True

  # Manager activity windows: any proven turn's start, any MASTER_DONE. A
  # confirmed-unhandled input needs NO activity after it — nothing could have
  # consumed it. Activity that does not name the input is never an
  # acknowledgement (the unrelated-master-output rule).
  activity_stamps: list[datetime] = []
  for log in proven_logs:
    if log.started_at is not None:
      activity_stamps.append(log.started_at)
  for done in _user_round_activity(events)["done_events"]:
    ts = done.get("timestamp")
    if isinstance(ts, str):
      try:
        from src.core.models import ensure_utc
        activity_stamps.append(ensure_utc(datetime.fromisoformat(ts)))
      except ValueError:
        pass

  for event in events:
    etype = event.get("type")
    if etype not in (ET.USER, ET.AGENT_MESSAGE):
      continue
    input_id = event.get("id")
    if not isinstance(input_id, str) or not input_id:
      uncertain.append(UnresolvedEntry(
          source_kind="old_input", source_id=f"{info.id}:untitled",
          reason="input event carries no stable id; handling cannot be correlated",
          refs=[info.chat_rel_path or f"sessions/{info.id}/data/chat_events.jsonl"]))
      continue
    done = _input_handled_by_done(events_by_id, done_by_input, input_id)
    if done is not None:
      # The MASTER_DONE fact is the old system's durable handled marker for
      # this exact event id. Binding it to a specific proven turn log (whose
      # raw content carries the input) sharpens the evidence when uniquely
      # possible; without such a log the input is still proven handled by the
      # round's own completion fact and is excluded from the import boundary.
      covering = [log for log in proven_logs if _turn_log_covering(events_by_id, log, event, input_id)]
      if len(covering) == 1:
        confirmed.append((input_id, covering[0].dir_name))
      else:
        confirmed.append((input_id, "master_done_unbound"))
      continue
    if in_flight_named == input_id and in_flight_proven:
      confirmed.append((input_id, "master_run"))
      continue
    if in_flight_named == input_id:
      uncertain.append(UnresolvedEntry(
          source_kind="old_input", source_id=f"{info.id}:{input_id}",
          reason="the recorded in-flight turn names this input but its raw log does not "
                 "prove a completed turn; handling is unproven",
          refs=[info.chat_rel_path or f"sessions/{info.id}/data/chat_events.jsonl"]))
      continue
    # Not named by any round: pending only when NO manager activity could have
    # consumed it (the old system replays exactly such messages on restart).
    stamp = event.get("timestamp")
    input_time: datetime | None = None
    if isinstance(stamp, str):
      try:
        from src.core.models import ensure_utc
        input_time = ensure_utc(datetime.fromisoformat(stamp))
      except ValueError:
        input_time = None
    later_activity = [s for s in activity_stamps if input_time is None or s > input_time]
    if not later_activity:
      source_ref = f"sessions/{info.id}/data/chat_events.jsonl#{input_id}"
      pending.append({
          "source_ref": source_ref,
          "input_id": input_id,
          "event_type": etype,
      })
      continue
    uncertain.append(UnresolvedEntry(
        source_kind="old_input", source_id=f"{info.id}:{input_id}",
        reason="no manager round names this input but later manager activity exists; "
               "whether a queued round consumed it cannot be proven from retained evidence",
        refs=[info.chat_rel_path or f"sessions/{info.id}/data/chat_events.jsonl"]))
  pending.sort(key=lambda e: e["input_id"])
  return pending, confirmed, uncertain


def _migration_appended_event_ids(info: SessionInfo, migrated_run_ids: set[str]) -> set[str]:
  """Ids of the events this converter itself appended to the node's history.

  Re-deriving a plan over an already-migrated node must classify the ORIGINAL
  history only: the import boundary's own facts (task_imported and the
  imported runs' run_finished acknowledgements) are migration products, not
  old evidence, and post-import inputs belong to the ordinary fold — never to
  a re-planned manifest's pending list.
  """
  appended = {str(uuid.uuid5(TASK_ID_NAMESPACE, f"task-imported:{info.id}"))}
  for event in info.events:
    if event.get("type") == ET.RUN_FINISHED and event.get("run_id") in migrated_run_ids:
      event_id = event.get("id")
      if isinstance(event_id, str):
        appended.add(event_id)
  return appended


def _session_kind(meta: SessionMetadata) -> str:
  if meta.scheduled_task:
    return "scheduled"
  if meta.role == "project":
    return "pm"
  return "ordinary"


def _classify_pm_supplement(text: str) -> tuple[str | None, str | None, str | None]:
  """Split one PM supplement into (goal, node rules, remainder).

  Only explicit direction markers classify a segment; anything outside them is
  ambiguous prose and becomes an unresolved review item (never silently
  imported as a rule or a goal). The first segment of each kind classifies;
  any further segment of the same kind falls to the remainder.
  """
  goal: str | None = None
  rules: str | None = None
  remainder: list[str] = []
  mode: str | None = None
  buffer: list[str] = []

  def flush() -> None:
    nonlocal goal, rules
    segment = "\n".join(buffer).strip()
    if not segment:
      return
    if mode == "goal":
      if goal is None:
        goal = segment
      else:
        remainder.append(segment)
    elif mode == "rules":
      if rules is None:
        rules = segment
      else:
        remainder.append(segment)
    else:
      remainder.append(segment)

  for line in text.splitlines():
    stripped = line.strip()
    marker = None
    if stripped.startswith(_PM_TASK_GOAL_MARKER):
      marker = "goal"
    elif stripped.startswith(_PM_NODE_RULES_MARKER):
      marker = "rules"
    if marker is not None and marker != mode:
      flush()
      buffer = []
      mode = marker
      inline = stripped[len(_PM_TASK_GOAL_MARKER if marker == "goal" else _PM_NODE_RULES_MARKER):].strip()
      if inline:
        buffer.append(inline)
      continue
    buffer.append(line)
  flush()
  return goal, rules, ("\n".join(remainder).strip() or None)


def build_conversion_plan(cfg: CharlieBotConfig, snap: SourceSnapshot) -> ConversionPlan:
  """Derive every product and disposition from one read-only snapshot."""
  unresolved: list[UnresolvedEntry] = []
  mappings: list[MappingEntry] = []
  managers: list[ManagerConversion] = []
  workers: list[WorkerNodeProduct] = []
  prompt_bodies: dict[str, PromptBodyWrite] = {}
  alias_old_sessions: dict[str, str] = {}
  alias_old_threads: dict[str, dict] = {}
  cron_rewrites: list[CronRewrite] = []
  trigger_moves: list[TriggerMove] = []
  organization_pending: list[dict] = []

  if snap.legacy_cron_file:
    unresolved.append(UnresolvedEntry(
        source_kind="cron_config", source_id=snap.legacy_cron_file,
        reason="legacy config.d/cron.yaml is present; its entries are not loadable "
               "and must be split into config.d/cron.d/ before migration",
        refs=[snap.legacy_cron_file]))
  for rel in snap.unreadable:
    unresolved.append(UnresolvedEntry(
        source_kind="unreadable_record", source_id=rel,
        reason="input record is unreadable or malformed; it must not be silently omitted",
        refs=[rel]))
  for sid, error in snap.session_parse_errors.items():
    unresolved.append(UnresolvedEntry(
        source_kind="session", source_id=sid, reason=error,
        refs=[f"sessions/{sid}/metadata.json"]))
  for sid in sorted(snap.sessions):
    errors = snap.sessions[sid].event_parse_errors
    if errors:
      unresolved.append(UnresolvedEntry(
          source_kind="chat_history", source_id=sid,
          reason=f"chat history holds {len(errors)} unparseable line(s); the old record set must "
                 "be complete before conversion (first: " + errors[0][:160] + ")",
          refs=[snap.sessions[sid].chat_rel_path or f"sessions/{sid}/data/chat_events.jsonl"]))

  # --- canonical elone tails -------------------------------------------------
  canonical: dict[str, str] = {}
  for sid, info in snap.sessions.items():
    if info.meta is None:
      continue
    if info.meta.successor_session_id:
      tail = canonical_successor_tail(snap.sessions, sid, unresolved)
    else:
      tail = sid
    canonical[sid] = tail

  # A tail must itself be an unconverted legacy session; a successor that is
  # already v2 or missing was recorded above when walking.
  predecessor_of: dict[str, list[str]] = {}
  for sid, tail in canonical.items():
    if tail != sid:
      predecessor_of.setdefault(tail, []).append(sid)

  # --- project rule bodies ---------------------------------------------------
  pm_of_group: dict[str, str] = {}
  for sid, info in snap.sessions.items():
    if info.meta is None:
      continue
    if _session_kind(info.meta) == "pm" and info.meta.group:
      pm_of_group.setdefault(info.meta.group, sid)

  project_bodies: dict[str, str] = {}  # group -> subtree body text (enabled projects)
  for group, project in snap.projects.items():
    if project.parse_error:
      unresolved.append(UnresolvedEntry(
          source_kind="project_rules", source_id=group, reason=project.parse_error,
          refs=[f"projects/{group}/project.yaml"]))
      continue
    common = project.common_body
    if common is None:
      organization_pending.append({
          "group": group, "item": "project config without a readable common body",
          "refs": [f"projects/{group}/project.yaml"]})
      continue
    ref = hashlib.sha256(common.encode("utf-8")).hexdigest()
    prompt_bodies[ref] = PromptBodyWrite(ref=ref, text=common)
    project_bodies[group] = ref
    supplement = project.supplement_body
    pm_id = pm_of_group.get(group)
    if project.supplement_body is not None and pm_id is None:
      organization_pending.append({
          "group": group,
          "item": "manager supplement present but no confirmed PM session carries this group",
          "refs": [f"projects/{group}/project.yaml"]})
    _ = supplement, pm_id
    # Non-body files (ledger etc.) stay as read-only history in place.
    organization_pending.append({
        "group": group,
        "item": "project directory retained as read-only history (ledger and other files "
                "are not converted; task references need human organization)",
        "refs": [f"projects/{group}/"]})

  # --- per-session manager conversion ---------------------------------------
  # A legacy session converts to a manager node with its original id. A v2
  # session is preserved untouched (mixed input) — unless it is this plan's
  # own published product: a partially-applied home must re-derive the same
  # plan, so a v2 node whose current bytes equal the re-derived conversion is
  # recognized as ours and planned again (idempotent), while any other v2 node
  # is preserved. Worker products are recognized after thread derivation below.
  deferred_v2: list[str] = []
  for sid in sorted(snap.sessions):
    info = snap.sessions[sid]
    if info.meta is None:
      continue
    if info.meta.profile is not None:
      deferred_v2.append(sid)
      continue
    if canonical.get(sid) != sid:
      # An elone predecessor: history + alias; its threads, triggers and turn
      # logs are planned under the canonical tail. Its own directory is never
      # converted to a manager node.
      tail = canonical[sid]
      alias_old_sessions[sid] = tail
      mappings.append(MappingEntry(
          source_kind="elone_predecessor", source_id=sid, target_session_id=tail,
          disposition="alias_to_tail",
          reason="explicit successor chain canonicalizes at its tail; this id stays a "
                 "resolvable alias and its history files stay at their original paths",
          detail={"successor_session_id": info.meta.successor_session_id or ""}))
      continue
    _plan_manager_conversion(snap, sid, info, canonical, predecessor_of, pm_of_group,
                             project_bodies, snap.projects, managers, mappings, unresolved,
                             prompt_bodies, organization_pending)

  # --- thread conversion ------------------------------------------------------
  thread_targets: dict[tuple[str, str], WorkerNodeProduct] = {}
  for sid in sorted(snap.sessions):
    info = snap.sessions[sid]
    if info.meta is None:
      continue
    owner_canonical = canonical.get(sid, sid)
    owner_manager = next((m for m in managers if m.session_id == owner_canonical), None)
    tail_info = snap.sessions.get(owner_canonical)
    if owner_manager is None and info.threads and (
        tail_info is None or tail_info.meta is None or tail_info.meta.profile is None):
      unresolved.append(UnresolvedEntry(
          source_kind="thread_owner", source_id=sid,
          reason="threads exist under a session whose manager conversion is missing",
          refs=[f"sessions/{sid}/metadata.json"]))
      continue
    for thread in sorted(info.threads, key=lambda t: (t.meta.created_at, t.meta.id)):
      _plan_thread(
          snap, info, thread, owner_canonical, owner_manager, managers, workers,
          thread_targets, mappings, unresolved, alias_old_threads, prompt_bodies)

  # --- classify deferred v2 sessions ------------------------------------------
  # A v2 session is this plan's own published product when its current bytes
  # equal the re-derived conversion (worker: its id is a derived target;
  # manager: the conversion re-derived from its current metadata). Anything
  # else is mixed-input data preserved untouched.
  for sid in deferred_v2:
    info = snap.sessions[sid]
    meta = info.meta
    assert meta is not None and meta.profile is not None
    worker_target = next((w for w in workers if w.target_id == sid), None)
    if worker_target is not None:
      existing_path = snap.home / "sessions" / sid / "metadata.json"
      try:
        existing_bytes = existing_path.read_text(encoding="utf-8")
      except OSError:
        existing_bytes = ""
      if existing_bytes != _expected_worker_metadata_bytes(worker_target):
        unresolved.append(UnresolvedEntry(
            source_kind="target_collision", source_id=sid,
            reason="a v2 session occupies this derived worker target with different content",
            refs=[f"sessions/{sid}/metadata.json"]))
      continue  # this plan's own published worker product; thread planning re-derived it
    if meta.profile == "manager" and meta.task_parent_id is None:
      before = len(managers)
      _plan_manager_conversion(snap, sid, info, canonical, predecessor_of, pm_of_group,
                               project_bodies, snap.projects, managers, mappings, unresolved,
                               prompt_bodies, organization_pending)
      converted = managers[before] if len(managers) > before else None
      existing_path = snap.home / "sessions" / sid / "metadata.json"
      try:
        existing_bytes = existing_path.read_text(encoding="utf-8")
      except OSError:
        existing_bytes = ""
      if converted is not None and existing_bytes == _expected_manager_metadata_bytes(converted):
        continue  # this plan's own published manager product (resumed apply)
      # Not our product: drop the provisional conversion, preserve the node.
      if converted is not None:
        managers.pop(before)
        mappings[:] = [m for m in mappings
                       if not (m.source_kind in ("ordinary", "pm", "scheduled", "manager_turn_log")
                               and (m.source_id == sid or m.source_id.startswith(f"{sid}/")))]
      mappings.append(MappingEntry(
          source_kind="v2_task", source_id=sid, target_session_id=sid,
          disposition="already_v2",
          reason="existing schema_version=2 task preserved untouched"))
      continue
    mappings.append(MappingEntry(
        source_kind="v2_task", source_id=sid, target_session_id=sid,
        disposition="already_v2",
        reason="existing schema_version=2 task preserved untouched"))

  # --- cron configuration bindings -------------------------------------------
  for config in snap.cron_configs:
    body = config.body
    if body is None:
      continue
    task_name = Path(config.rel_path).stem
    if body.get("session_id"):
      bound = str(body["session_id"])
      if _is_our_cron_binding(body, bound, task_name, managers):
        # This plan's own applied binding: re-planning the same rewrite keeps a
        # resumed apply idempotent (the write lands byte-identical content).
        target = bound
      else:
        mappings.append(MappingEntry(
            source_kind="cron_config", source_id=config.rel_path,
            target_session_id=bound, disposition="cron_binding_kept",
            reason="already carries an explicit session_id binding; preserved"))
        continue
    else:
      target = _resolve_cron_target(snap, body, managers, unresolved, config.rel_path)
      if target is None:
        continue
    new_body = dict(body)
    new_body.pop("project", None)  # an explicit binding replaces role/group discovery
    new_body["session_id"] = target
    import yaml as _yaml
    new_text = _yaml.safe_dump(new_body, allow_unicode=True, default_flow_style=False, sort_keys=False)
    error = _validate_rewritten_cron(cfg, config.path, new_text, task_name)
    if error:
      unresolved.append(UnresolvedEntry(
          source_kind="cron_config", source_id=config.rel_path, reason=f"rewritten config would not load: {error}",
          refs=[config.rel_path]))
      continue
    cron_rewrites.append(CronRewrite(
        rel_path=config.rel_path, original_text=config.path.read_text(encoding="utf-8"),
        new_text=new_text))
    mappings.append(MappingEntry(
        source_kind="cron_config", source_id=config.rel_path, target_session_id=target,
        disposition="cron_binding", detail={"mode": str(body.get("mode") or "worker")}))

  # --- triggers ---------------------------------------------------------------
  # Grouped by trigger id: a delayed trigger this plan already moved (its copy
  # sits at the canonical manager with the rebound session_id) must re-derive
  # the same mapping identity on a resumed apply instead of planning twice.
  by_trigger_id: dict[str, list[TriggerInfo]] = {}
  for sid in sorted(snap.sessions):
    for trigger_info in snap.sessions[sid].triggers:
      if trigger_info.trigger is None:
        continue
      by_trigger_id.setdefault(trigger_info.trigger.id, []).append(trigger_info)
  for trigger_id in sorted(by_trigger_id):
    candidates = by_trigger_id[trigger_id]
    predecessor_entries = [
        t for t in candidates
        if canonical.get(t.trigger.session_id, t.trigger.session_id) != t.trigger.session_id]
    entries = predecessor_entries or candidates
    for trigger_info in entries:
      trigger = trigger_info.trigger
      owner_canonical = canonical.get(trigger.session_id, trigger.session_id)
      manager = next((m for m in managers if m.session_id == owner_canonical), None)
      target_info = snap.sessions.get(owner_canonical)
      target_is_ours = (
          manager is not None
          or (target_info is not None and target_info.meta is not None
              and target_info.meta.profile is not None))
      if not target_is_ours:
        unresolved.append(UnresolvedEntry(
            source_kind="trigger", source_id=trigger_info.rel_path,
            reason=f"trigger targets session {trigger.session_id}, which has no manager conversion",
            refs=[trigger_info.rel_path]))
        continue
      if owner_canonical == trigger.session_id:
        mappings.append(MappingEntry(
            source_kind="trigger", source_id=trigger.id,
            target_session_id=owner_canonical, disposition="trigger_kept",
            detail={"path": trigger_info.rel_path,
                    "status": trigger.status.value,
                    "watch_targets": [w.model_dump() for w in trigger.watch_targets]}))
        continue
      new_rel = f"sessions/{owner_canonical}/triggers/{trigger_info.path.name}"
      moved = trigger.model_copy(deep=True)
      moved.session_id = owner_canonical
      trigger_moves.append(TriggerMove(
          old_rel_path=trigger_info.rel_path, new_rel_path=new_rel, trigger=moved))
      mappings.append(MappingEntry(
          source_kind="trigger", source_id=trigger.id,
          target_session_id=owner_canonical, disposition="trigger_rebound",
          reason="delayed trigger on an elone predecessor moves to the canonical manager; "
                 "watch targets preserved verbatim and nothing fires",
          detail={"new_path": new_rel}))

  # --- completed-import verdicts (row 8) --------------------------------------
  _finalize_completed_imports(snap, workers, managers, mappings, unresolved)

  # --- target collisions and alias conflicts ----------------------------------
  _check_target_collisions(snap, workers, managers, unresolved)
  _check_alias_conflicts(snap, alias_old_sessions, alias_old_threads, unresolved)

  plan = ConversionPlan(
      managers=managers, workers=workers, prompt_bodies=sorted(prompt_bodies.values(), key=lambda b: b.ref),
      alias_old_sessions=alias_old_sessions, alias_old_threads=alias_old_threads,
      cron_rewrites=cron_rewrites, trigger_moves=trigger_moves,
      mappings=mappings, unresolved=unresolved,
      organization_pending=organization_pending,
      input_summary=_input_summary(mappings, managers, workers))
  return plan


def _manager_turn_request_id_by_dir(dir_name: str) -> str:
  return f"{_MANAGER_TURN_REQUEST_PREFIX}:{dir_name}"


def _is_our_cron_binding(body: dict, bound: str, task_name: str,
                         managers: list[ManagerConversion]) -> bool:
  """Whether an existing session_id binding is this plan's own applied product.

  True only when the bound session is one of this plan's manager conversions
  and the binding is exactly what an unbound file with this body would derive
  (mode master → the PM session of the body's project, recovered from the
  manager's own project_key; worker/steps → the active scheduled session
  carrying the task name). A different binding is foreign and preserved.
  """
  manager = next((m for m in managers if m.session_id == bound), None)
  if manager is None:
    return False
  if body.get("mode") == "master":
    return manager.kind == "pm"
  return manager.kind == "scheduled" and manager.metadata.scheduled_task == task_name


def _resolve_cron_target(snap: SourceSnapshot, body: dict, managers: list[ManagerConversion],
                         unresolved: list[UnresolvedEntry], rel_path: str) -> str | None:
  """The stable manager id one unbound cron task binds to.

  A mode:master task binds its declared project group's PM session; a
  worker/steps task binds the active scheduled session carrying the task's
  name. Zero or several active sessions with that name is ambiguous and stays
  unresolved — a guess would point scheduled fires at the wrong node.
  """
  task_name = task_name_of(rel_path)
  if body.get("mode") == "master":
    project = body.get("project")
    if isinstance(project, str) and project:
      pm = next((m for m in managers if m.kind == "pm" and m.metadata.project_key == project), None)
      if pm is None:
        unresolved.append(UnresolvedEntry(
            source_kind="cron_config", source_id=rel_path,
            reason=f"mode master task declares project {project!r} but no PM session carries that group",
            refs=[rel_path]))
        return None
      return pm.session_id
    unresolved.append(UnresolvedEntry(
        source_kind="cron_config", source_id=rel_path,
        reason="mode master task has neither an explicit session_id nor a project group to bind",
        refs=[rel_path]))
    return None
  active = [
      m for m in managers
      if m.kind == "scheduled" and m.metadata.scheduled_task == task_name
      and m.metadata.status.value == "active"]
  if len(active) == 1:
    return active[0].session_id
  archived_count = sum(
      1 for m in managers
      if m.kind == "scheduled" and m.metadata.scheduled_task == task_name)
  unresolved.append(UnresolvedEntry(
      source_kind="cron_config", source_id=rel_path,
      reason=(
          f"{len(active)} active scheduled session(s) named {task_name!r} "
          f"({archived_count} including archived); a worker/steps task needs exactly one "
          "active binding target"),
      refs=[rel_path]))
  return None


def task_name_of(rel_path: str) -> str:
  return Path(rel_path).stem


def _validate_rewritten_cron(cfg: CharlieBotConfig, path: Path, new_text: str, task_name: str) -> str | None:
  """Whether a rewritten cron file would load — through the loader itself.

  The rewritten text is written to a temporary file and read back by the real
  per-file loader, so the validation is the same one a running server applies
  (inline-prompt rejection, prompt pointer resolution, model validation).
  """
  import tempfile

  from src.core.config import _load_cron_file

  with tempfile.TemporaryDirectory() as tmp:
    temp_path = Path(tmp) / path.name
    temp_path.write_text(new_text, encoding="utf-8")
    try:
      _load_cron_file(temp_path, cfg.charlie_bot_repo, task_name)
      return None
    except Exception as e:
      return str(e)


def _input_summary(mappings: list[MappingEntry], managers: list[ManagerConversion],
                   workers: list[WorkerNodeProduct]) -> dict:
  counts: dict[str, int] = {}
  for entry in mappings:
    key = f"{entry.source_kind}:{entry.disposition}"
    counts[key] = counts.get(key, 0) + 1
  return {
      "mappings": len(mappings),
      "managers": len(managers),
      "workers": len(workers),
      "pending_inputs": sum(len(m.pending_inputs) for m in managers),
      "dispositions": dict(sorted(counts.items())),
  }


def _thread_outcome(meta: ThreadMetadata) -> str | None:
  """The run outcome the thread's recorded terminal status proves (None = open)."""
  if meta.status == ThreadStatus.COMPLETED:
    return "success"
  if meta.status == ThreadStatus.FAILED:
    return "failed"
  if meta.status in (ThreadStatus.CANCELLED, ThreadStatus.RUNNING):
    # Cancelled and interrupted runs keep their evidence; RUNNING threads only
    # plan as terminal when quiescence proved their process dead (apply
    # re-checks live/unknown ownership before any write).
    return "interrupted"
  return None


def _worktree_landing_proven(thread: ThreadInfo) -> tuple[bool, str | None]:
  """Whether the implement work provably landed on its base branch.

  The migration's offline form of the ``landed:`` evidence check the completion
  owner enforces: the recorded repo, work branch and base branch must all
  exist and git must show the work branch's tip reachable from the base branch
  tip. A missing repo or branch is missing evidence, never a proven landing;
  the check is read-only against the recorded repo.
  """
  repo_path = thread.meta.repo_path
  branch = thread.meta.branch_name
  base = thread.meta.base_branch
  if not repo_path or not branch or not base:
    return False, "recorded repo/branch/base_branch do not name a checkable landing"
  repo = Path(repo_path)
  if not (repo / ".git").exists():
    return False, f"recorded repo {repo_path} is not present; landing cannot be checked"

  def git(*args: str, check: bool = True) -> str:
    result = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, timeout=30, check=False)
    if check and result.returncode != 0:
      raise ValueError(f"git {' '.join(args[:2])}: {result.stderr.decode('utf-8', 'replace')[:200]}")
    return result.stdout.decode("utf-8", "replace").strip()

  try:
    tip = git("rev-parse", "--verify", branch)
    git("rev-parse", "--verify", base)
    landed = subprocess.run(
        ["git", "-C", str(repo), "merge-base", "--is-ancestor", tip, base],
        capture_output=True, timeout=30, check=False)
  except (ValueError, OSError, subprocess.TimeoutExpired) as e:
    return False, f"git landing check failed: {e}"
  if landed.returncode != 0:
    return False, f"work branch {branch!r} is not reachable from base {base!r}"
  return True, None


def _improve_loop_association(
    snap: SourceSnapshot,
    session: SessionInfo,
    thread: ThreadInfo,
    match: re.Match,
) -> tuple[LoopInfo | None, str | None]:
  """The one loop an iteration thread provably belongs to, or an ambiguity reason.

  Association needs causal identity, never the description prefix alone: the
  loop's goal text, work branch and repo must match the thread's recorded
  execution context, the thread must postdate the loop's creation, and the
  loop directory must hold this iteration's report. Zero or several candidate
  loops is unresolved.
  """
  iteration = int(match.group(1))
  goal_text = thread.meta.description
  candidates: list[LoopInfo] = []
  for loop in session.loops:
    state = loop.state
    if state is None:
      continue
    if state.goal and state.goal not in goal_text:
      continue
    if thread.meta.branch_name and state.work_branch and thread.meta.branch_name != state.work_branch:
      continue
    if thread.meta.repo_path and state.repo_path and thread.meta.repo_path != state.repo_path:
      continue
    if thread.meta.created_at and state.created_at:
      try:
        from src.core.models import ensure_utc
        loop_start = ensure_utc(datetime.fromisoformat(state.created_at))
        if thread.meta.created_at < loop_start:
          continue
      except ValueError:
        pass
    report_name = f"iter_{iteration:04d}.md"
    if report_name not in loop.report_files and (loop.dir / report_name).exists() is False:
      continue
    candidates.append(loop)
  if len(candidates) == 1:
    return candidates[0], None
  if not candidates:
    return None, ("no improve loop's recorded goal/branch/repo/report evidence matches this "
                  "iteration thread")
  return None, (f"{len(candidates)} improve loops match this iteration thread's evidence; "
                "the association is ambiguous")


def _plan_thread(
    snap: SourceSnapshot,
    session: SessionInfo,
    thread: ThreadInfo,
    owner_canonical: str,
    owner_manager: ManagerConversion | None,
    managers: list[ManagerConversion],
    workers: list[WorkerNodeProduct],
    thread_targets: dict[tuple[str, str], WorkerNodeProduct],
    mappings: list[MappingEntry],
    unresolved: list[UnresolvedEntry],
    alias_old_threads: dict[str, dict],
    prompt_bodies: dict[str, PromptBodyWrite],
) -> None:
  """Plan one legacy thread's conversion (rows 2-5 of the approved table)."""
  meta = thread.meta
  original_owner = session.id
  thread_key = (original_owner, meta.id)
  if thread_key in thread_targets:
    return  # already planned (e.g. the review pass below)

  if meta.review_of:
    target = thread_targets.get((original_owner, meta.review_of))
    if target is None:
      reviewed = next((t for t in session.threads if t.meta.id == meta.review_of), None)
      if reviewed is None:
        unresolved.append(UnresolvedEntry(
            source_kind="review_thread", source_id=f"{original_owner}/{meta.id}",
            reason=f"review_of names thread {meta.review_of}, which has no record in this session",
            refs=[str(thread.meta_path)]))
        return
      if reviewed.meta.review_of:
        unresolved.append(UnresolvedEntry(
            source_kind="review_thread", source_id=f"{original_owner}/{meta.id}",
            reason="review_of names another review thread; reviews chain to the work thread, "
                   "never to another review",
            refs=[str(thread.meta_path)]))
        return
      unresolved.append(UnresolvedEntry(
          source_kind="review_thread", source_id=f"{original_owner}/{meta.id}",
          reason=f"reviewed thread {meta.review_of} produced no worker target",
          refs=[str(thread.meta_path)]))
      return
    _append_review_run(snap, target, thread, mappings, unresolved, alias_old_threads)
    return

  if meta.chain_root:
    _plan_cron_chain(snap, session, thread, owner_canonical, managers, workers,
                     thread_targets, mappings, unresolved, alias_old_threads)
    return

  match = _IMPROVE_ITERATION_RE.match(meta.description or "")
  if match:
    loop, ambiguity = _improve_loop_association(snap, session, thread, match)
    if loop is None:
      unresolved.append(UnresolvedEntry(
          source_kind="improve_iteration", source_id=f"{original_owner}/{meta.id}",
          reason=ambiguity or "iteration association is ambiguous",
          refs=[str(thread.meta_path)]))
      return
    _plan_improve_iteration(snap, session, thread, owner_canonical, managers, workers, loop, match,
                            thread_targets, mappings, unresolved, alias_old_threads, prompt_bodies)
    return

  _plan_work_thread(snap, session, thread, owner_canonical, managers, workers,
                    thread_targets, mappings, unresolved, alias_old_threads)


def _expected_manager_metadata_bytes(manager: ManagerConversion) -> str:
  return manager.metadata.model_dump_json(indent=2, exclude=_TRANSIENT_METADATA_FIELDS)


def _plan_manager_conversion(
    snap: SourceSnapshot,
    sid: str,
    info: SessionInfo,
    canonical: dict[str, str],
    predecessor_of: dict[str, list[str]],
    pm_of_group: dict[str, str],
    project_bodies: dict[str, str],
    projects: dict[str, "ProjectInfo"],
    managers: list[ManagerConversion],
    mappings: list[MappingEntry],
    unresolved: list[UnresolvedEntry],
    prompt_bodies: dict[str, PromptBodyWrite],
    organization_pending: list[dict],
) -> None:
  """Plan one legacy session's conversion to a manager node (original id kept).

  Rows 1, 6, 8, 9, 10 and the manager-turn-log coverage of the approved
  table. Called for legacy sessions and — for resume recognition — for v2
  sessions this plan may have converted itself (the caller compares bytes).
  """
  meta = info.meta
  assert meta is not None
  sid = info.id
  kind = _session_kind(meta)
  archived = meta.status.value == "archived"
  goal = meta.name
  subtree_ref: str | None = None
  node_ref: str | None = None
  unproven: str | None = None
  if kind == "pm" and meta.group:
    body_ref = project_bodies.get(meta.group)
    if body_ref is not None:
      subtree_ref = body_ref
    project = projects.get(meta.group)
    if project is not None and project.supplement_body is not None:
      pm_goal, node_rules, rest = _classify_pm_supplement(project.supplement_body)
      if pm_goal:
        goal = pm_goal
      if node_rules:
        node_ref = hashlib.sha256(node_rules.encode("utf-8")).hexdigest()
        prompt_bodies[node_ref] = PromptBodyWrite(ref=node_ref, text=node_rules)
      if rest:
        unresolved.append(UnresolvedEntry(
            source_kind="pm_supplement", source_id=f"projects/{meta.group}",
            reason="manager supplement contains prose outside the explicit "
                   f"{_PM_TASK_GOAL_MARKER}/{_PM_NODE_RULES_MARKER} markers; classify it "
                   "as goal, node rule, or history before apply",
            refs=[project.supplement_body_rel_path or f"projects/{meta.group}/"]))
  elif kind == "ordinary" and meta.group and meta.group in project_bodies:
    # An unorganized root in an enabled project keeps the old applicable
    # scope (the shared body version) and is listed for organization.
    if meta.task_parent_id is None and not predecessor_of.get(sid):
      subtree_ref = project_bodies[meta.group]
      organization_pending.append({
          "group": meta.group, "item": f"unorganized root session {sid} references the shared "
          "project body version; organize its scope",
          "refs": [f"sessions/{sid}/metadata.json"]})

  converted = meta.model_copy(deep=True)
  converted.schema_version = 2
  converted.profile = "manager"
  converted.task_parent_id = None  # roots by default; explicit parent evidence is review-only
  converted.task = TaskSpec(goal=goal)
  converted.project_key = meta.group
  converted.presentation = "hidden" if archived else "auto"
  if kind == "scheduled" and archived:
    converted.automation_paused = True  # old archived scheduled sessions import paused
  converted.subtree_prompt_ref = subtree_ref
  converted.node_prompt_ref = node_ref
  converted.native_prompt_hash = None  # never manufacture prompt provenance
  converted.native_backend = None
  converted.native_model = None
  converted.master_run = None  # in-flight state is not carried into a v2 node

  # Proven manager turns become manager_turn Runs on this logical manager
  # (an elone tail also carries its predecessors' proven turns).
  turn_runs: list[RunProduct] = []
  turn_owners = [sid, *predecessor_of.get(sid, [])]
  for owner_id in turn_owners:
    owner_info = snap.sessions.get(owner_id)
    if owner_info is None:
      continue
    for log in sorted(owner_info.manager_turn_logs,
                      key=lambda turn_log: (turn_log.started_at
                                            or datetime.min.replace(tzinfo=UTC), turn_log.dir_name)):
      request_id = _manager_turn_request_id(log)
      run_id = _run_id_for(sid, request_id)
      if log.proven and log.outcome is not None:
        outcome_exit = 0 if log.outcome == "success" else 1
        record = RunRecord(
            id=run_id, session_id=sid, kind="manager_turn",
            started_at=log.started_at,
            ended_at=log.completed_at,
            exit_code=outcome_exit,
            backend=meta.backend or None,
            raw_log_ref=str(log.raw_path) if log.raw_path else None,
            result_ref=str(log.raw_path) if log.raw_path else None,
        )
        turn_runs.append(RunProduct(
            record=record, outcome=log.outcome,
            ended_at=log.completed_at,
            exit_code=outcome_exit))
        mappings.append(MappingEntry(
            source_kind="manager_turn_log", source_id=f"{owner_id}/{log.dir_name}",
            target_session_id=sid, target_run_id=run_id,
            disposition="manager_turn_run",
            detail={"raw_log_ref": str(log.raw_path) if log.raw_path else "",
                    "outcome": log.outcome}))
      else:
        mappings.append(MappingEntry(
            source_kind="manager_turn_log", source_id=f"{owner_id}/{log.dir_name}",
            disposition="historical_evidence",
            reason="manager execution log without a provable completed turn; retained at "
                   "its original path as historical evidence, not imported as a Run",
            detail={"raw_log_ref": str(log.raw_path) if log.raw_path else ""}))

  # Re-deriving a plan over an already-migrated node classifies only the
  # ORIGINAL history: this converter's own appended facts are not old
  # evidence, and post-import inputs belong to the ordinary fold, never to a
  # re-planned manifest's pending list. The imported runs' ids are known only
  # after the turn-run construction above, so classification follows it.
  if meta.profile is not None:
    migrated_run_ids = {str(p.record.id) for p in turn_runs}
    appended_ids = _migration_appended_event_ids(info, migrated_run_ids)
    classified_events = [e for e in info.events if e.get("id") not in appended_ids]
  else:
    classified_events = info.events
  pending, confirmed, session_uncertain = _classify_inputs(
      info, classified_events, meta, unresolved,
      _user_round_activity(classified_events)["done_by_input"],
      [log for log in info.manager_turn_logs if log.proven],
      meta.master_run if meta.profile is None else None,
  )
  unresolved.extend(session_uncertain)

  confirmed_map: dict[str, str] = {}
  for input_id, correlation in confirmed:
    if correlation == "master_run" and meta.master_run is not None and meta.master_run.raw_log:
      # Bind to the run derived from the recorded in-flight turn's log dir.
      log_dir = Path(meta.master_run.raw_log).parent.name
      run_id = _run_id_for(sid, _manager_turn_request_id_by_dir(log_dir))
    elif correlation == "master_done_unbound":
      # Proven handled by the MASTER_DONE fact itself; no turn log binds it.
      continue
    else:
      run_id = _run_id_for(sid, _manager_turn_request_id_by_dir(correlation))
    confirmed_map[input_id] = run_id
    for run_product in turn_runs:
      if run_product.record.id == run_id:
        # The run's registered batch is the exact input it provably consumed;
        # its run_finished fact acknowledges exactly this batch (the fold's
        # handled-input confirmation).
        if input_id not in run_product.record.input_event_ids:
          run_product.record.input_event_ids.append(input_id)
          run_product.input_event_ids.append(input_id)

  source_refs = [f"sessions/{sid}/metadata.json"]
  if info.chat_rel_path:
    source_refs.append(info.chat_rel_path)
  source_refs.extend(info.archive_rel_paths)

  kind_disposition = {
      "ordinary": "manager_root", "pm": "manager_pm", "scheduled": "manager_scheduled"}[kind]
  detail: dict = {"archived": archived}
  if kind == "scheduled":
    detail["scheduled_task"] = meta.scheduled_task or ""
    detail["import_paused"] = bool(archived)
  if archived and kind != "scheduled":
    unproven = ("old archived session metadata alone does not prove delivery; the task "
                "stays open (hidden) with its original evidence")
    detail["unproven_delivery"] = unproven
  mappings.append(MappingEntry(
      source_kind=kind, source_id=sid, target_session_id=sid,
      disposition=kind_disposition, detail=detail, reason=unproven))

  # The task_imported boundary event: written LAST for this node, after all
  # conversion facts are durable.
  task_imported = {
      "id": str(uuid.uuid5(TASK_ID_NAMESPACE, f"task-imported:{sid}")),
      "type": ET.TASK_IMPORTED,
      "actor": "system",
      "source_session_id": sid,
      "source_refs": source_refs,
      "pending_inputs": [
          {"source_ref": entry["source_ref"], "input_id": entry["input_id"]}
          for entry in pending],
  }
  managers.append(ManagerConversion(
      session_id=sid, kind=kind, canonical_id=sid, metadata=converted,
      task_imported=task_imported, pending_inputs=pending,
      manager_turn_runs=turn_runs, confirmed_inputs=confirmed_map,
      prompt_bodies=[], source_refs=source_refs, unproven_delivery=unproven))


def _owner_hint(snap: SourceSnapshot, managers: list[ManagerConversion],
                owner_canonical: str) -> tuple[bool, str | None]:
  """(archived, backend) of the owner manager, from the conversion when planned
  and from the scanned metadata otherwise (a resumed apply's already-published
  manager is deferred at thread-planning time, but its status/backend never
  change in conversion)."""
  manager = _manager_of(managers, owner_canonical)
  if manager is not None:
    return manager.metadata.status.value == "archived", manager.metadata.backend or None
  info = snap.sessions.get(owner_canonical)
  if info is not None and info.meta is not None:
    return info.meta.status.value == "archived", info.meta.backend or None
  return False, None


def _worker_target_id(owner_canonical: str, request_id: str) -> str:
  return stable_task_id(owner_canonical, request_id)


def _new_worker_metadata(
    *,
    target_id: str,
    owner_canonical: str,
    name: str,
    goal: str,
    thread: ThreadInfo | None,
    created_at: datetime,
    hidden: bool,
    task_type: object | None,
    repo_path: str | None,
    base_branch: str | None,
    backend: str | None,
    keep_worktree: bool = False,
) -> SessionMetadata:
  task = TaskSpec(
      goal=goal,
      repo_path=repo_path,
      base_branch=base_branch,
      task_type=task_type,  # type: ignore[arg-type]
      keep_worktree=keep_worktree,
  )
  return SessionMetadata(
      id=target_id,
      name=name[:80] or "Migrated worker task",
      schema_version=2,
      profile="worker",
      task=task,
      task_parent_id=owner_canonical,
      backend=backend or "",
      created_at=created_at,
      updated_at=created_at,
      presentation="hidden" if hidden else "auto",
  )


def _run_from_thread(
    target_id: str,
    request_id: str,
    thread: ThreadInfo,
    *,
    kind: str,
    sequence_ref: dict | None = None,
    review_of_run_id: str | None = None,
    retry_of_run_id: str | None = None,
) -> RunProduct:
  meta = thread.meta
  run_id = _run_id_for(target_id, request_id)
  record = RunRecord(
      id=run_id,
      session_id=target_id,
      kind=kind,  # type: ignore[arg-type]
      started_at=meta.started_at,
      ended_at=meta.completed_at,
      exit_code=meta.exit_code,
      backend=meta.backend or None,
      model=meta.model or None,
      native_session_id=meta.claude_session_id,
      repo_path=meta.repo_path,
      base_branch=meta.base_branch,
      branch_name=meta.branch_name,
      worktree_path=meta.worktree_path,
      review_of_run_id=review_of_run_id,
      retry_of_run_id=retry_of_run_id,
      sequence_ref=sequence_ref,  # type: ignore[arg-type]
      raw_log_ref=str(thread.raw_log_path) if thread.raw_log_path else None,
      events_ref=str(thread.events_path) if thread.events_path.is_file() else None,
      result_ref=str(thread.events_path) if thread.events_path.is_file() else None,
  )
  return RunProduct(record=record, outcome=_thread_outcome(meta),
                    ended_at=meta.completed_at, exit_code=meta.exit_code)


def _register_worker(
    workers: list[WorkerNodeProduct],
    thread_targets: dict[tuple[str, str], WorkerNodeProduct],
    alias_old_threads: dict[str, dict],
    *,
    target_id: str,
    owner_canonical: str,
    original_owner: str,
    metadata: SessionMetadata,
    source_refs: list[str],
) -> WorkerNodeProduct:
  task_imported = {
      "id": str(uuid.uuid5(TASK_ID_NAMESPACE, f"task-imported:{target_id}")),
      "type": ET.TASK_IMPORTED,
      "actor": "system",
      "source_session_id": target_id,
      "source_refs": source_refs,
      "pending_inputs": [],
  }
  product = WorkerNodeProduct(
      target_id=target_id, owner_id=owner_canonical, original_owner_id=original_owner,
      metadata=metadata, runs=[], task_imported=task_imported, source_refs=source_refs)
  workers.append(product)
  return product


def _plan_work_thread(
    snap: SourceSnapshot,
    session: SessionInfo,
    thread: ThreadInfo,
    owner_canonical: str,
    managers: list[ManagerConversion],
    workers: list[WorkerNodeProduct],
    thread_targets: dict[tuple[str, str], WorkerNodeProduct],
    mappings: list[MappingEntry],
    unresolved: list[UnresolvedEntry],
    alias_old_threads: dict[str, dict],
) -> None:
  meta = thread.meta
  original_owner = session.id
  request_id = _worker_request_id(original_owner, meta.id)
  target_id = _worker_target_id(owner_canonical, request_id)
  existing = thread_targets.get(thread_key_of(original_owner, meta.id))
  if existing is not None:
    return
  owner_archived, owner_backend = _owner_hint(snap, managers, owner_canonical)
  hidden = owner_archived
  created_at = meta.created_at
  metadata = _new_worker_metadata(
      target_id=target_id, owner_canonical=owner_canonical,
      name=(meta.description or "").strip().splitlines()[0] if (meta.description or "").strip() else "Migrated worker task",
      goal=meta.description or "",
      thread=thread, created_at=created_at, hidden=hidden,
      task_type=meta.task_type, repo_path=meta.repo_path, base_branch=meta.base_branch,
      backend=meta.backend or owner_backend or None,
      keep_worktree=meta.keep_worktree)
  run = _run_from_thread(target_id, request_id, thread, kind="work")
  source_refs = [str(thread.meta_path)]
  if thread.events_path.is_file():
    source_refs.append(thread.events_path.relative_to(snap.home).as_posix())
  if thread.raw_log_path is not None:
    source_refs.append(thread.raw_log_path.relative_to(snap.home).as_posix())
  product = _register_worker(
      workers, thread_targets, alias_old_threads,
      target_id=target_id, owner_canonical=owner_canonical, original_owner=original_owner,
      metadata=metadata, source_refs=source_refs)
  product.runs.append(run)
  product.completion_thread = thread
  thread_targets[thread_key_of(original_owner, meta.id)] = product
  alias_old_threads[f"{original_owner}/{meta.id}"] = {
      "session_id": target_id, "run_id": run.record.id}
  mappings.append(MappingEntry(
      source_kind="worker_thread", source_id=f"{original_owner}/{meta.id}",
      target_session_id=target_id, target_run_id=run.record.id,
      disposition="worker_work",
      detail={
          "old_status": meta.status.value,
          "old_pid": meta.pid,
          "require_review": meta.require_review,
          "task_type": meta.task_type.value if meta.task_type else None,
          "raw_log_ref": run.record.raw_log_ref or "",
          "events_ref": run.record.events_ref or "",
      }))


def thread_key_of(owner: str, thread_id: str) -> tuple[str, str]:
  return (owner, thread_id)


def _manager_of(managers: list[ManagerConversion], session_id: str) -> ManagerConversion | None:
  return next((m for m in managers if m.session_id == session_id), None)


def _append_review_run(
    snap: SourceSnapshot,
    work_product: WorkerNodeProduct,
    thread: ThreadInfo,
    mappings: list[MappingEntry],
    unresolved: list[UnresolvedEntry],
    alias_old_threads: dict[str, dict],
) -> None:
  """One review thread becomes a review Run of the reviewed worker's work Run.

  Chained reviews (several review threads naming the same original) chain by
  ``retry_of_run_id`` in creation order — the old reviewer-retry relationship,
  preserved exactly.
  """
  meta = thread.meta
  original_owner = work_product.original_owner_id
  request_id = _worker_request_id(original_owner, meta.id)
  work_runs = [r for r in work_product.runs if r.record.kind == "work"]
  if not work_runs:
    unresolved.append(UnresolvedEntry(
        source_kind="review_thread", source_id=f"{original_owner}/{meta.id}",
        reason="reviewed worker target carries no work Run to review",
        refs=[str(thread.meta_path)]))
    return
  work_run_id = work_runs[0].record.id
  previous_reviews = sorted(
      (r for r in work_product.runs if r.record.kind == "review" and r.record.review_of_run_id == work_run_id),
      key=lambda r: (r.record.started_at or datetime.min.replace(tzinfo=UTC), r.record.id))
  retry_of = previous_reviews[-1].record.id if previous_reviews else None
  run = _run_from_thread(
      work_product.target_id, request_id, thread, kind="review",
      review_of_run_id=work_run_id, retry_of_run_id=retry_of)
  work_product.runs.append(run)
  alias_old_threads[f"{original_owner}/{meta.id}"] = {
      "session_id": work_product.target_id, "run_id": run.record.id}
  mappings.append(MappingEntry(
      source_kind="review_thread", source_id=f"{original_owner}/{meta.id}",
      target_session_id=work_product.target_id, target_run_id=run.record.id,
      disposition="worker_review",
      detail={"review_of_run_id": work_run_id, "retry_of_run_id": retry_of or "",
              "old_status": meta.status.value}))


def _plan_cron_chain(
    snap: SourceSnapshot,
    session: SessionInfo,
    thread: ThreadInfo,
    owner_canonical: str,
    managers: list[ManagerConversion],
    workers: list[WorkerNodeProduct],
    thread_targets: dict[tuple[str, str], WorkerNodeProduct],
    mappings: list[MappingEntry],
    unresolved: list[UnresolvedEntry],
    alias_old_threads: dict[str, dict],
) -> None:
  """One cron firing chain becomes one worker with ordered scheduled_step Runs."""
  meta = thread.meta
  original_owner = session.id
  chain_root = meta.chain_root or ""
  root_thread = next((t for t in session.threads if t.meta.id == chain_root), None)
  if root_thread is None:
    unresolved.append(UnresolvedEntry(
        source_kind="cron_chain", source_id=f"{original_owner}/{meta.id}",
        reason=f"chain_root {chain_root} has no thread record in this session",
        refs=[str(thread.meta_path)]))
    return
  request_id = f"cron-chain:{original_owner}/{chain_root}"
  target_id = _worker_target_id(owner_canonical, request_id)
  product = thread_targets.get((original_owner, chain_root))
  if product is None:
    owner_archived, owner_backend = _owner_hint(snap, managers, owner_canonical)
    hidden = owner_archived
    created_at = root_thread.meta.created_at
    metadata = _new_worker_metadata(
        target_id=target_id, owner_canonical=owner_canonical,
        name=(root_thread.meta.description or "Scheduled steps chain").strip().splitlines()[0],
        goal=root_thread.meta.description or "Scheduled steps chain",
        thread=root_thread, created_at=created_at, hidden=hidden,
        task_type=root_thread.meta.task_type, repo_path=root_thread.meta.repo_path,
        base_branch=root_thread.meta.base_branch,
        backend=root_thread.meta.backend or owner_backend or None,
        keep_worktree=root_thread.meta.keep_worktree)
    product = _register_worker(
        workers, thread_targets, alias_old_threads,
        target_id=target_id, owner_canonical=owner_canonical, original_owner=original_owner,
        metadata=metadata,
        source_refs=[str(root_thread.meta_path)])
    thread_targets[(original_owner, chain_root)] = product
  if meta.step_index is None:
    unresolved.append(UnresolvedEntry(
        source_kind="cron_chain", source_id=f"{original_owner}/{meta.id}",
        reason="chain thread carries chain_root without step_index; its chain position is unknown",
        refs=[str(thread.meta_path)]))
    return
  duplicate = [
      r for r in product.runs
      if r.record.sequence_ref is not None and r.record.sequence_ref.position == meta.step_index]
  if duplicate:
    unresolved.append(UnresolvedEntry(
        source_kind="cron_chain", source_id=f"{original_owner}/{meta.id}",
        reason=f"step_index {meta.step_index} is already claimed by thread "
               f"{duplicate[0].record.id} in chain {chain_root}",
        refs=[str(thread.meta_path)]))
    return
  sequence = {"kind": "cron_steps", "owner_ref": f"cron-chain:{original_owner}/{chain_root}",
              "position": meta.step_index}
  target_id = product.target_id
  run = _run_from_thread(
      target_id, _worker_request_id(original_owner, meta.id),
      thread, kind="scheduled_step", sequence_ref=sequence)
  product.runs.append(run)
  alias_old_threads[f"{original_owner}/{meta.id}"] = {
      "session_id": product.target_id, "run_id": run.record.id}
  mappings.append(MappingEntry(
      source_kind="cron_chain_step", source_id=f"{original_owner}/{meta.id}",
      target_session_id=product.target_id, target_run_id=run.record.id,
      disposition="worker_scheduled_step",
      detail={"chain_root": chain_root, "step_index": meta.step_index,
              "old_status": meta.status.value}))


def _plan_improve_iteration(
    snap: SourceSnapshot,
    session: SessionInfo,
    thread: ThreadInfo,
    owner_canonical: str,
    managers: list[ManagerConversion],
    workers: list[WorkerNodeProduct],
    loop: LoopInfo,
    match: re.Match,
    thread_targets: dict[tuple[str, str], WorkerNodeProduct],
    mappings: list[MappingEntry],
    unresolved: list[UnresolvedEntry],
    alias_old_threads: dict[str, dict],
    prompt_bodies: dict[str, PromptBodyWrite],
) -> None:
  """Improve loops become one worker with ordered iteration Runs.

  The old loop state file stays the controller's configuration/progress
  source: it is never rewritten; the new node's task spec and sequence
  positions derive from it and from the iteration reports.
  """
  from src.core.improve_command import ImproveState

  meta = thread.meta
  original_owner = session.id
  iteration = int(match.group(1))
  request_id = f"improve:{original_owner}/{loop.loop_id}"
  target_id = _worker_target_id(owner_canonical, request_id)
  product = thread_targets.get((original_owner, f"loop:{loop.loop_id}"))
  if product is None:
    state = loop.state
    assert isinstance(state, ImproveState)
    owner_archived, owner_backend = _owner_hint(snap, managers, owner_canonical)
    hidden = owner_archived
    created_at = thread.meta.created_at
    metadata = _new_worker_metadata(
        target_id=target_id, owner_canonical=owner_canonical,
        name=f"Improve loop {loop.loop_id}: {state.goal}"[:80],
        goal=state.goal,
        thread=thread, created_at=created_at, hidden=hidden,
        task_type=TaskType.IMPLEMENT, repo_path=state.repo_path,
        base_branch=state.base_branch,
        backend=state.backend or owner_backend or None)
    product = _register_worker(
        workers, thread_targets, alias_old_threads,
        target_id=target_id, owner_canonical=owner_canonical, original_owner=original_owner,
        metadata=metadata,
        source_refs=[loop.state_path.relative_to(snap.home).as_posix(),
                     str(thread.meta_path)])
    thread_targets[(original_owner, f"loop:{loop.loop_id}")] = product
    mappings.append(MappingEntry(
        source_kind="improve_loop", source_id=f"{original_owner}/{loop.loop_id}",
        target_session_id=target_id, disposition="worker_improve_loop",
        reason="the old loop state file remains the controller's configuration and "
               "progress source at its original path",
        detail={"loop_dir": loop.dir.relative_to(snap.home).as_posix(),
                "loop_status": state.status}))
  report_name = f"iter_{iteration:04d}.md"
  report_path = loop.dir / report_name
  sequence = {"kind": "improve", "owner_ref": loop.dir.relative_to(snap.home).as_posix(),
              "position": iteration}
  run = _run_from_thread(
      product.target_id, _worker_request_id(original_owner, meta.id), thread,
      kind="iteration", sequence_ref=sequence)
  if report_path.is_file():
    run.record.result_ref = str(report_path)
  product.runs.append(run)
  alias_old_threads[f"{original_owner}/{meta.id}"] = {
      "session_id": product.target_id, "run_id": run.record.id}
  mappings.append(MappingEntry(
      source_kind="improve_iteration", source_id=f"{original_owner}/{meta.id}",
      target_session_id=product.target_id, target_run_id=run.record.id,
      disposition="worker_iteration",
      detail={"loop_id": loop.loop_id, "iteration": iteration,
              "report": report_path.relative_to(snap.home).as_posix() if report_path.is_file() else "",
              "old_status": meta.status.value}))


def _finalize_completed_imports(
    snap: SourceSnapshot,
    workers: list[WorkerNodeProduct],
    managers: list[ManagerConversion],
    mappings: list[MappingEntry],
    unresolved: list[UnresolvedEntry],
) -> None:
  """Second pass: decide completed import for each work thread (row 8).

  Old completed metadata alone never proves delivery. A worker imports
  completed only with the full evidence set: a proven successful run, a
  successful review when the thread required one, and — for implement work —
  a provable target-branch landing. Everything else stays open with its
  evidence; an archived owner also keeps the task hidden, and the mapping
  records the explicit unproven-delivery reason.
  """
  from src.core.models import TaskType

  for product in workers:
    thread = product.completion_thread
    if thread is None:
      continue
    work_runs = [r for r in product.runs if r.record.kind == "work"]
    if not work_runs:
      continue
    run = work_runs[0]
    meta = thread.meta
    owner_manager = _manager_of(managers, product.owner_id)
    archived_owner = bool(owner_manager and owner_manager.metadata.status.value == "archived")
    unproven: str | None = None
    if run.outcome != "success":
      unproven = f"old thread status {meta.status.value} does not prove successful delivery"
    elif thread.events_path.is_file():
      events, errors = _read_ndjson_file(thread.events_path)
      results = [e for e in events if e.get("type") == ET.RESULT]
      if not results:
        unproven = "no retained result event proves the run's success"
      else:
        last = results[-1]
        if not (last.get("subtype") in (None, "success") and last.get("is_error") in (None, False)):
          unproven = "the retained result event records a failure"
      if errors:
        unproven = unproven or f"thread events log has unreadable lines: {errors[0]}"
    else:
      unproven = "thread events log missing; no retained result evidence"
    review_runs = [r for r in product.runs if r.record.kind == "review"]
    if unproven is None and meta.require_review:
      if not review_runs:
        unproven = "the thread required review and no review thread is retained"
      elif any(r.outcome != "success" for r in review_runs):
        unproven = "a retained review run did not succeed"
    if unproven is None and meta.task_type == TaskType.IMPLEMENT:
      landed, reason = _worktree_landing_proven(thread)
      if not landed:
        unproven = f"implement landing unproven: {reason}"
    entry = next(
        (m for m in mappings
         if m.source_kind == "worker_thread" and m.source_id == f"{product.original_owner_id}/{meta.id}"),
        None)
    assert entry is not None
    if unproven is None:
      entry.disposition = "worker_work_completed"
      entry.reason = None
      close_request = f"migration:{product.original_owner_id}/{meta.id}"
      close_event_id = stable_close_event_id(product.target_id, close_request)
      summary = f"Imported completed from old thread {meta.id} (status completed, exit 0)."
      result_refs = [r for r in (run.record.result_ref, run.record.raw_log_ref) if r]
      close_event = {
          "id": close_event_id,
          "type": ET.TASK_CLOSED,
          "timestamp": (meta.completed_at or meta.created_at).astimezone(UTC).isoformat(),
          "actor": "system",
          "source_session_id": product.target_id,
          "outcome": "completed",
          "summary": summary,
          "result_refs": result_refs,
          "run_ids": [r.record.id for r in product.runs],
          "report_to": product.owner_id,
      }
      report_id = stable_child_report_id(product.target_id, close_event_id, product.owner_id)
      report_event = {
          "id": report_id,
          "type": ET.CHILD_REPORT,
          "timestamp": (meta.completed_at or meta.created_at).astimezone(UTC).isoformat(),
          "actor": "system",
          "source_session_id": product.target_id,
          "child_session_id": product.target_id,
          "child_event_id": close_event_id,
          "outcome": "completed",
          "summary": summary,
          "result_refs": result_refs,
      }
      product.task_closed = close_event
      product.child_report = report_event
      product.metadata.presentation = "auto"
    else:
      entry.disposition = "worker_work_unproven"
      entry.reason = unproven
      entry.detail["unproven_delivery"] = unproven
      if archived_owner:
        entry.detail["kept_hidden"] = "archived owner preference retained; task open but hidden"


def _check_target_collisions(
    snap: SourceSnapshot,
    workers: list[WorkerNodeProduct],
    managers: list[ManagerConversion],
    unresolved: list[UnresolvedEntry],
) -> None:
  """A planned target id that an unrelated existing node occupies is a conflict.

  Existing v2 nodes from mixed input are preserved; a conversion may never
  reuse their ids. A legacy session's own manager conversion reuses that
  session's id by design (the original id is the product), so only worker
  targets are checked against every existing session directory.
  """
  for product in workers:
    existing = snap.sessions.get(product.target_id)
    if existing is None:
      continue
    existing_path = snap.home / "sessions" / product.target_id / "metadata.json"
    if existing.meta is not None and existing_path.is_file():
      try:
        existing_bytes = existing_path.read_text(encoding="utf-8")
      except OSError:
        existing_bytes = ""
      if existing_bytes == _expected_worker_metadata_bytes(product):
        continue  # this plan's own published product (resumed apply), not a collision
    unresolved.append(UnresolvedEntry(
        source_kind="target_collision", source_id=product.target_id,
        reason=f"derived worker target id already exists as session directory "
               f"({existing.meta.profile if existing.meta else 'unreadable'}); stable-id derivation "
               "collided with existing data",
        refs=[f"sessions/{product.target_id}/metadata.json"]))


def _check_alias_conflicts(
    snap: SourceSnapshot,
    alias_old_sessions: dict[str, str],
    alias_old_threads: dict[str, dict],
    unresolved: list[UnresolvedEntry],
) -> None:
  """Existing alias rows are preserved; a disagreement is a visible conflict."""
  raw = snap.aliases_raw or {}
  existing_sessions = raw.get("old_session_ids") or {}
  if isinstance(existing_sessions, dict):
    for old, canonical in alias_old_sessions.items():
      existing = existing_sessions.get(old)
      if existing is not None and existing != canonical:
        unresolved.append(UnresolvedEntry(
            source_kind="alias", source_id=old,
            reason=f"existing session alias maps {old} to {existing}; conversion derives {canonical}",
            refs=[snap.aliases_rel_path or ALIASES_FILE_NAME]))
  existing_threads = raw.get("old_threads") or {}
  if isinstance(existing_threads, dict):
    for key, target in alias_old_threads.items():
      existing = existing_threads.get(key)
      if existing is not None and existing != target:
        unresolved.append(UnresolvedEntry(
            source_kind="alias", source_id=key,
            reason=f"existing thread alias maps {key} to {existing}; conversion derives {target}",
            refs=[snap.aliases_rel_path or ALIASES_FILE_NAME]))


def converter_code_sha256() -> str:
  """The converting code's content hash (the code half of drift detection)."""
  import src.core.session_tree_migration as module

  digest = hashlib.sha256()
  for rel in CONVERTER_MODULE_PATHS:
    digest.update((Path(module.__file__).parent / Path(rel).name).read_bytes())
  return digest.hexdigest()


def source_sha_of(snap: SourceSnapshot) -> str:
  """The manifest's source binding: one hash over every hashed input file."""
  payload = "".join(
      f"{path}\0{snap.hashes[path]}\n" for path in sorted(snap.hashes))
  return _sha256_bytes(payload.encode("utf-8"))


# ---------------------------------------------------------------------------
# Quiescence (the stopped-writer boundary)
# ---------------------------------------------------------------------------


def _ancestor_pids(pid: int) -> set[int]:
  ancestors: set[int] = set()
  current = pid
  for _ in range(4096):
    try:
      with open(f"/proc/{current}/stat", encoding="utf-8") as f:
        content = f.read()
      ppid = int(content.rpartition(")")[2].split()[1])
    except (OSError, ValueError, IndexError):
      return ancestors
    if ppid <= 1:
      return ancestors
    ancestors.add(ppid)
    current = ppid
  return ancestors


def _process_env_binds_home(pid: int, home: Path) -> bool:
  """Whether /proc/<pid>/environ binds CHARLIEBOT_HOME to this exact home."""
  try:
    raw = Path(f"/proc/{pid}/environ").read_bytes()
  except OSError:
    return False
  home_str = str(home)
  for chunk in raw.split(b"\0"):
    if not chunk.startswith(b"CHARLIEBOT_HOME="):
      continue
    value = chunk[len(b"CHARLIEBOT_HOME="):].decode("utf-8", "replace")
    if value == home_str:
      return True
    try:
      if str(Path(value).expanduser().resolve()) == str(home.resolve()):
        return True
    except (OSError, RuntimeError, ValueError):
      continue
  return False


def _cmdline_of(pid: int) -> str:
  try:
    raw = Path(f"/proc/{pid}/cmdline").read_bytes()
  except OSError:
    return ""
  return raw.replace(b"\0", b" ").decode("utf-8", "replace").strip()


def scan_live_home_processes(home: Path) -> list[dict]:
  """Live processes whose environment binds them to this home (diagnostic).

  The current process and its ancestor chain are excluded: the CLI itself and
  the shell that exported CHARLIEBOT_HOME are not home writers. Everything
  else is a visible blocker; nothing is ever signalled.
  """
  mine = os.getpid()
  ancestors = _ancestor_pids(mine)
  hits: list[dict] = []
  for entry in os.scandir("/proc"):
    if not entry.name.isdigit():
      continue
    pid = int(entry.name)
    if pid == mine or pid in ancestors:
      continue
    if not _process_env_binds_home(pid, home):
      continue
    stat_pair = _read_pid_stat_quiet(pid)
    hits.append({
        "pid": pid,
        "cmdline": _cmdline_of(pid),
        "state": stat_pair[1] if stat_pair else "gone",
    })
  return hits


def _read_pid_stat_quiet(pid: int) -> tuple[str, str] | None:
  try:
    return _read_pid_stat_public(pid)
  except Exception:
    return None


def _read_pid_stat_public(pid: int) -> tuple[str, str] | None:
  from src.core.runs import read_pid_stat
  return read_pid_stat(pid)


def _recorded_process_verdict(pid: int | None, pid_start: str | None) -> str:
  """alive | dead | unknown for one recorded (pid, pid_start) pair."""
  if pid is None or pid_start is None:
    return "unknown"
  pair = _read_pid_stat_quiet(pid)
  if pair is None or pair[1] == "Z":
    return "dead"
  if pair[0] != pid_start:
    return "dead"  # the pid was reused: the recorded process is gone
  return "alive"


def quiescence_blockers(snap: SourceSnapshot) -> list[str]:
  """Every visible reason the source is not a proven stopped-writer home."""
  blockers: list[dict] = []
  fence = probe_writer_fence(snap.home)
  if fence.get("exclusive_holder_alive"):
    holder = fence.get("identity_recorded")
    if isinstance(holder, FenceHolder):
      blockers.append({"kind": "writer_fence", "detail": (
          f"home writer fence held by pid {holder.pid} (purpose {holder.purpose!r})")})
    else:
      blockers.append({"kind": "writer_fence",
                       "detail": "home writer fence is held; holder identity unreadable"})
  for hit in scan_live_home_processes(snap.home):
    blockers.append({"kind": "live_process", "pid": hit["pid"],
                     "detail": f"live process bound to this home: {hit['cmdline'] or hit['pid']}"})
  for sid in sorted(snap.sessions):
    info = snap.sessions[sid]
    if info.meta is None:
      continue
    for thread in info.threads:
      meta = thread.meta
      if meta.status != ThreadStatus.RUNNING:
        continue
      verdict = _recorded_process_verdict(meta.pid, meta.pid_start)
      if verdict == "alive":
        blockers.append({"kind": "live_worker", "pid": meta.pid,
                         "detail": f"thread {sid}/{meta.id} records a live process"})
      elif verdict == "unknown":
        blockers.append({"kind": "unknown_worker_ownership", "pid": meta.pid,
                         "detail": (f"thread {sid}/{meta.id} is marked running without a provable "
                                    f"process identity (pid={meta.pid}, pid_start={'set' if meta.pid_start else 'missing'})")})
      else:
        holders = ()
        if thread.raw_log_path is not None:
          try:
            holders = leftover_holders_for(
                thread.raw_log_path, scan_stdout_holders(), run_pid=meta.pid)
          except OSError:
            holders = ()
        if holders:
          blockers.append({"kind": "leftover_holder", "detail": (
              f"thread {sid}/{meta.id}'s raw log is still held by live descendant(s): "
              + ", ".join(f"{h.pid} {h.cmdline}" for h in holders))})
    record = info.meta.master_run
    if record is not None:
      verdict = _recorded_process_verdict(record.pid, record.pid_start)
      if verdict == "alive":
        blockers.append({"kind": "live_master_turn", "pid": record.pid,
                         "detail": f"session {sid} records a live in-flight master turn"})
      elif verdict == "unknown":
        blockers.append({"kind": "unknown_master_ownership", "pid": record.pid,
                         "detail": (f"session {sid}'s recorded in-flight master turn has no provable "
                                    "process identity")})
    for loop in info.loops:
      state = loop.state
      if state is None or getattr(state, "status", "") != "running":
        continue
      server_pid = getattr(state, "server_pid", None)
      if server_pid is None:
        blockers.append({"kind": "unknown_loop_ownership", "detail": (
            f"improve loop {sid}/{loop.loop_id} is marked running without a recorded "
            "controller process")})
        continue
      pair = _read_pid_stat_quiet(server_pid)
      if pair is not None and pair[1] != "Z":
        blockers.append({"kind": "live_loop_controller", "pid": server_pid,
                         "detail": f"improve loop {sid}/{loop.loop_id} has a live controller"})
    for trigger_info in info.triggers:
      trigger = trigger_info.trigger
      if trigger is None or trigger.status.value != "pending":
        continue
      for target in trigger.watch_targets:
        payload = target.model_dump()
        if payload.get("kind") == "local_pid":
          pair = _read_pid_stat_quiet(int(payload["pid"]))
          if pair is not None and pair[1] != "Z":
            blockers.append({"kind": "live_watch_target", "pid": payload["pid"],
                             "detail": (f"pending trigger {trigger_info.rel_path} watches live "
                                        f"local pid {payload['pid']}")})
        else:
          blockers.append({"kind": "unverifiable_watch_target", "detail": (
              f"pending trigger {trigger_info.rel_path} watches external work whose ownership "
              f"cannot be verified offline: {payload}")})
  return blockers


# ---------------------------------------------------------------------------
# Manifest build
# ---------------------------------------------------------------------------


def build_manifest(cfg: CharlieBotConfig, snap: SourceSnapshot) -> tuple[MigrationManifest, ConversionPlan]:
  """One dry-run product: the reviewable manifest plus the derived plan."""
  plan = build_conversion_plan(cfg, snap)
  source_files = [
      SourceFileRecord(path=path, sha256=snap.hashes[path], size=snap.files[path])
      for path in sorted(snap.hashes)]
  created: list[str] = []
  bodies_dir = cfg.charliebot_home / PROMPT_BODIES_DIR_NAME
  for body in plan.prompt_bodies:
    created.append((bodies_dir / f"{body.ref}.md").relative_to(cfg.charliebot_home).as_posix())
  for product in plan.workers:
    node = cfg.sessions_dir / product.target_id
    created.append((node / "metadata.json").relative_to(cfg.charliebot_home).as_posix())
    created.append((node / DATA_DIR_NAME / "chat_events.jsonl").relative_to(cfg.charliebot_home).as_posix())
    for run in product.runs:
      created.append((node / DATA_DIR_NAME / "runs" / run.record.id / "metadata.json")
                     .relative_to(cfg.charliebot_home).as_posix())
  for manager in plan.managers:
    created.append(f"sessions/{manager.session_id}/metadata.json")
    for run in manager.manager_turn_runs:
      created.append(f"sessions/{manager.session_id}/data/runs/{run.record.id}/metadata.json")
    created.append(f"sessions/{manager.session_id}/data/chat_events.jsonl")
  if plan.alias_old_sessions or plan.alias_old_threads:
    created.append(f"sessions/{ALIASES_FILE_NAME}")
  for rewrite in plan.cron_rewrites:
    created.append(rewrite.rel_path)
  for move in plan.trigger_moves:
    created.append(move.new_rel_path)
  manifest = MigrationManifest(
      created_at=datetime.now(UTC),
      converter_code_sha256=converter_code_sha256(),
      home_path=str(cfg.charliebot_home),
      source_sha=source_sha_of(snap),
      source_files=source_files,
      mappings=plan.mappings,
      unresolved=plan.unresolved,
      created_files=sorted(set(created)),
      input_summary=plan.input_summary,
  )
  return manifest, plan


# ---------------------------------------------------------------------------
# Apply
# ---------------------------------------------------------------------------


@dataclass
class _ApplyContext:
  cfg: CharlieBotConfig
  plan: ConversionPlan
  manifest: MigrationManifest
  state_dir: Path
  backup_dir: Path
  receipts_path: Path
  receipts: dict[str, ProductReceipt] = field(default_factory=dict)  # last receipt per path
  tree: "TaskTreeManager | None" = None


def _read_receipts(path: Path) -> dict[str, ProductReceipt]:
  receipts: dict[str, ProductReceipt] = {}
  if not path.is_file():
    return receipts
  for line in path.read_text(encoding="utf-8").splitlines():
    if not line.strip():
      continue
    try:
      receipt = ProductReceipt.model_validate_json(line)
    except ValueError as e:
      raise MigrationRefused(f"migration receipt journal unreadable at {path}: {e}") from e
    receipts[receipt.path] = receipt
  return receipts


def _append_receipt(ctx: _ApplyContext, receipt: ProductReceipt) -> None:
  ctx.receipts[receipt.path] = receipt
  ctx.receipts_path.parent.mkdir(parents=True, exist_ok=True)
  with open(ctx.receipts_path, "ab") as f:
    f.write(receipt.model_dump_json().encode("utf-8") + b"\n")
    f.flush()
    os.fsync(f.fileno())


def _hash_rel(cfg: CharlieBotConfig, rel: str) -> str | None:
  path = cfg.charliebot_home / rel
  if not path.is_file():
    return None
  return _sha256_file(path)


def _confined(cfg: CharlieBotConfig, rel: str) -> Path:
  """Resolve a home-relative product path, refusing symlink escapes.

  Every migration write stays inside the selected home: a symlinked path or
  parent that resolves outside the home is a refusal, never a write target.
  """
  home = cfg.charliebot_home.resolve()
  path = home / rel
  resolved = path.resolve()
  if not resolved.is_relative_to(home):
    raise MigrationRefused(f"product path {rel!r} resolves outside the home ({resolved})")
  current = path
  while True:
    if current.is_symlink():
      raise MigrationRefused(f"product path {rel!r} traverses the symlink {current}")
    if current == home:
      break
    current = current.parent
    if not str(current).startswith(str(home)):
      break
  return resolved


def _backup_file(ctx: _ApplyContext, rel: str, current_hash: str | None) -> str | None:
  """Copy the original bytes of *rel* into the backup dir before mutation."""
  if current_hash is None:
    return None  # the file does not exist yet; nothing to back up
  source = _confined(ctx.cfg, rel)
  dest = ctx.backup_dir / rel
  dest.parent.mkdir(parents=True, exist_ok=True)
  shutil.copyfile(source, dest)
  copied = _sha256_file(dest)
  if copied != current_hash:
    raise MigrationRefused(
        f"backup verification failed for {rel}: copied {copied}, source {current_hash}")
  return rel


def _expected_worker_metadata_bytes(product: WorkerNodeProduct) -> str:
  return product.metadata.model_dump_json(indent=2, exclude=_TRANSIENT_METADATA_FIELDS)


def _expected_run_bytes(record: RunRecord) -> str:
  return record.model_dump_json(indent=2)


def _log_contains_event(cfg: CharlieBotConfig, session_id: str, event_id: str) -> bool:
  """Whether the node's durable history already holds this event id."""
  path = cfg.sessions_dir / session_id / DATA_DIR_NAME / "chat_events.jsonl"
  if not path.is_file():
    return False
  try:
    raw = path.read_bytes()
  except OSError as e:
    raise MigrationRefused(f"chat log unreadable at {path}: {e}") from e
  for line in raw.split(b"\n"):
    if not line.strip():
      continue
    try:
      event = orjson.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict) and event.get("id") == event_id:
      return True
  return False


def _append_fact_if_absent(cfg: CharlieBotConfig, session_id: str, event: dict) -> tuple[str, bool]:
  """Append one control fact unless its stable id is already in the log.

  Idempotent by event id: an interrupted apply re-derives the same fact and
  finds the landed copy instead of duplicating it. Returns (log hash after the
  (non-)write, whether this call wrote).
  """
  from src.core.ndjson import append_ndjson_sync

  event_id = str(event.get("id"))
  if not event_id:
    raise MigrationRefused(f"refusing to append a fact without a stable id to {session_id}")
  log_rel = f"sessions/{session_id}/data/chat_events.jsonl"
  if _log_contains_event(cfg, session_id, event_id):
    return _hash_rel(cfg, log_rel) or "", False
  path = cfg.sessions_dir / session_id / DATA_DIR_NAME / "chat_events.jsonl"
  path.parent.mkdir(parents=True, exist_ok=True)
  append_ndjson_sync(path, event)
  return _sha256_file(path), True


def _validate_manifest_shape(manifest: MigrationManifest) -> None:
  if manifest.schema_version != MANIFEST_SCHEMA_VERSION:
    raise MigrationRefused(
        f"manifest schema_version {manifest.schema_version} is not supported "
        f"(this converter writes {MANIFEST_SCHEMA_VERSION})")
  for record in manifest.source_files:
    rel = record.path
    if rel.startswith("/") or ".." in Path(rel).parts or not rel:
      raise MigrationRefused(f"manifest source path {rel!r} is not a confined relative path")


def _expected_product_hashes(cfg: CharlieBotConfig, plan: ConversionPlan) -> dict[str, str]:
  """Content hashes of this plan's deterministic replacement/created products.

  A crash between an atomic write and its receipt append leaves the product
  on disk with no receipt; the deterministic content proves the product is
  this manifest's own, so a resumed apply neither duplicates it nor mistakes
  it for drift.
  """
  expected: dict[str, str] = {}
  bodies_dir = cfg.charliebot_home / PROMPT_BODIES_DIR_NAME
  for body in plan.prompt_bodies:
    rel = (bodies_dir / f"{body.ref}.md").relative_to(cfg.charliebot_home).as_posix()
    expected[rel] = _sha256_bytes(body.text.encode("utf-8"))
  for product in plan.workers:
    node = f"sessions/{product.target_id}"
    expected[f"{node}/metadata.json"] = _sha256_bytes(
        _expected_worker_metadata_bytes(product).encode("utf-8"))
    for run in product.runs:
      expected[f"{node}/data/runs/{run.record.id}/metadata.json"] = _sha256_bytes(
          _expected_run_bytes(run.record).encode("utf-8"))
  for manager in plan.managers:
    expected[f"sessions/{manager.session_id}/metadata.json"] = _sha256_bytes(
        manager.metadata.model_dump_json(indent=2, exclude=_TRANSIENT_METADATA_FIELDS).encode("utf-8"))
    for run in manager.manager_turn_runs:
      expected[f"sessions/{manager.session_id}/data/runs/{run.record.id}/metadata.json"] = _sha256_bytes(
          _expected_run_bytes(run.record).encode("utf-8"))
  for rewrite in plan.cron_rewrites:
    expected[rewrite.rel_path] = _sha256_bytes(rewrite.new_text.encode("utf-8"))
  for move in plan.trigger_moves:
    expected[move.new_rel_path] = _sha256_bytes(move.trigger.model_dump_json(indent=2).encode("utf-8"))
  return expected


def _planned_fact_ids(cfg: CharlieBotConfig, plan: ConversionPlan) -> dict[str, set[str]]:
  """Per-node ids of every fact this plan appends (append-only proof of apply)."""
  planned: dict[str, set[str]] = {}
  for manager in plan.managers:
    ids = planned.setdefault(manager.session_id, set())
    for run in manager.manager_turn_runs:
      if run.outcome is not None:
        ids.add(run.record.id)  # run_finished facts carry the run id
    ids.add(str(manager.task_imported["id"]))
  for product in plan.workers:
    ids = planned.setdefault(product.target_id, set())
    for run in product.runs:
      if run.outcome is not None:
        ids.add(run.record.id)
    if product.task_closed is not None:
      ids.add(str(product.task_closed["id"]))
    ids.add(str(product.task_imported["id"]))
  return planned


def _check_drift(cfg: CharlieBotConfig, manifest: MigrationManifest, receipts: dict[str, ProductReceipt],
                 expected: dict[str, str], planned_fact_ids: dict[str, set[str]]) -> None:
  """Every manifest input must match the home, a receipt, or this plan's product.

  After a partial apply the replaced/appended files no longer match their
  pre-hashes; a receipt whose post-hash matches, or byte-identical expected
  product content, proves the change is this manifest's own product. Anything
  else is drift and refuses.
  """
  drifted: list[str] = []
  for record in manifest.source_files:
    rel = record.path
    current = _hash_rel(cfg, rel)
    if current == record.sha256:
      continue
    receipt = receipts.get(rel)
    if receipt is not None and receipt.pre_sha256 == record.sha256 and (
        current == receipt.post_sha256 or (receipt.kind == "removed" and current is None)):
      continue
    if current is not None and expected.get(rel) == current:
      continue
    if rel.endswith("chat_events.jsonl") and current is not None:
      node = rel.split("/")[1]
      raw = (cfg.charliebot_home / rel).read_bytes()
      present: set[str] = set()
      for line in raw.split(b"\n"):
        if not line.strip():
          continue
        event = _safe_json(line)
        if event is None:
          continue
        if event.get("id"):
          present.add(str(event["id"]))
        if event.get("run_id"):
          present.add(str(event["run_id"]))
      if planned_fact_ids.get(node) and planned_fact_ids[node].issubset(present):
        continue
    drifted.append(
        f"{rel}: manifest {record.sha256[:12]}, current {None if current is None else current[:12]}")
  if drifted:
    raise ManifestDriftError(
        "source drift: the home no longer matches the manifest's hash binding "
        "(regenerate the manifest with --dry-run); first mismatches: " + "; ".join(drifted[:8]),
        details=drifted)


def _safe_json(line: bytes) -> dict | None:
  try:
    value = orjson.loads(line)
  except ValueError:
    return None
  return value if isinstance(value, dict) else None


def _check_converter_code(manifest: MigrationManifest) -> None:
  current = converter_code_sha256()
  if current != manifest.converter_code_sha256:
    raise MigrationRefused(
        "converter code drift: this manifest was built by a different "
        f"session_tree_migration.py ({manifest.converter_code_sha256[:12]} vs {current[:12]}); "
        "regenerate it with --dry-run")


def _plan_matches_manifest(plan: ConversionPlan, manifest: MigrationManifest) -> None:
  """The rebuilt plan must be the manifest's plan (same input, same products)."""
  def key(entry: MappingEntry) -> tuple:
    return (entry.source_kind, entry.source_id, entry.target_session_id, entry.target_run_id)

  planned = sorted(key(m) for m in plan.mappings)
  recorded = sorted(key(m) for m in manifest.mappings)
  if planned != recorded:
    missing = [k for k in recorded if k not in set(planned)][:5]
    extra = [k for k in planned if k not in set(recorded)][:5]
    raise MigrationRefused(
        "manifest mismatch: rebuilding the plan from the manifest's bound source produces "
        f"different mappings (recorded-only: {missing}; derived-only: {extra}); regenerate "
        "the manifest with --dry-run")
  if plan.unresolved and not manifest.unresolved:
    raise MigrationRefused(
        "unresolved conversions discovered while rebuilding the plan: "
        + "; ".join(f"{u.source_kind}:{u.source_id} ({u.reason})" for u in plan.unresolved[:8]))
  if manifest.unresolved:
    raise MigrationRefused(
        "manifest lists unresolved conversions; resolve them and regenerate: "
        + "; ".join(f"{u.source_kind}:{u.source_id} ({u.reason})" for u in manifest.unresolved[:8]))


def _verify_receipts_intact(ctx: _ApplyContext) -> None:
  """Every receipted product must still match its recorded post state."""
  broken: list[str] = []
  for receipt in ctx.receipts.values():
    current = _hash_rel(ctx.cfg, receipt.path)
    if receipt.kind == "removed":
      if current is not None:
        broken.append(f"{receipt.path}: removed product reappeared ({current[:12]})")
      continue
    if current != receipt.post_sha256:
      broken.append(
          f"{receipt.path}: receipt {None if receipt.post_sha256 is None else receipt.post_sha256[:12]}, "
          f"current {None if current is None else current[:12]}")
  if broken:
    raise MigrationRefused(
        "applied products no longer match their receipts (a new-system write or an edit "
        "reached the migration's products); refusing to continue: " + "; ".join(broken[:8]),
        details=broken)


def _worker_state_dir(cfg: CharlieBotConfig, target_id: str) -> Path:
  return cfg.sessions_dir / target_id


def _publish_worker_node(cfg: CharlieBotConfig, product: WorkerNodeProduct) -> None:
  """Publish one new worker node atomically (temp dir + rename), idempotently."""
  sessions_dir = cfg.sessions_dir
  sessions_dir.mkdir(parents=True, exist_ok=True)
  final_dir = sessions_dir / product.target_id
  meta_bytes = _expected_worker_metadata_bytes(product)
  if final_dir.exists():
    existing_meta = final_dir / "metadata.json"
    if not existing_meta.is_file() or existing_meta.read_text(encoding="utf-8") != meta_bytes:
      raise MigrationRefused(
          f"target collision: {final_dir} exists with different content than this manifest's "
          "worker product; refusing to overwrite")
    return
  temp_dir = sessions_dir / f".task-{product.target_id}-{os.getpid()}-{uuid.uuid4().hex}.tmp"
  (temp_dir / "data").mkdir(parents=True)
  (temp_dir / "threads").mkdir()
  (temp_dir / "data" / "chat_events.jsonl").touch()
  for run in product.runs:
    run_dir = temp_dir / "data" / "runs" / run.record.id
    run_dir.mkdir(parents=True)
    atomic_write_text(run_dir / "metadata.json", _expected_run_bytes(run.record))
  atomic_write_text(temp_dir / "metadata.json", meta_bytes)
  try:
    os.replace(temp_dir, final_dir)
  except OSError:
    if not final_dir.exists():
      raise
  finally:
    if temp_dir.exists():
      shutil.rmtree(temp_dir, ignore_errors=True)


def _write_prompt_bodies(cfg: CharlieBotConfig, bodies: list[PromptBodyWrite]) -> None:
  bodies_dir = cfg.charliebot_home / PROMPT_BODIES_DIR_NAME
  bodies_dir.mkdir(parents=True, exist_ok=True)
  for body in bodies:
    path = bodies_dir / f"{body.ref}.md"
    if path.exists():
      if path.read_text(encoding="utf-8") != body.text:
        raise MigrationRefused(
            f"prompt body store corrupted at {path}: fingerprint content mismatch")
      continue
    atomic_write_text(path, body.text)


def _replace_metadata(cfg: CharlieBotConfig, session_id: str, meta: SessionMetadata) -> str:
  path = cfg.sessions_dir / session_id / "metadata.json"
  atomic_write_text(path, meta.model_dump_json(indent=2, exclude=_TRANSIENT_METADATA_FIELDS))
  return _sha256_file(path)


def _write_run_record(cfg: CharlieBotConfig, session_id: str, record: RunRecord) -> str:
  run_dir = cfg.sessions_dir / session_id / DATA_DIR_NAME / "runs" / record.id
  run_dir.mkdir(parents=True, exist_ok=True)
  path = run_dir / "metadata.json"
  atomic_write_text(path, _expected_run_bytes(record))
  return _sha256_file(path)


def apply_manifest(cfg: CharlieBotConfig, manifest_path: Path, *, manifest: MigrationManifest | None = None) -> dict:
  """Apply one reviewed manifest to the selected home (the CLI's --apply)."""
  if manifest is None:
    try:
      manifest = MigrationManifest.model_validate_json(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
      raise MigrationRefused(f"manifest unreadable at {manifest_path}: {e}") from e
  _validate_manifest_shape(manifest)
  _check_converter_code(manifest)
  if manifest.rolled_back_at is not None:
    raise MigrationRefused(
        "this manifest was rolled back; it is a historical record — rebuild a fresh "
        "manifest with --dry-run before applying again")

  snap = scan_source(cfg)
  state_dir = cfg.charliebot_home / "state" / MIGRATION_STATE_DIR_NAME / manifest.source_sha[:16]
  receipts_path = state_dir / RECEIPTS_FILE_NAME
  receipts = _read_receipts(receipts_path)
  resuming = bool(receipts)

  # The plan is re-derived from the CURRENT home: a partially-applied home
  # re-derives the same products (this converter's deterministic outputs are
  # recognized), so resume and idempotent re-apply never rebuild a different plan.
  plan = build_conversion_plan(cfg, snap)
  _plan_matches_manifest(plan, manifest)
  _check_drift(cfg, manifest, receipts, _expected_product_hashes(cfg, plan),
               _planned_fact_ids(cfg, plan))

  ctx = _ApplyContext(
      cfg=cfg, plan=plan, manifest=manifest, state_dir=state_dir,
      backup_dir=state_dir / "backup", receipts_path=receipts_path, receipts=receipts)
  _verify_receipts_intact(ctx)
  if not resuming:
    receipts_path.parent.mkdir(parents=True, exist_ok=True)
    atomic_write_text(receipts_path, "")

  blockers = quiescence_blockers(snap)
  if blockers:
    raise MigrationRefused(
        "quiescence not proven; the source may have live writers (resolve these and retry): "
        + "; ".join(str(b.get("detail") or b) for b in blockers[:10]),
        details=[orjson.dumps(b).decode() for b in blockers])

  try:
    fence = acquire_home_writer_fence(cfg.charliebot_home, purpose="session-tree migrate --apply")
  except HomeWriterActiveError as e:
    raise MigrationRefused(str(e)) from e
  try:
    return asyncio.run(_apply_locked(cfg, manifest_path, ctx))
  finally:
    fence.release()


def _iter_products(ctx: _ApplyContext):
  """Every migration product in its write order.

  Fact ordering is the durability contract: a node's run_finished facts land
  before its task_closed, a child's close lands before the owner's
  child_report, and every node's task_imported boundary lands last, after all
  its history/conversion facts are durable.
  """
  for body in ctx.plan.prompt_bodies:
    yield ("created", (ctx.cfg.charliebot_home / PROMPT_BODIES_DIR_NAME / f"{body.ref}.md")
           .relative_to(ctx.cfg.charliebot_home).as_posix())
  for product in ctx.plan.workers:
    yield ("worker_node", product)
  for manager in ctx.plan.managers:
    yield ("manager_metadata", manager)
    for run in manager.manager_turn_runs:
      yield ("manager_run", (manager, run))
  for manager in ctx.plan.managers:
    for run in manager.manager_turn_runs:
      if run.outcome is not None:
        yield ("run_finished", (manager.session_id, run))
  for product in ctx.plan.workers:
    for run in product.runs:
      if run.outcome is not None:
        yield ("run_finished", (product.target_id, run))
  for product in ctx.plan.workers:
    if product.task_closed is not None:
      yield ("task_closed", product)
  for product in ctx.plan.workers:
    if product.child_report is not None:
      yield ("child_report", product)
  for manager in ctx.plan.managers:
    yield ("task_imported", manager)
  for product in ctx.plan.workers:
    yield ("task_imported_worker", product)
  yield ("aliases", ctx.plan)
  for rewrite in ctx.plan.cron_rewrites:
    yield ("cron", rewrite)
  for move in ctx.plan.trigger_moves:
    yield ("trigger", move)


def _backup_targets(ctx: _ApplyContext) -> list[tuple[str, str | None]]:
  """(home-relative path, pre-hash) for every file apply replaces, appends or removes."""
  cfg = ctx.cfg
  targets: list[tuple[str, str | None]] = []
  for manager in ctx.plan.managers:
    targets.append((f"sessions/{manager.session_id}/metadata.json",
                    _hash_rel(cfg, f"sessions/{manager.session_id}/metadata.json")))
    targets.append((f"sessions/{manager.session_id}/data/chat_events.jsonl",
                    _hash_rel(cfg, f"sessions/{manager.session_id}/data/chat_events.jsonl")))
    for run in manager.manager_turn_runs:
      targets.append((f"sessions/{manager.session_id}/data/runs/{run.record.id}/metadata.json",
                      _hash_rel(cfg, f"sessions/{manager.session_id}/data/runs/{run.record.id}/metadata.json")))
  for rewrite in ctx.plan.cron_rewrites:
    targets.append((rewrite.rel_path, _hash_rel(cfg, rewrite.rel_path)))
  existing_aliases = _hash_rel(cfg, f"sessions/{ALIASES_FILE_NAME}")
  if ctx.plan.alias_old_sessions or ctx.plan.alias_old_threads or existing_aliases is not None:
    targets.append((f"sessions/{ALIASES_FILE_NAME}", existing_aliases))
  for move in ctx.plan.trigger_moves:
    targets.append((move.old_rel_path, _hash_rel(cfg, move.old_rel_path)))
  return targets


async def _apply_locked(cfg: CharlieBotConfig, manifest_path: Path, ctx: _ApplyContext) -> dict:
  """The mutation phase; the writer fence is held by the caller.

  The task-tree owner (TaskTreeManager over the ordinary SessionManager) is
  the read/verify path; RunStore is the run-record and terminal-fact owner
  whose idempotent finish writes land every migrated run's outcome.
  """
  session_mgr = SessionManager(cfg)
  ctx.tree = TaskTreeManager(cfg, session_mgr)
  manifest = ctx.manifest
  # Mutation-boundary drift recheck: the source is re-read and re-planned now,
  # under the fence. The same acceptance rule as preflight applies — a file
  # must match its manifest hash, a receipt, or this plan's own product — so
  # the manifest's own applied products are not "drift" but anything else is.
  late = scan_live_home_processes(cfg.charliebot_home)
  if late:
    raise MigrationRefused(
        "quiescence lost at the mutation boundary: live process(es) bound to this home "
        "appeared after the preflight check: "
        + "; ".join(f"pid {h['pid']} ({h['cmdline'] or 'unknown'})" for h in late[:8]),
        details=[orjson.dumps(h).decode() for h in late])
  fresh_snap = scan_source(cfg)
  fresh_plan = build_conversion_plan(cfg, fresh_snap)
  _plan_matches_manifest(fresh_plan, manifest)
  _check_drift(cfg, manifest, ctx.receipts, _expected_product_hashes(cfg, fresh_plan),
               _planned_fact_ids(cfg, fresh_plan))

  # Complete, verified rollback backup before the first replacement.
  backup_manifest: dict[str, str] = {}
  for rel, pre_hash in _backup_targets(ctx):
    receipt = ctx.receipts.get(rel)
    if receipt is not None and receipt.kind in ("replaced", "appended", "removed", "moved_from"):
      continue  # already backed up by a previous (resumed) run
    _backup_file(ctx, rel, pre_hash)
    backup_manifest[rel] = pre_hash or ""
  if backup_manifest:
    index_path = ctx.backup_dir / "backup_index.json"
    existing_index: dict = {}
    if index_path.is_file():
      try:
        existing_index = orjson.loads(index_path.read_bytes())
      except ValueError as e:
        raise MigrationRefused(f"backup index unreadable at {index_path}: {e}") from e
    existing_index.update(backup_manifest)
    atomic_write_text(index_path, orjson.dumps(existing_index, option=orjson.OPT_INDENT_2).decode())

  products_done = 0
  for kind, payload in _iter_products(ctx):
    products_done += await _apply_product(ctx, kind, payload)
  return await _finish_apply(cfg, manifest_path, ctx, products_done)


async def _apply_product(ctx: _ApplyContext, kind: str, payload: object) -> int:
  """Write one product (idempotently) and record its receipt. Returns 1 when written."""
  cfg = ctx.cfg
  if kind == "created":
    rel = str(payload)
    path = _confined(cfg, rel)
    receipt = ctx.receipts.get(rel)
    if receipt is not None and receipt.post_sha256 == _hash_rel(cfg, rel):
      return 0
    body = next((b for b in ctx.plan.prompt_bodies
                 if (cfg.charliebot_home / PROMPT_BODIES_DIR_NAME / f"{b.ref}.md")
                 .relative_to(cfg.charliebot_home).as_posix() == rel), None)
    if body is None:
      raise MigrationRefused(f"created product {rel} has no planned body")
    if path.exists():
      if path.read_text(encoding="utf-8") != body.text:
        raise MigrationRefused(
            f"prompt body store corrupted at {path}: fingerprint content mismatch")
      if receipt is None:
        _append_receipt(ctx, ProductReceipt(path=rel, kind="created",
                                            post_sha256=_sha256_file(path)))
      return 0
    path.parent.mkdir(parents=True, exist_ok=True)
    atomic_write_text(path, body.text)
    _append_receipt(ctx, ProductReceipt(
        path=rel, kind="created", post_sha256=_sha256_file(path)))
    return 1

  if kind == "worker_node":
    product = payload  # type: ignore[assignment]
    assert isinstance(product, WorkerNodeProduct)
    node_rel = f"sessions/{product.target_id}"
    if ctx.receipts.get(f"{node_rel}/metadata.json") is not None:
      # Published by a previous (interrupted) run: top up any run-record
      # receipts that crash never wrote, so rollback still owns the whole node.
      for run in product.runs:
        run_rel = f"{node_rel}/data/runs/{run.record.id}/metadata.json"
        receipt = ctx.receipts.get(run_rel)
        if receipt is None and _hash_rel(cfg, run_rel) is not None:
          _append_receipt(ctx, ProductReceipt(
              path=run_rel, kind="created", post_sha256=_hash_rel(cfg, run_rel)))
      return 0
    _publish_worker_node(cfg, product)
    post = _hash_rel(cfg, f"{node_rel}/metadata.json")
    assert post is not None
    if ctx.receipts.get(f"{node_rel}/metadata.json") is None:
      _append_receipt(ctx, ProductReceipt(
          path=f"{node_rel}/metadata.json", kind="created", post_sha256=post))
    for run in product.runs:
      run_rel = f"{node_rel}/data/runs/{run.record.id}/metadata.json"
      if ctx.receipts.get(run_rel) is None:
        _append_receipt(ctx, ProductReceipt(
            path=run_rel, kind="created", post_sha256=_hash_rel(cfg, run_rel)))
    return 1

  if kind == "manager_metadata":
    manager = payload  # type: ignore[assignment]
    assert isinstance(manager, ManagerConversion)
    rel = f"sessions/{manager.session_id}/metadata.json"
    pre = _hash_rel(cfg, rel)
    expected = manager.metadata.model_dump_json(indent=2, exclude=_TRANSIENT_METADATA_FIELDS)
    if pre is not None and pre == _sha256_bytes(expected.encode("utf-8")):
      if ctx.receipts.get(rel) is None:
        _append_receipt(ctx, ProductReceipt(
            path=rel, kind="replaced",
            pre_sha256=_manifest_pre_hash(ctx, rel), post_sha256=pre,
            backup=rel))
      return 0
    _confined(cfg, rel)
    post = _replace_metadata(cfg, manager.session_id, manager.metadata)
    _append_receipt(ctx, ProductReceipt(
        path=rel, kind="replaced", pre_sha256=_manifest_pre_hash(ctx, rel),
        post_sha256=post, backup=rel))
    return 1

  if kind == "manager_run":
    manager, run_product = payload  # type: ignore[misc]
    assert isinstance(manager, ManagerConversion) and isinstance(run_product, RunProduct)
    rel = f"sessions/{manager.session_id}/data/runs/{run_product.record.id}/metadata.json"
    pre = _hash_rel(cfg, rel)
    expected = _expected_run_bytes(run_product.record)
    if pre is not None and pre == _sha256_bytes(expected.encode("utf-8")):
      if ctx.receipts.get(rel) is None:
        _append_receipt(ctx, ProductReceipt(
            path=rel, kind="created", post_sha256=pre))
      return 0
    if pre is not None:
      raise MigrationRefused(
          f"target collision: {rel} exists with different content; refusing to overwrite")
    post = _write_run_record(cfg, manager.session_id, run_product.record)
    _append_receipt(ctx, ProductReceipt(path=rel, kind="created", post_sha256=post))
    return 1

  if kind == "run_finished":
    session_id, run_product = payload  # type: ignore[misc]
    assert isinstance(session_id, str) and isinstance(run_product, RunProduct)
    assert ctx.tree is not None
    log_rel = f"sessions/{session_id}/data/chat_events.jsonl"
    events = ctx.tree.runs.load_events_sync(session_id)
    existing_outcome = ctx.tree.runs.terminal_outcome(events, run_product.record.id)
    if existing_outcome is None:
      # The RunStore owner is idempotent by its own first-terminal-fact-wins
      # contract; this branch only counts genuinely new terminal facts.
      await ctx.tree.runs.record_finish(
          session_id, run_product.record.id, str(run_product.outcome),
          exit_code=run_product.exit_code,
          ended_at=run_product.ended_at)
    post = _hash_rel(ctx.cfg, log_rel)
    existing = ctx.receipts.get(log_rel)
    if post is not None and (existing is None or existing.post_sha256 != post):
      _append_receipt(ctx, ProductReceipt(
          path=log_rel, kind="appended", pre_sha256=_manifest_pre_hash(ctx, log_rel),
          post_sha256=post, backup=log_rel))
    return 1 if existing_outcome is None else 0

  if kind == "task_closed":
    product = payload  # type: ignore[assignment]
    assert isinstance(product, WorkerNodeProduct)
    assert product.task_closed is not None
    post, wrote = _append_fact_if_absent(
        ctx.cfg, product.target_id, product.task_closed)
    log_rel = f"sessions/{product.target_id}/data/chat_events.jsonl"
    existing = ctx.receipts.get(log_rel)
    if existing is None or existing.post_sha256 != post:
      _append_receipt(ctx, ProductReceipt(
          path=log_rel, kind="appended", pre_sha256=_manifest_pre_hash(ctx, log_rel),
          post_sha256=post, backup=log_rel))
    return 1 if wrote else 0

  if kind == "child_report":
    product = payload  # type: ignore[assignment]
    assert isinstance(product, WorkerNodeProduct)
    assert product.child_report is not None
    post, wrote = _append_fact_if_absent(
        ctx.cfg, product.owner_id, product.child_report)
    log_rel = f"sessions/{product.owner_id}/data/chat_events.jsonl"
    existing = ctx.receipts.get(log_rel)
    if existing is None or existing.post_sha256 != post:
      _append_receipt(ctx, ProductReceipt(
          path=log_rel, kind="appended", pre_sha256=_manifest_pre_hash(ctx, log_rel),
          post_sha256=post, backup=log_rel))
    return 1 if wrote else 0

  if kind in ("task_imported", "task_imported_worker"):
    is_worker = kind == "task_imported_worker"
    if is_worker:
      product = payload  # type: ignore[assignment]
      assert isinstance(product, WorkerNodeProduct)
      session_id, event = product.target_id, product.task_imported
    else:
      manager = payload  # type: ignore[assignment]
      assert isinstance(manager, ManagerConversion)
      session_id, event = manager.session_id, manager.task_imported
    log_rel = f"sessions/{session_id}/data/chat_events.jsonl"
    post, wrote = _append_fact_if_absent(ctx.cfg, session_id, event)
    if is_worker:
      # A published node's log: receipt it as a created product at its final
      # fact state (rollback deletes it whole, only when unchanged).
      _append_receipt(ctx, ProductReceipt(path=log_rel, kind="created", post_sha256=post))
    else:
      _append_receipt(ctx, ProductReceipt(
          path=log_rel, kind="appended", pre_sha256=_manifest_pre_hash(ctx, log_rel),
          post_sha256=post, backup=log_rel))
    return 1 if wrote else 0

  if kind == "aliases":
    plan = payload  # type: ignore[assignment]
    assert isinstance(plan, ConversionPlan)
    if not plan.alias_old_sessions and not plan.alias_old_threads:
      return 0
    rel = f"sessions/{ALIASES_FILE_NAME}"
    store = SessionAliasStore(cfg.sessions_dir)
    pre = _hash_rel(cfg, rel)
    store.merge_imported_entries(plan.alias_old_sessions, plan.alias_old_threads)
    post = _sha256_file(store.path)
    if pre == post:
      return 0
    _append_receipt(ctx, ProductReceipt(
        path=rel, kind="replaced", pre_sha256=_manifest_pre_hash(ctx, rel),
        post_sha256=post, backup=rel))
    return 1

  if kind == "cron":
    rewrite = payload  # type: ignore[assignment]
    assert isinstance(rewrite, CronRewrite)
    rel = rewrite.rel_path
    pre = _hash_rel(cfg, rel)
    if pre == _sha256_bytes(rewrite.new_text.encode("utf-8")):
      if ctx.receipts.get(rel) is None:
        _append_receipt(ctx, ProductReceipt(
            path=rel, kind="replaced", pre_sha256=_manifest_pre_hash(ctx, rel),
            post_sha256=pre, backup=rel))
      return 0
    _confined(cfg, rel)
    atomic_write_text(_confined(cfg, rel), rewrite.new_text)
    post = _sha256_file(_confined(cfg, rel))
    _append_receipt(ctx, ProductReceipt(
        path=rel, kind="replaced", pre_sha256=_manifest_pre_hash(ctx, rel),
        post_sha256=post, backup=rel))
    return 1

  if kind == "trigger":
    move = payload  # type: ignore[assignment]
    assert isinstance(move, TriggerMove)
    new_path = _confined(cfg, move.new_rel_path)
    receipt_new = ctx.receipts.get(move.new_rel_path)
    expected_new = move.trigger.model_dump_json(indent=2)
    if receipt_new is None:
      if new_path.exists():
        if new_path.read_text(encoding="utf-8") != expected_new:
          raise MigrationRefused(
              f"target collision: {move.new_rel_path} exists with different content; "
              "refusing to overwrite")
        _append_receipt(ctx, ProductReceipt(
            path=move.new_rel_path, kind="created", post_sha256=_sha256_file(new_path)))
      else:
        new_path.parent.mkdir(parents=True, exist_ok=True)
        atomic_write_text(new_path, expected_new)
        _append_receipt(ctx, ProductReceipt(
            path=move.new_rel_path, kind="created",
            post_sha256=_sha256_file(new_path)))
    old_path = _confined(cfg, move.old_rel_path)
    if old_path.exists():
      os.unlink(old_path)
      _append_receipt(ctx, ProductReceipt(
          path=move.old_rel_path, kind="removed",
          pre_sha256=_manifest_pre_hash(ctx, move.old_rel_path), post_sha256=None,
          backup=move.old_rel_path))
    return 1

  raise MigrationRefused(f"unknown product kind {kind!r}")


def _manifest_pre_hash(ctx: _ApplyContext, rel: str) -> str | None:
  for record in ctx.manifest.source_files:
    if record.path == rel:
      return record.sha256
  return None


# ---------------------------------------------------------------------------
# Post-apply verification (through the ordinary readers)
# ---------------------------------------------------------------------------


async def verify_applied(cfg: CharlieBotConfig, manifest: MigrationManifest,
                         plan: ConversionPlan) -> list[str]:
  """Verify the applied home through the ordinary post-import readers.

  Uses TaskTreeManager's fold, RunStore, and the alias store — the same
  surfaces a running server reads — never a migration-private re-derivation.
  Returns a list of problems; empty means verified.
  """
  problems: list[str] = []
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  await tree._get_index(force=True)

  expected_pending: dict[str, set[str]] = {}
  expected_runs: dict[str, dict[str, str]] = {}
  for manager in plan.managers:
    expected_pending[manager.session_id] = {e["input_id"] for e in manager.pending_inputs}
    for run in manager.manager_turn_runs:
      if run.outcome is not None:
        expected_runs.setdefault(manager.session_id, {})[run.record.id] = str(run.outcome)
  for product in plan.workers:
    expected_pending[product.target_id] = set()
    for run in product.runs:
      if run.outcome is not None:
        expected_runs.setdefault(product.target_id, {})[run.record.id] = str(run.outcome)

  for session_id in sorted(expected_pending):
    meta = await tree.load_meta(session_id)
    if meta is None:
      problems.append(f"{session_id}: migrated node missing after apply")
      continue
    if meta.profile is None:
      problems.append(f"{session_id}: node is not a v2 task after apply")
      continue
    facts = tree.facts_of(session_id)
    if facts.boundary_index is None:
      problems.append(f"{session_id}: no task_imported boundary found in the folded history")
    imported = [e for e in facts.events_by_id.values() if e.get("type") == ET.TASK_IMPORTED]
    if not imported:
      problems.append(f"{session_id}: task_imported fact missing")
    pending = {str(e.get("id")) for e in tree.dispatch.pending_inputs(session_id)}
    if pending != expected_pending[session_id]:
      problems.append(
          f"{session_id}: pending inputs {sorted(pending)} != expected "
          f"{sorted(expected_pending[session_id])}")
    runs = {r.id: r for r in tree.runs.list_run_records_sync(session_id)}
    for run_id, outcome in expected_runs.get(session_id, {}).items():
      record = runs.get(run_id)
      if record is None:
        problems.append(f"{session_id}: run {run_id} missing after apply")
        continue
      actual = tree.runs.terminal_outcome(tree.runs.load_events_sync(session_id), run_id)
      if actual != outcome:
        problems.append(f"{session_id}: run {run_id} outcome {actual!r} != expected {outcome!r}")
      if record.raw_log_ref and not Path(record.raw_log_ref).exists():
        problems.append(f"{session_id}: run {run_id} raw_log_ref does not resolve: {record.raw_log_ref}")
      if record.events_ref and not Path(record.events_ref).exists():
        problems.append(f"{session_id}: run {run_id} events_ref does not resolve: {record.events_ref}")

  # Completed children: the close fact and the delivered parent receipt.
  for product in plan.workers:
    if product.task_closed is None:
      continue
    facts = tree.facts_of(product.target_id)
    if facts.task_state != "completed":
      problems.append(f"{product.target_id}: imported-completed task is not completed after apply")
    parent_facts = tree.facts_of(product.owner_id)
    close_id = product.task_closed["id"]
    if (product.target_id, close_id) not in parent_facts.delivered_reports:
      problems.append(
          f"{product.target_id}: completed import receipt not delivered to {product.owner_id}")

  # Aliases resolve through the ordinary read surface.
  aliases = SessionAliasStore(cfg.sessions_dir)
  for key, target in plan.alias_old_threads.items():
    owner, _, thread_id = key.rpartition("/")
    resolved = aliases.resolve_thread(owner, thread_id)
    if resolved != target:
      problems.append(f"alias {key} resolves to {resolved}, expected {target}")
  for old, canonical in plan.alias_old_sessions.items():
    resolved = aliases.resolve_session(old)
    if resolved != canonical:
      problems.append(f"session alias {old} resolves to {resolved}, expected {canonical}")

  # Cron bindings load (the ordinary loader, per file).
  for rewrite in plan.cron_rewrites:
    error = _validate_rewritten_cron(
        cfg, cfg.charliebot_home / rewrite.rel_path,
        (cfg.charliebot_home / rewrite.rel_path).read_text(encoding="utf-8"),
        task_name_of(rewrite.rel_path))
    if error:
      problems.append(f"{rewrite.rel_path}: rewritten config does not load: {error}")

  # Archive presentation matches the plan.
  index = await tree._get_index()
  for manager in plan.managers:
    meta = index.metas.get(manager.session_id)
    if meta is None:
      continue
    archived = tree.archived_of(index, meta)
    was_archived = manager.metadata.status.value == "archived"
    if was_archived and not archived:
      problems.append(f"{manager.session_id}: archived source lost its collapsed presentation")
  return problems


async def _finish_apply(cfg: CharlieBotConfig, manifest_path: Path, ctx: _ApplyContext,
                        products_done: int) -> dict:
  """Record the rollback refs and receipts, verify, and update the manifest."""
  manifest = ctx.manifest
  manifest.rollback_refs = [str(ctx.backup_dir)]
  manifest.receipts = list(ctx.receipts.values())
  manifest.applied_at = datetime.now(UTC)
  problems = await verify_applied(cfg, manifest, ctx.plan)
  atomic_write_text(manifest_path, manifest.model_dump_json(indent=2))
  if problems:
    raise MigrationRefused(
        "apply completed but post-apply verification failed; the home holds the migration's "
        "products and its backups (do NOT rerun apply; inspect and roll back): "
        + "; ".join(problems[:10]),
        details=problems)
  return {
      "status": "already_applied" if products_done == 0 else "applied",
      "products_written": products_done,
      "receipts": len(ctx.receipts),
      "unresolved": 0,
      "verification": "ok",
      "rollback_refs": manifest.rollback_refs,
      "manifest": str(manifest_path),
  }


# ---------------------------------------------------------------------------
# Rollback
# ---------------------------------------------------------------------------


def _prune_empty_dirs_bottom_up(root: Path, *, stop_at: Path) -> None:
  """Remove *root* and its now-empty descendants (migration-owned skeleton)."""
  if not root.is_dir() or root == stop_at or stop_at not in root.parents:
    return
  for child in sorted(root.rglob("*"), reverse=True):
    if child.is_dir():
      try:
        child.rmdir()
      except OSError:
        pass
  try:
    root.rmdir()
  except OSError:
    pass


def rollback_manifest(cfg: CharlieBotConfig, manifest_path: Path,
                      *, manifest: MigrationManifest | None = None) -> dict:
  """Restore originals and remove migration-owned unchanged products.

  Eligible only while every migration product still matches its receipt (no
  new-system write has been admitted): once any product changed, rollback
  refuses instead of erasing new data.
  """
  if manifest is None:
    try:
      manifest = MigrationManifest.model_validate_json(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
      raise MigrationRefused(f"manifest unreadable at {manifest_path}: {e}") from e
  _validate_manifest_shape(manifest)
  if manifest.applied_at is None:
    raise MigrationRefused("this manifest was never applied; there is nothing to roll back")
  if manifest.rolled_back_at is not None:
    raise MigrationRefused("this manifest was already rolled back")
  if not manifest.receipts:
    raise MigrationRefused("manifest carries no receipts; rollback cannot prove product state")

  receipts_path = (cfg.charliebot_home / "state" / MIGRATION_STATE_DIR_NAME /
                   manifest.source_sha[:16] / RECEIPTS_FILE_NAME)
  durable = _read_receipts(receipts_path)
  receipts = durable or {r.path: r for r in manifest.receipts}
  backup_dir = cfg.charliebot_home / "state" / MIGRATION_STATE_DIR_NAME / manifest.source_sha[:16] / "backup"

  # Refusal pass first: no restore happens unless every product is unchanged.
  broken: list[str] = []
  for receipt in receipts.values():
    current = _hash_rel(cfg, receipt.path)
    if receipt.kind == "removed":
      if current is not None:
        broken.append(f"{receipt.path}: removed product reappeared ({current[:12]})")
      continue
    if current != receipt.post_sha256:
      broken.append(
          f"{receipt.path}: receipt {None if receipt.post_sha256 is None else receipt.post_sha256[:12]}, "
          f"current {None if current is None else current[:12]} (new-system write or edit; "
          "direct rollback would erase it)")
  if broken:
    raise MigrationRefused(
        "rollback refused: migration products no longer match their receipts: "
        + "; ".join(broken[:10]),
        details=broken)

  # Backup verification pass BEFORE the first restore: a missing or corrupted
  # backup aborts with every product and original still in place, never a
  # partially restored home.
  backup_problems: list[str] = []
  for receipt in sorted(receipts.values(), key=lambda r: r.path):
    if receipt.kind == "created" or receipt.pre_sha256 is None:
      continue
    if receipt.backup is None:
      backup_problems.append(f"{receipt.path}: backup reference missing")
      continue
    backup_file = backup_dir / receipt.backup
    if not backup_file.is_file():
      backup_problems.append(f"{receipt.path}: backup file missing ({backup_file})")
      continue
    backed = _sha256_file(backup_file)
    if backed != receipt.pre_sha256:
      backup_problems.append(
          f"{receipt.path}: backup hash {backed[:12]} != pre-apply hash {receipt.pre_sha256[:12]}")
  if backup_problems:
    raise MigrationRefused(
        "rollback refused: backup verification failed; nothing was restored, evidence is "
        "intact: " + "; ".join(backup_problems[:10]),
        details=backup_problems)

  try:
    fence = acquire_home_writer_fence(cfg.charliebot_home, purpose="session-tree migrate --rollback")
  except HomeWriterActiveError as e:
    raise MigrationRefused(str(e)) from e
  try:
    restored, removed = 0, 0
    # Restore replaced/appended/removed products from their verified backups.
    for receipt in sorted(receipts.values(), key=lambda r: r.path):
      if receipt.kind in ("created",):
        continue
      if receipt.pre_sha256 is None:
        # The file did not exist before apply (its whole content is this
        # apply's append, verified unchanged above): remove it.
        path = _confined(cfg, receipt.path)
        if path.exists():
          path.unlink()
          restored += 1
        continue
      backup_rel = receipt.backup
      if backup_rel is None:
        raise MigrationRefused(
            f"backup reference missing for {receipt.path}; rollback cannot restore it")
      backup_file = backup_dir / backup_rel
      if not backup_file.is_file():
        raise MigrationRefused(f"backup file missing: {backup_file}")
      backed = _sha256_file(backup_file)
      if receipt.pre_sha256 is not None and backed != receipt.pre_sha256:
        raise MigrationRefused(
            f"backup hash mismatch for {receipt.path}: backup {backed[:12]}, "
            f"expected {receipt.pre_sha256[:12]}")
      dest = _confined(cfg, receipt.path)
      dest.parent.mkdir(parents=True, exist_ok=True)
      shutil.copyfile(backup_file, dest)
      restored += 1
    # Remove created products only when unchanged, then prune the migration's
    # own now-empty directories (a published node's skeleton dirs included).
    node_roots: set[Path] = set()
    for receipt in sorted(receipts.values(), key=lambda r: r.path):
      if receipt.kind != "created":
        continue
      path = _confined(cfg, receipt.path)
      if not path.exists():
        continue
      current = _sha256_file(path)
      if current != receipt.post_sha256:
        raise MigrationRefused(
            f"created product {receipt.path} changed after apply ({current[:12]}); "
            "refusing to delete it")
      path.unlink()
      removed += 1
      node_roots.add(cfg.sessions_dir / receipt.path.split("/")[1])
    for node_root in node_roots:
      _prune_empty_dirs_bottom_up(node_root, stop_at=cfg.sessions_dir)
    manifest.rolled_back_at = datetime.now(UTC)
    atomic_write_text(manifest_path, manifest.model_dump_json(indent=2))
    # The journal described an apply that no longer exists; the backups stay
    # for forensics. A later apply of the same source starts fresh.
    if receipts_path.exists():
      receipts_path.unlink()
    return {"status": "rolled_back", "restored": restored, "removed": removed,
            "manifest": str(manifest_path)}
  finally:
    fence.release()

"""The cron.d loader: each ``config.d/cron.d/<name>.yaml`` file loads as a task model or an error record."""

import json
import os
import re
from pathlib import Path
from zoneinfo import ZoneInfo

from src.features.cron.config import ScheduledTaskConfig, ScheduledTaskError
from src.infra.config import get_config
from src.infra.home import charliebot_home_dir
from src.infra.json_utils import write_json_atomically
from src.infra.log_once import LazyStructlogLogger
from src.infra.notifications import send_telegram
from src.infra.tasks import create_logged_task
from src.infra.yaml_utils import load_yaml

log = LazyStructlogLogger()


class _CronSnapshot:
  """Module-level cache of the last cron.d load, invalidated by any fingerprint change."""

  __slots__ = ('errors', 'fingerprint', 'prompt_mtimes', 'tasks')

  def __init__(self) -> None:
    self.tasks: list[ScheduledTaskConfig] = []
    self.errors: list[ScheduledTaskError] = []
    self.prompt_mtimes: dict[Path, float] = {}
    self.fingerprint: object = None


_cron_snapshot = _CronSnapshot()


def _resolve_pointer_path(pointer: str, repo: Path) -> Path:
  """Resolve one raw ``prompt_file`` pointer: ``~``-prefixed or absolute literal, else against *repo*."""
  if pointer.startswith("~") or Path(pointer).is_absolute():
    return Path(os.path.expanduser(pointer))
  return repo / pointer


def _resolve_prompt_file(entry: dict, repo_root: Path) -> Path | None:
  """Resolve a cron entry's ``prompt_file`` into ``prompt`` in place.

  Reads the referenced file, sets ``entry['prompt']`` to its contents, and
  removes the ``prompt_file`` key while resolving. Callers that expose the
  runtime model restore the raw pointer after this step. Returns the resolved
  :class:`Path` (for mtime tracking) or ``None`` if the entry had no
  ``prompt_file``.

  Path resolution has no search order and no shadowing:
  :func:`_resolve_pointer_path` is the rule.

  Raises :class:`ValueError` if the entry carries both a non-empty ``prompt``
  and a ``prompt_file`` (two prompt sources is a configuration error), or if the
  file is missing or unreadable.
  """
  pf = entry.get("prompt_file")
  if not pf:
    return None
  if entry.get("prompt"):
    raise ValueError(
        f"cron entry {entry.get('name')!r} has both 'prompt' and 'prompt_file'; "
        "exactly one prompt source is allowed")
  path = _resolve_pointer_path(pf, repo_root)
  try:
    body = path.read_text(encoding="utf-8")
  except OSError as e:
    raise ValueError(f"cron entry {entry.get('name')!r} prompt_file unreadable: {path} ({e})") from e
  entry["prompt"] = body
  del entry["prompt_file"]
  return path


def _detect_local_timezone() -> str:
  """Return the host's IANA timezone, derived from ``/etc/localtime``.

  Resolves the ``/etc/localtime`` symlink to its real path, takes the part after
  ``zoneinfo/``, and validates it with :class:`ZoneInfo`. On any failure (no
  symlink, no ``zoneinfo/`` segment, invalid key) logs one warning and returns
  ``"UTC"`` — this is an environment limitation, not a user configuration error.
  """
  try:
    real = os.path.realpath("/etc/localtime")
    marker = "/zoneinfo/"
    idx = real.rfind(marker)
    if idx < 0:
      raise ValueError(f"no {marker!r} segment in {real!r}")
    tz_name = real[idx + len(marker):]
    if not tz_name:
      raise ValueError(f"empty timezone name in {real!r}")
    ZoneInfo(tz_name)  # validate — raises ZoneInfoNotFoundError on a bad key
    return tz_name
  except Exception as e:
    log.warning("local_timezone_resolve_failed", error=str(e), fallback="UTC")
    return "UTC"


def _resolve_local_timezone(entry: dict) -> None:
  """Rewrite the ``local`` sentinel into the host's IANA zone in place.

  Only entries literally carrying ``timezone: local`` are affected; every other
  value (including the model/API/UI default ``America/Los_Angeles``) is left
  untouched.
  """
  if entry.get("timezone") != "local":
    return
  entry["timezone"] = _detect_local_timezone()


def _stat_prompt_files(paths: dict[Path, float]) -> dict[Path, float] | None:
  """Return current mtimes for *paths*, or ``None`` if any is missing.

  Returning ``None`` forces a reload so a missing ``prompt_file`` surfaces as a
  per-file load error instead of silently serving a cached body from a file that
  no longer exists.
  """
  current: dict[Path, float] = {}
  for p in paths:
    try:
      current[p] = os.stat(str(p)).st_mtime
    except OSError:
      return None
  return current


def cron_dir() -> Path:
  """Path of this profile's per-job cron config directory. Resolved per call."""
  return charliebot_home_dir() / "config.d" / "cron.d"


def cron_path(name: str) -> Path:
  """Path of one job's cron config file. Resolved per call, never at import."""
  return cron_dir() / f"{name}.yaml"


def _valid_cron_name(name: str) -> bool:
  """Return whether *name* is a safe cron job name for a single host file.

  Only names matching ``^[A-Za-z0-9][A-Za-z0-9._-]*$`` are safe: anything else
  (``..``, an embedded ``/``, a leading ``.``, an absolute-looking name) could
  escape the ``cron.d`` directory, so it is rejected with a 400 before any
  filesystem access. The host also uses this guard when enumerating files.
  """
  return bool(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name))


def _prompt_pointer_entries(body: dict) -> list[dict]:
  """The mapping entries a cron prompt-pointer walk covers: the body, then each mapping step."""
  return [body] + [step for step in body.get("steps") or [] if isinstance(step, dict)]


def _record_prompt_mtime(prompt_mtimes: dict[Path, float], path: Path) -> None:
  """Record *path*'s mtime in *prompt_mtimes*, or the 0.0 sentinel when it has vanished.

  The sentinel keeps a vanished pointer file in the hot-reload fingerprint so
  the next tick re-reads it instead of serving a cached body. Only
  :class:`OSError` rides the sentinel: the fingerprint walker
  :func:`_stat_prompt_files` catches no other stat failure, so a recorded path
  that raises, e.g. :class:`ValueError` on an embedded null byte, would escape
  :func:`get_scheduled_tasks` and break its never-raises contract on every
  later tick.
  """
  try:
    prompt_mtimes[path] = path.stat().st_mtime
  except OSError:
    prompt_mtimes[path] = 0.0


def _resolve_prompt_pointer(entry: dict, repo: Path, prompt_mtimes: dict[Path, float]) -> None:
  """Resolve one mapping's ``prompt_file`` into ``prompt`` in place.

  Restores the raw pointer on the mapping afterwards (the pointer is what the
  API and UI display) and records the resolved file's mtime into
  *prompt_mtimes* for the hot-reload fingerprint. Applies to a task body and
  to each ``steps`` entry alike; a mapping without a pointer is a no-op.
  """
  prompt_file = entry.get("prompt_file")
  if not prompt_file:
    return
  resolved = _resolve_prompt_file(entry, repo)
  entry["prompt_file"] = prompt_file  # preserve the raw pointer for the API/UI
  _record_prompt_mtime(prompt_mtimes, resolved)


def _validate_cron_body(body: dict, repo: Path, stem: str) -> tuple[ScheduledTaskConfig, dict[Path, float]]:
  """Resolve and validate one cron job body into a ``ScheduledTaskConfig``.

  Mutates *body* in place — resolves ``prompt_file`` (setting ``prompt`` to the
  referenced file's body and preserving the raw pointer on the model), resolves
  each ``steps`` entry's ``prompt_file`` the same way, rewrites a literal
  ``timezone: local`` to the host IANA zone, and expands ``~`` in
  ``repo`` — matching the :func:`_resolve_prompt_file` mutate-in-place
  convention. The pointer owns the prompt body; the body carries only the path
  to it, and the loader reads that file on every load. Any caller that needs
  the pre-write file format (e.g. the cron API's create/update paths, which
  validate a deep copy) is guaranteed a result the loader can reload unchanged.

  Returns the model plus the mtime map for any ``prompt_file`` it read (for the
  hot-reload fingerprint). Raises :class:`ValueError` (or a pydantic validation
  error) on any failure.
  """
  prompt_mtimes: dict[Path, float] = {}
  for entry in _prompt_pointer_entries(body):
    _resolve_prompt_pointer(entry, repo, prompt_mtimes)
  _resolve_local_timezone(body)
  if body.get("repo"):
    body["repo"] = os.path.expanduser(body["repo"])
  return ScheduledTaskConfig(name=stem, **body), prompt_mtimes


def _load_cron_file(path: Path, repo: Path, stem: str) -> tuple[ScheduledTaskConfig, dict[Path, float]]:
  """Load, resolve, and validate one ``cron.d`` file into a ``ScheduledTaskConfig``.

  The file body is a top-level mapping of :class:`ScheduledTaskConfig` fields
  *without* ``name``; the job name is *stem* (the file stem) and is injected
  here. A body that carries a ``name`` key is an error (the file name is the
  single source of the name). A host file carries the path to its prompt source
  under ``prompt_file``; the pointed file owns the body, and this loader reads
  it on every load. A body that instead carries the body itself under ``prompt``
  holds a second source, so it is a load error. Resolution follows the existing
  order: ``prompt_file`` against *repo* (``~``-prefixed or absolute taken
  literally), ``timezone: local`` to the host IANA zone, and ``repo`` to
  ``expanduser`` — see :func:`_validate_cron_body`.

  Returns the model plus the mtime map for any ``prompt_file`` it read (for the
  hot-reload fingerprint). Raises :class:`ValueError` on any failure; the caller
  records it as a per-file error rather than propagating it.
  """
  body = load_yaml(path, default=None)
  if not isinstance(body, dict):
    raise ValueError("cron config must be a mapping")
  if "name" in body:
    raise ValueError("the body must not carry a 'name' key; the file name is the job name")
  if "prompt" in body:
    raise ValueError(
        "a cron.d host file must not carry an inline 'prompt'; it holds the "
        "path to the prompt source under 'prompt_file', and the loader reads "
        "that file on every load")
  for step in body.get("steps") or []:
    if isinstance(step, dict) and "prompt" in step:
      raise ValueError(
          "a cron.d host file must not carry an inline 'prompt'; a step holds the "
          "path to its prompt source under 'prompt_file', and the loader reads "
          "that file on every load")
  return _validate_cron_body(body, repo, stem)


def _read_cron_file_enabled(path: Path) -> bool | None:
  """Best-effort raw ``enabled`` read of a cron file the loader failed on.

  Returns the file's own ``enabled`` when the body parses as a mapping with a
  boolean value; ``None`` on any read/parse failure — an unparseable file has
  no truthful raw value, and inventing a default would misstate it.
  """
  try:
    body = load_yaml(path, default=None)
  except Exception as e:
    log.debug("cron_file_enabled_unreadable", path=str(path), error=str(e))
    return None
  if isinstance(body, dict) and isinstance(body.get("enabled"), bool):
    return body["enabled"]
  return None


def _reload_cron_snapshot() -> _CronSnapshot:
  """Recompute the snapshot by loading every ``cron.d`` file independently."""
  global _cron_snapshot
  repo = get_config().charlie_bot_repo
  cron_d = cron_dir()

  tasks: list[ScheduledTaskConfig] = []
  errors: list[ScheduledTaskError] = []
  prompt_mtimes: dict[Path, float] = {}

  # A missing cron.d/ directory is an empty set, not an error.
  if cron_d.is_dir():
    for path in sorted(cron_d.iterdir()):
      if not (path.is_file() and path.name.endswith(".yaml") and not path.name.startswith(".")):
        continue
      stem = path.stem
      if not _valid_cron_name(stem):
        errors.append(
            ScheduledTaskError(
                name=stem,
                path=str(path),
                error="file name is not a valid cron task name",
                enabled=_read_cron_file_enabled(path)))
        continue
      try:
        task, file_prompt_mtimes = _load_cron_file(path, repo, stem)
      except Exception as e:
        # Keep a failed pointer in the fingerprint too. If its target is
        # restored without touching the host yaml, the next call must retry
        # the file and clear the error instead of serving a cached failure.
        try:
          failed_body = load_yaml(path, default=None)
        except Exception as read_error:
          log.debug("cron_failed_file_prompt_path_unreadable", path=str(path), error=str(read_error))
        else:
          if isinstance(failed_body, dict):
            for entry in _prompt_pointer_entries(failed_body):
              pointer = entry.get("prompt_file")
              if isinstance(pointer, str) and pointer:
                try:
                  _record_prompt_mtime(prompt_mtimes, _resolve_pointer_path(pointer, repo))
                except ValueError as stat_error:
                  # An unstatable pointer (e.g. an embedded null byte) must stay out of the
                  # fingerprint; see _record_prompt_mtime. Skipping it keeps this loader total.
                  log.debug("cron_failed_prompt_path_unstatable", path=str(pointer), error=str(stat_error))
        errors.append(
            ScheduledTaskError(name=stem, path=str(path), error=str(e), enabled=_read_cron_file_enabled(path)))
        log.error("cron_task_load_failed", name=stem, path=str(path), error=str(e))
        continue
      tasks.append(task)
      prompt_mtimes.update(file_prompt_mtimes)

  tasks.sort(key=lambda t: t.name)
  errors.sort(key=lambda e: e.name)
  _fire_cron_error_alert([e.name for e in errors])
  snapshot = _CronSnapshot()
  snapshot.tasks = tasks
  snapshot.errors = errors
  snapshot.prompt_mtimes = prompt_mtimes
  snapshot.fingerprint = _cron_fingerprint(prompt_mtimes)
  _cron_snapshot = snapshot
  return snapshot


def _cron_alert_state_path() -> Path:
  """Path of the persisted last-alerted cron error fingerprint. Resolved per call."""
  return charliebot_home_dir() / "state" / "cron_alert_fingerprint.json"


def _read_cron_alert_state() -> frozenset[str]:
  """The last-alerted set of broken cron task names persisted on disk.

  A missing file reads as the empty set: on first deployment any currently
  broken task counts as a fresh non-empty transition and alerts once. A
  corrupt or unreadable file also reads as empty (alerting again beats never
  alerting), with a warning.
  """
  try:
    raw = _cron_alert_state_path().read_text(encoding="utf-8")
  except FileNotFoundError:
    return frozenset()
  except OSError as e:
    log.warning("cron_alert_state_unreadable", error=str(e))
    return frozenset()
  try:
    data = json.loads(raw)
  except ValueError as e:
    log.warning("cron_alert_state_unparseable", error=str(e))
    return frozenset()
  if not isinstance(data, list):
    log.warning("cron_alert_state_unparseable", error="state file is not a JSON list")
    return frozenset()
  return frozenset(str(name) for name in data)


def _fire_cron_error_alert(error_names: list[str]) -> None:
  """Alert over Telegram on every transition of the broken-cron-task name set.

  Compares the fresh set against the last-alerted set persisted at
  :func:`_cron_alert_state_path`; on any difference it records the new set and
  fires one notification — ``"⚠️ cron tasks failed to load: <names>"`` when the new set
  is non-empty, ``"✅ all cron load failures resolved"`` when it turned empty (recovery
  fires only on the full transition, not on every shrink); an identical set
  stays silent.

  The send is fire-and-forget through
  :func:`src.infra.tasks.create_logged_task`, so a Telegram failure is a logged
  background-task failure and can never raise back into the config loader or
  the scheduler tick. With no running event loop (a synchronous CLI path) the
  send is skipped and the new set left unpersisted, so the next looped
  evaluation — the scheduler's unconditional 60s tick through
  :func:`get_scheduled_tasks` — transitions again and fires.
  """
  new_set = frozenset(error_names)
  if new_set == _read_cron_alert_state():
    return
  import asyncio

  try:
    asyncio.get_running_loop()
  except RuntimeError:
    log.info("cron_alert_skipped_no_event_loop", names=sorted(new_set))
    return
  names = sorted(new_set)
  message = "⚠️ cron tasks failed to load: " + ", ".join(names) if names else "✅ all cron load failures resolved"
  try:
    create_logged_task(send_telegram(message, get_config()), name="cron-load-alert")
  except Exception:
    log.exception("cron_alert_dispatch_failed", names=names)
  try:
    state_path = _cron_alert_state_path()
    state_path.parent.mkdir(parents=True, exist_ok=True)
    write_json_atomically(state_path, names, newline=True)
  except OSError:
    log.exception("cron_alert_state_write_failed")


def _cron_fingerprint(
    prompt_mtimes: dict[Path, float],) -> tuple[tuple[tuple[str, float], ...], dict[Path, float] | None]:
  """Compute the hot-reload fingerprint over both re-read inputs.

  The set of ``cron.d/*.yaml`` paths with each file's mtime, and the mtime of
  every referenced ``prompt_file`` (a referenced file that has gone missing
  makes the stat fail, returning ``None`` and forcing a full re-read so the
  failure surfaces instead of a stale cached body).

  This walk runs on every ``get_scheduled_tasks`` call (each /scheduled and
  /api/cron/tasks request, every scheduler tick), so it is one ``os.scandir``
  over the raw string dir with ``DirEntry`` answering
  ``is_file`` from the directory record, no per-entry ``Path`` construction —
  the pathlib form measured 174 us vs 53 us on the live 13-file corpus.
  """
  cron_d = str(cron_dir())
  files: list[tuple[str, float]] = []
  if os.path.isdir(cron_d):
    with os.scandir(cron_d) as entries:
      for entry in sorted(entries, key=lambda e: e.name):
        try:
          if not entry.is_file():
            continue
        except OSError:
          continue  # the pathlib is_file contract: unreadable entry reads as absent
        if entry.name.endswith(".yaml") and not entry.name.startswith("."):
          try:
            files.append((entry.name, entry.stat().st_mtime))
          except OSError:
            files.append((entry.name, 0.0))
  current_prompt_mtimes = _stat_prompt_files(prompt_mtimes)
  return (tuple(files), current_prompt_mtimes)


def get_scheduled_tasks() -> list[ScheduledTaskConfig]:
  """Load the valid scheduled tasks from ``config.d/cron.d/<name>.yaml``.

  Total and never raises: any file that fails to parse or validate becomes a
  single entry in :func:`get_scheduled_task_errors` and is skipped, every other
  file still loads and is schedulable. The result is sorted by name.

  The snapshot refreshes whenever the fingerprint changes (cron.d file set and
  mtimes, referenced prompt_file mtimes), so a change takes effect on the next
  call with no restart.
  """
  return _refresh_cron_snapshot().tasks


def scheduled_tasks_snapshot() -> tuple[list[ScheduledTaskConfig], object]:
  """The current snapshot's ``(tasks, fingerprint)``, from one refresh read.

  The fingerprint is the freshness key :func:`get_scheduled_tasks` itself
  answers on: an equal value proves the task list unchanged since the caller
  last read it, so derived fields keyed on it stay current until it moves.
  Both values come from the one snapshot so a consumer cannot stamp one
  generation's fingerprint on another's answer.
  """
  snapshot = _refresh_cron_snapshot()
  return snapshot.tasks, snapshot.fingerprint


def get_scheduled_task_errors() -> list[ScheduledTaskError]:
  """Return one record per failing cron job file, sorted by name.

  Total and never raises, mirroring :func:`get_scheduled_tasks`.
  """
  return _refresh_cron_snapshot().errors


def _refresh_cron_snapshot() -> _CronSnapshot:
  """Return the cached snapshot, reloading when the fingerprint changed."""
  snapshot = _cron_snapshot
  if snapshot.fingerprint == _cron_fingerprint(snapshot.prompt_mtimes):
    return snapshot
  return _reload_cron_snapshot()

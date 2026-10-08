"""Codex's part of the cold-storage sweep (src/features/usage/storage_cool.py): rollout files.

``usage_logs.sweep()`` imports this module on its first call, so a usage capture and
``charliebot --help`` never load it. The sweep's one deletion rule is stated on ``SweepScope``
(src/runtime/hooks/usage_sources.py). A rollout file name embeds the Codex thread id, which is
the backend session id that CharlieBot metadata references.
"""

from pathlib import Path

from src.backends.codex.codex_usage import default_codex_home
from src.infra import log_once
from src.runtime.hooks import usage_sources

log = log_once.LazyStructlogLogger()

_CODEX_ROLLOUT_PREFIX = "rollout-"


def codex_session_trees() -> list[Path]:
  """The rollout tree codex writes into; codex runs from the default home."""
  return [default_codex_home() / "sessions"]


def codex_rollout_session_id(path: Path) -> str | None:
  """The codex thread id embedded in ``rollout-<iso timestamp>-<uuid>.jsonl``.

  The timestamp itself contains hyphens, so the id is the last five
  hyphen-separated components of the stem, not the first five.
  """
  stem = path.stem
  if not stem.startswith(_CODEX_ROLLOUT_PREFIX):
    return None
  candidate = "-".join(stem.split("-")[-5:])
  return candidate if usage_sources.SESSION_ID_RE.fullmatch(candidate) else None


def _sweep_codex_rollouts(scope: usage_sources.SweepScope, counter: usage_sources.SweepCounter) -> None:
  """Delete rollout files under the session-cold / unreferenced-plus-window rule."""
  scoped_backends = scope.scoped_backend_sessions()
  for path in usage_sources.sweep_root_entries(codex_session_trees(),
                                               lambda root: root.rglob(f"{_CODEX_ROLLOUT_PREFIX}*.jsonl")):
    backend_session = codex_rollout_session_id(path)
    if backend_session is None:
      continue
    if scope.session_id is not None:
      # Scoped run: only the named session's own record, already cold-verified,
      # and never a record also referenced by a live session.
      if backend_session in scoped_backends and scope.referenced_and_cold(backend_session):
        counter.delete_file(path, scope.dry_run)
      continue
    if scope.references.get(backend_session) is None:
      # Unreferenced: the file's own mtime is the idle clock.
      try:
        idle_since = path.stat().st_mtime
      except OSError as e:
        log.warning("storage_cool_file_stat_failed", path=str(path), error=str(e))
        continue
      if scope.idle_past(idle_since):
        counter.delete_file(path, scope.dry_run)
      continue
    if scope.referenced_and_cold(backend_session):
      counter.delete_file(path, scope.dry_run)


def sweep(scope: usage_sources.SweepScope) -> usage_sources.SourceSweep:
  """Codex's part of the sweep: one ``codex-rollouts`` category, counted in files."""
  counter = usage_sources.SweepCounter("codex-rollouts", "files")
  _sweep_codex_rollouts(scope, counter)
  return usage_sources.SourceSweep(categories=(counter.result(),), freelist=None)

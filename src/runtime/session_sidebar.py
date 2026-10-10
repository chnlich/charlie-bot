"""Session sidebar state: each row's running, work-state, pending-trigger and plan-approval flags, and their probes.

``SessionSidebar.resolve_sidebar_state`` is the one derivation of those flags. It serves the in-process snapshot
(:mod:`src.runtime.sidebar_state`) and re-probes only the sessions whose probe inputs moved; the listing and search
blocks call it for the rows they return. The task-tree owner registers its activity derivation as
``task_tree_activity``. The process builds one block (``sidebar()``); tests monkeypatch the ``_sidebar``
global.
"""

import asyncio
import os
import stat
from collections.abc import Callable
from datetime import datetime
from pathlib import Path
from typing import Any, NamedTuple

from src.infra.config import CharlieBotConfig, get_config
from src.infra.json_utils import load_json_meta
from src.infra.log_once import LazyStructlogLogger
from src.infra.memo import BoundedMemo, StatSignatureMemo, stat_signature
from src.infra.models import SessionMetadata, SessionStatus, parse_utc_datetime
from src.infra.tasks import create_logged_task
from src.runtime import session_store, sidebar_state, trigger_files
from src.runtime.hooks.sidebar_contributions import sidebar_contributions
from src.runtime.thinking_state import busy_since

log = LazyStructlogLogger()


def _sidebar_entry(
    include_running_status: bool,
    include_pending_trigger_status: bool,
    include_pending_plan_approval: bool,
    *,
    running: bool,
    trigger_count: int,
    next_trigger_at: datetime | None,
    plan_approval: bool,
    task_activity: tuple[bool, str] | None = None,
) -> dict:
  """Build one session's derived sidebar entry: the include-gated key set.

  Both build sites in :meth:`SessionSidebar.resolve_sidebar_state` — the
  archived shortcut and the probed path — must carry the same keys, so the
  set lives here. ``has_pending_trigger`` derives from the count.

  *task_activity* is the task-tree derivation's ``(has_running_tasks,
  work_state)`` pair. It owns the row's activity verdict and supplies its
  work state when the tree owner is available.
  """
  entry: dict = {}
  if include_running_status:
    entry[sidebar_state.HAS_RUNNING_TASKS] = (bool(task_activity[0]) if task_activity is not None else running)
  if include_pending_trigger_status:
    entry[sidebar_state.HAS_PENDING_TRIGGER] = trigger_count > 0
    entry[sidebar_state.PENDING_TRIGGER_COUNT] = trigger_count
    entry[sidebar_state.NEXT_TRIGGER_AT] = next_trigger_at
  if include_pending_plan_approval:
    entry[sidebar_state.HAS_PENDING_PLAN_APPROVAL] = plan_approval
  if task_activity is not None:
    entry[sidebar_state.WORK_STATE] = task_activity[1]
  return entry


def apply_sidebar_state(
    sessions: list[SessionMetadata],
    derived: dict[str, dict],
    include_running_status: bool,
    include_pending_trigger_status: bool,
    include_pending_plan_approval: bool = False,
) -> None:
  """Write :meth:`SessionSidebar.resolve_sidebar_state`'s derived fields onto *sessions*.

  The callers hand rows they own (copies or fresh builds), so the write may
  mutate them; a holder of shared cache references must serve the derived
  dict alongside the rows instead (:meth:`SessionSearch.search_sessions_readonly`).
  """
  for meta in sessions:
    entry = derived[meta.id]
    if include_running_status:
      meta.has_running_tasks = entry[sidebar_state.HAS_RUNNING_TASKS]
      if sidebar_state.WORK_STATE in entry:
        meta.work_state = entry[sidebar_state.WORK_STATE]  # type: ignore[assignment]
    if include_pending_trigger_status:
      meta.has_pending_trigger = entry[sidebar_state.HAS_PENDING_TRIGGER]
      meta.pending_trigger_count = entry[sidebar_state.PENDING_TRIGGER_COUNT]
      meta.next_trigger_at = entry[sidebar_state.NEXT_TRIGGER_AT]
    if include_pending_plan_approval:
      meta.has_pending_plan_approval = entry[sidebar_state.HAS_PENDING_PLAN_APPROVAL]


# ---------------------------------------------------------------------------
# Sidebar probe cores — pure path-in/result-out functions shared by the
# per-session probe methods below and by the poll's serial re-probe (one
# asyncio.to_thread task over all sessions instead of one task per session
# per probe group).
# ---------------------------------------------------------------------------

# Parsed trigger files keyed by path: the sidebar deep probe re-enters this scan on every poll
# that follows any write to the session, and re-reading every trigger file
# dominated the probe (~3.6 ms per probe on the 101-file worst corpus); a repeat
# scan pays one scandir + stat per file and reads only files whose signature
# moved — the same rename-publish ground TriggerManager.list_triggers
# (src/runtime/triggers.py) states once for both memos (the stat-before-read race
# contract is StatSignatureMemo's). Stored dicts are shared across calls —
# consumers must treat them as read-only.
_TRIGGER_META_MEMO_LIMIT = 1024
_trigger_meta_memo: StatSignatureMemo[str, dict] = StatSignatureMemo(_TRIGGER_META_MEMO_LIMIT)

# The trigger scan's directory verdict: dir path -> (dir (mtime_ns, size),
# pending count, earliest fire). Signed on the directory's (mtime_ns, size) on
# the rename-publish ground TriggerManager.list_triggers (src/runtime/triggers.py)
# states once; the steady-state scan serves it for one directory stat without
# the scandir+stat walk or the per-file memo loop.
_TRIGGER_STATE_VERDICT_LIMIT = 1024
_trigger_state_verdicts: BoundedMemo[str, tuple[tuple[int, int], int,
                                                datetime | None]] = BoundedMemo(_TRIGGER_STATE_VERDICT_LIMIT)

# The probe's trigger-dir walk memo: (trigger dir path, its st_mode) -> the
# walk's (path, stat) pairs, signed on the directory's (mtime_ns, size). Every
# trigger writer publishes by rename into the directory, so any pair-changing
# write moves the directory's own stat pair (the rename-publish ground
# TriggerManager.list_triggers states once) and the fresh per-file stat list
# below is the only content witness; the scandir+stat phase rides the memo, the
# phase the sidebar's 10th-poll sweep repeats for every active session. The
# mode rides the key so a permission change misses into the scandir's own
# error. Served lists are shared across calls — consumers treat them read-only.
# Like the threads walk memo, the cap must hold every active session's entry:
# the sweep walks the whole corpus in one pass, and an LRU under that corpus
# thrashes (each session re-scandirs per sweep).
_TRIGGER_WALK_MEMO_LIMIT = 4096
_trigger_walk_pairs: StatSignatureMemo[tuple[str, int],
                                       list[tuple[str, os.stat_result]]] = StatSignatureMemo(_TRIGGER_WALK_MEMO_LIMIT)


def _iter_trigger_stats(triggers_dir: str, dir_st: os.stat_result) -> list[tuple[str, os.stat_result]]:
  """The shared trigger-dir stat walk (src.runtime.trigger_files.iter_trigger_file_stats).

  *dir_st* is the directory stat the caller holds from before its walk — the
  memo's stat-before-read half.
  """
  memo_key = (triggers_dir, dir_st.st_mode)
  pairs = _trigger_walk_pairs.fresh(memo_key, dir_st)
  if pairs is not None:
    return pairs
  pairs = trigger_files.iter_trigger_file_stats(triggers_dir)
  _trigger_walk_pairs.record(memo_key, dir_st, pairs)
  return pairs


def _parse_optional_utc(raw: Any, log_event: str, **log_ctx: Any) -> datetime | None:
  """Parse a stored timestamp tolerantly for trigger and plan state scans."""
  if not raw:
    return None
  try:
    return parse_utc_datetime(raw)
  except ValueError as e:
    log.debug(log_event, **log_ctx, error=str(e))
    return None


def pending_trigger_state_sync(
    triggers_dir: Path,
    walked: list[tuple[str, os.stat_result]] | None,
    dir_sig: tuple[int, int] | None,
) -> tuple[int, datetime | None]:
  """(pending trigger count, earliest fire time) from the *.json files under *triggers_dir*.

  Steady state pays one directory stat: an unchanged (mtime_ns, size) of the
  directory serves the stored verdict without the scandir+stat walk or the
  per-file memo loop, on the rename-publish ground TriggerManager.list_triggers
  (src/runtime/triggers.py) states once. The signature is taken before the walk,
  so a write landing mid-walk moves the directory past the stored signature and
  the next call re-walks; within one proved directory state, a file that fails
  to parse re-reads and re-warns once for that state, not once per call.

  *walked* supplies (path, stat) pairs a caller already walked, replacing this
  scan's own scandir+stat phase; *dir_sig* must then be the directory's
  (mtime_ns, size) taken at that walk's instant (the verdict keys on it, so a
  sig newer than the walked contents could never be stored). Without *walked*
  the scan stats the directory itself before its scandir, and *dir_sig* is
  ignored. Non-regular *.json* entries (a directory named like a trigger file)
  are skipped off the walked stat, the same ``is_file`` gate the self-walked
  path applies before its stat.
  """
  triggers_str = os.fspath(triggers_dir)
  if walked is None:
    try:
      dst = os.stat(triggers_str)
    except OSError:
      _trigger_state_verdicts.drop(triggers_str)
      return 0, None
    if not stat.S_ISDIR(dst.st_mode):
      _trigger_state_verdicts.drop(triggers_str)
      return 0, None
    dir_sig = (dst.st_mtime_ns, dst.st_size)
  if dir_sig is not None:
    verdict = _trigger_state_verdicts.get(triggers_str)
    if verdict is not None and verdict[0] == dir_sig:
      return verdict[1], verdict[2]
  if walked is None:
    walked = _iter_trigger_stats(triggers_str, dst)

  pending_count = 0
  next_trigger_at: datetime | None = None
  for trigger_path, st in walked:
    if not stat.S_ISREG(st.st_mode):
      continue
    # The memo keys on the walked string path directly, the same string-path
    # pattern the thread-metadata memo uses.
    trigger = _trigger_meta_memo.fresh(trigger_path, st)
    if trigger is None:
      trigger = load_json_meta(
          Path(trigger_path),
          "trigger_meta_read_failed",
          catch=(OSError, ValueError),
      )
      if trigger is None:
        continue
      _trigger_meta_memo.record(trigger_path, st, trigger)
    if trigger.get("status") != "pending":
      continue

    pending_count += 1
    fire_at = _parse_optional_utc(trigger.get("fire_at"), "trigger_fire_at_parse_failed", trigger_path=trigger_path)
    if fire_at is None:
      continue
    if next_trigger_at is None or fire_at < next_trigger_at:
      next_trigger_at = fire_at

  if dir_sig is not None:
    _trigger_state_verdicts.store(triggers_str, (dir_sig, pending_count, next_trigger_at))
  return pending_count, next_trigger_at


# The detached every-10th-poll self-heal sweep (single-flight holder). The
# snapshot it serves is process-global, so the task is module state, not
# per-manager state.
_sidebar_sweep_task: asyncio.Task | None = None


class _WalkedProbeInputs(NamedTuple):
  """The trigger files and directory signature captured during one probe walk."""

  trigger_files: list[tuple[str, os.stat_result]] | None
  trigger_dir_sig: tuple[int, int] | None


class SidebarProbeSpec(NamedTuple):
  """One task node's trigger, contribution, and task-run activity probe inputs.

  ``session_dir`` is where the sidebar contributions' watched files and flags
  live; the walk extends the trigger scan with the files the task-tree
  activity derivation reads (a node's metadata, its fact history, its Run
  records); ``recheck_liveness`` marks a node whose stored verdict is
  ``running`` — the self-heal sweep must re-judge its recorded process
  identity even when no covered file moved, because a process death writes
  nothing.
  """
  session_id: str
  triggers_dir: Path
  session_dir: Path
  recheck_liveness: bool = False


def _task_tree_probe_signature(session_dir_str: str) -> tuple:
  """Stat-only identity of every file the task-tree activity derivation reads.

  The derivation reads the node's fact history (metadata.json's archive offset,
  the live chat_events.jsonl, the archived segments under data/archives) and
  every Run record under data/runs. An unchanged signature proves the
  derivation's inputs unchanged, so a fresh-signature poll never hides a
  changed state — and never pays the derivation's reads either.
  """
  metadata_sig = stat_signature(session_dir_str + "/metadata.json")
  events_sig = stat_signature(session_dir_str + "/data/chat_events.jsonl")
  archives_sig = stat_signature(session_dir_str + "/data/archives")
  runs_sig: list[tuple[str, int, int]] = []
  runs_dir_str = session_dir_str + "/data/runs"
  try:
    run_names = sorted(os.listdir(runs_dir_str))
  except OSError:
    run_names = []
  for name in run_names:
    run_meta = stat_signature(runs_dir_str + "/" + name + "/metadata.json")
    if run_meta is not None:
      runs_sig.append((name, run_meta[0], run_meta[1]))
  return (metadata_sig, events_sig, archives_sig, tuple(runs_sig))


def probe_sidebar_state_sync(
    specs: list[SidebarProbeSpec],
    walked: dict[str, _WalkedProbeInputs] | None,
) -> dict[str, dict]:
  """Probe every spec serially.

  The deep-probe core of a sidebar re-probe: all probe groups per session, one
  session at a time, so probing N sessions costs one thread-pool task instead
  of 3*N. Returns
  ``{session_id: {"pending_trigger_count", "next_trigger_at",
  "has_pending_plan_approval"}}``; the last comes from the sidebar contributions'
  ``row_flags``.

  *walked* maps session id to the stat pairs a probe-input walk already took
  for that session; the trigger core consumes them instead of re-taking the
  same scandir+stat phase.
  """
  results: dict[str, dict] = {}
  contributions = sidebar_contributions()
  for spec in specs:
    inputs = walked.get(spec.session_id) if walked is not None else None
    pending_count, next_trigger_at = pending_trigger_state_sync(
        spec.triggers_dir,
        walked=inputs.trigger_files if inputs else None,
        dir_sig=inputs.trigger_dir_sig if inputs else None,
    )
    entry = {
        sidebar_state.PENDING_TRIGGER_COUNT: pending_count,
        sidebar_state.NEXT_TRIGGER_AT: next_trigger_at,
        sidebar_state.HAS_PENDING_PLAN_APPROVAL: False,
    }
    for contribution in contributions:
      entry.update(contribution.row_flags(spec.session_dir, spec.session_id))
    results[spec.session_id] = entry
  return results


def _sidebar_probe_walk(spec: SidebarProbeSpec, watched_files: tuple[str, ...]) -> tuple[tuple, _WalkedProbeInputs]:
  """Stat-only identity of every byte the sidebar probe reads, plus the walk's stat pairs.

  A deep probe's result can change only two ways, and the signature pins both:
  a probed file's content changes (caught by ``(st_mtime_ns, st_size)`` for
  both atomic-rename and in-place writers), or a probed file appears or
  disappears among the readable entries (caught by the sorted name sets — a
  trigger file whose stat fails contributes nothing until the file lands, and
  its arrival then moves the set). The stat pass mirrors the probe cores' own
  scandir+stat phase, so a signature sweep costs the cheap half of a probe and
  skips every content read and parse. The sidebar contributions'
  *watched_files* (paths relative to *session_dir*) ride the signature as one
  ``(mtime_ns, size)`` entry each, so a contribution's flags re-derive exactly
  when a file it reads changed. String paths instead of Path objects: the
  sweep runs per poll over every selected session, and pathlib's parse/alloc
  overhead would dominate the raw stat syscalls — os.stat on joined strs
  measures ~2x faster over the active-session corpus.

  The walked pairs ride along for the deep probe (:func:`probe_sidebar_state_sync`
  with *walked*).
  """
  trigger_sig = []
  trigger_pairs: list[tuple[str, os.stat_result]] | None = None
  trigger_dir_sig: tuple[int, int] | None = None
  triggers_str = os.fspath(spec.triggers_dir)
  try:
    dir_st = os.stat(triggers_str)
  except OSError:
    dir_st = None
  if dir_st is not None and stat.S_ISDIR(dir_st.st_mode):
    trigger_dir_sig = (dir_st.st_mtime_ns, dir_st.st_size)
    trigger_pairs = _iter_trigger_stats(triggers_str, dir_st)
    trigger_sig = [(os.path.basename(path), st.st_mtime_ns, st.st_size) for path, st in trigger_pairs]
  session_dir_str = os.fspath(spec.session_dir)
  watched_sig = tuple(stat_signature(session_dir_str + "/" + rel) for rel in watched_files)
  # The task-tree derivation reads its fact history and Run records; the
  # signature covers those files too, so an unchanged signature can never hide
  # a changed state (and a fresh-signature poll skips their reads).
  task_sig = _task_tree_probe_signature(session_dir_str)
  signature = (tuple(sorted(trigger_sig)), watched_sig, task_sig)
  return signature, _WalkedProbeInputs(trigger_pairs, trigger_dir_sig)


def _sidebar_signature_fresh(session_id: str, signature: tuple) -> bool:
  """True when the stored probe signature equals *signature*."""
  return sidebar_state.probe_signature(session_id) == signature


def _store_probe_results(probed: dict[str, dict], probe_sigs: dict[str, tuple]) -> None:
  """Persist one probe round's outputs into :mod:`src.runtime.sidebar_state`: snapshot entries, then signatures."""
  for session_id, entry in probed.items():
    sidebar_state.store_snapshot_entry(session_id, entry)
  for session_id, sig in probe_sigs.items():
    sidebar_state.store_probe_signature(session_id, sig)


def selective_probe_sidebar_state(
    specs: list[SidebarProbeSpec],
    *,
    deep: bool,
    task_probe: Callable[[str], tuple[bool, str]] | None = None,
) -> tuple[dict[str, dict], dict[str, tuple]]:
  """Deep-probe exactly the specs whose probe inputs changed since the last probe.

  Composes one signature stat pass with :func:`probe_sidebar_state_sync`-shaped
  probing of the changed specs so one thread-pool task covers selection and
  execution. ``deep=True`` probes every spec regardless of signatures (the
  ``/status?force=1`` escape hatch). A spec whose ``recheck_liveness`` stands is
  probed whatever its signature says: its stored verdict holds a live Run, and
  a process death changes no file the signature covers. ``task_probe`` derives
  a task-tree node's activity through the task-tree owner's own derivation —
  it runs only for specs actually selected for probing, so a clean node pays
  neither it nor its /proc reads. Returns ``(entries, signatures)``; the caller
  stores both in :mod:`src.runtime.sidebar_state` on the event loop.
  """
  sigs: dict[str, tuple] = {}
  walked_inputs: dict[str, _WalkedProbeInputs] = {}
  to_probe: list[SidebarProbeSpec] = []
  watched_files = tuple(
      dict.fromkeys(rel for contribution in sidebar_contributions() for rel in contribution.watched_files))
  for spec in specs:
    if not isinstance(spec, SidebarProbeSpec):
      spec = SidebarProbeSpec(*spec)
    sig, inputs = _sidebar_probe_walk(spec, watched_files)
    sigs[spec.session_id] = sig
    walked_inputs[spec.session_id] = inputs
    if deep or spec.recheck_liveness or not _sidebar_signature_fresh(spec.session_id, sig):
      to_probe.append(spec)
  entries = probe_sidebar_state_sync(to_probe, walked_inputs)
  if task_probe is not None:
    for spec in to_probe:
      has_running, work_state = task_probe(spec.session_id)
      entries[spec.session_id][sidebar_state.TASK_TREE_ACTIVITY] = (has_running, work_state)
  return entries, sigs


class SessionSidebar:
  """Sidebar run flags over the session metadata store."""

  def __init__(self, cfg: CharlieBotConfig, store: session_store.SessionStore) -> None:
    self._cfg = cfg
    self._store = store
    # The task-tree owner registers its activity derivation here at wiring
    # time (TaskTreeManager.__init__): session id -> (has_running_tasks,
    # work_state), the same derivation the tree projection answers with. The
    # sidebar probe calls it for a task-tree node's deep probe, so a sidebar
    # row's state is the tree's own verdict, never a second copy of the rules.
    # None only before that wiring exists (no tree consumer in this process).
    self.task_tree_activity: Callable[[str], tuple[bool, str]] | None = None

  def _probe_spec(self, meta: SessionMetadata, *, recheck_liveness: bool = False) -> SidebarProbeSpec:
    """The probe-input spec :func:`selective_probe_sidebar_state` consumes.

    ``recheck_liveness`` marks a task-tree node whose stored verdict is
    ``running``: the self-heal sweep must re-judge its recorded process
    identity even when no covered file moved, because a process death writes
    nothing. The poll path leaves it False — a clean node's poll pays no
    /proc read.
    """
    return SidebarProbeSpec(
        meta.id,
        self._store.session_dir(meta.id) / "triggers", self._store.session_dir(meta.id), recheck_liveness)

  async def resolve_sidebar_state(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool,
      include_pending_trigger_status: bool,
      include_pending_plan_approval: bool = False,
      force: bool = False,
  ) -> dict[str, dict]:
    """Probe-or-serve the derived sidebar state for *sessions* without mutating them.

    Returns session id -> derived fields: ``has_running_tasks`` when
    *include_running_status*, ``has_pending_trigger`` / ``pending_trigger_count``
    / ``next_trigger_at`` when *include_pending_trigger_status*, and
    ``has_pending_plan_approval`` when *include_pending_plan_approval* — only
    the requested keys are present. ``has_running_tasks`` reads
    ``busy_since``, not the passed objects' ``thinking_since``, so callers may
    hand out cache references whose transient field was never stamped.

    Active sessions are served from the in-process sidebar snapshot
    (:mod:`src.runtime.sidebar_state`) with zero disk access, and re-probed — in
    ONE ``asyncio.to_thread`` task, serial over sessions, via the pure probe
    cores above — only the dirty sessions, or every active session on every
    10th call and whenever *force* is set (the ``/status?force=1`` escape
    hatch). The every-10th self-heal sweep runs detached (single-flight): the
    calling poll answers from the snapshot and dirty-set probes, and the
    sweep's results land for the polls that follow it, so a missed mark heals
    one poll later inside the same window. A selected probe first checks the
    stat-only probe-input signature: unchanged inputs (never-probed excepted)
    skip the deep read in every case except an explicit *force*. A session
    with no snapshot entry (cold boot, new session) is probed like a dirty
    one, so an empty snapshot is a full probe. Archived sessions keep the
    constant-False shortcut.

    A poll whose probed-state generation (:func:`sidebar_state.derived_generation`)
    still matches the last fold's serves that fold's map whole, so between
    state bumps every caller — and every concurrent poll — reads the same
    entry dicts. The map and its entries are read-only to callers, the
    contract every consumer here already holds.
    """
    if not sessions:
      return {}
    if not (include_running_status or include_pending_trigger_status or include_pending_plan_approval):
      return {meta.id: {} for meta in sessions}

    # Every input the fold below reads sits behind sidebar_state's generation:
    # the probe snapshot (store bumps), the busy map (thinking_state marks
    # through mark_sidebar_dirty), and the metadata fields partitioned on
    # (save_metadata's funnel mark). An unchanged generation therefore proves
    # the last fold's map current, and the poll serves it whole — the
    # per-session rebuild is the /status poll's largest handler term. Keyed on
    # the id tuple, not the metas: the read-only loaders hand fresh copies per
    # poll, so a list-identity key (the listings memo's ground) never hits.
    # force keeps its deep-probe teeth and bypasses the read; its fresh fold
    # still lands in the memo for the polls that follow.
    ids = tuple(meta.id for meta in sessions)
    flags = (include_running_status, include_pending_trigger_status, include_pending_plan_approval)
    generation = sidebar_state.derived_generation()
    force_full = sidebar_state.register_poll(force)
    if not force:
      cached = sidebar_state.peek_derived_map((ids, flags, generation))
      if cached is not None:
        if force_full:
          # The every-10th sweep serves the polls that follow it: its stores
          # bump the generation and the next poll re-derives with its results.
          self.schedule_sidebar_sweep([m for m in sessions if m.status != SessionStatus.ARCHIVED])
        return cached

    # Archived sessions cannot have running tasks or pending triggers, so skip
    # the per-session filesystem work for them.
    active_sessions = [m for m in sessions if m.status != SessionStatus.ARCHIVED]
    archived_sessions = [m for m in sessions if m.status == SessionStatus.ARCHIVED]

    derived: dict[str, dict] = {}
    for meta in archived_sessions:
      derived[meta.id] = _sidebar_entry(
          include_running_status,
          include_pending_trigger_status,
          include_pending_plan_approval,
          running=False,
          trigger_count=0,
          next_trigger_at=None,
          plan_approval=False)

    if not active_sessions:
      return derived

    if force_full and not force:
      # The self-heal sweep serves the polls that follow it, not this request:
      # it runs detached (single-flight), so the poll's wall stays at the
      # dirty-set cost and a missed mark heals one poll later, inside the same
      # every-10th window. force=1 keeps its synchronous full probe.
      self.schedule_sidebar_sweep(active_sessions)
      force_full = False
    metas_by_id = {meta.id: meta for meta in active_sessions}
    if force_full:
      probe_ids = [meta.id for meta in active_sessions]
    else:
      probe_ids = [
          meta.id
          for meta in active_sessions
          if sidebar_state.is_dirty(meta.id) or sidebar_state.snapshot_entry(meta.id) is None
      ]
    # Selection-time removal: a transition mark landing while the probe runs
    # re-adds the id, so that write is re-probed by the next poll even if the
    # in-flight probe raced it.
    sidebar_state.discard_dirty(probe_ids)

    if probe_ids:
      specs = [self._probe_spec(metas_by_id[session_id]) for session_id in probe_ids]
      try:
        # Explicit force keeps its teeth as the escape hatch: it deep-probes
        # every selected session. Narrowed sweeps (the every-10th self-heal)
        # deep-probe only sessions whose probe-input signature moved. The
        # task-tree owner's derivation rides as task_probe: only a selected
        # task node pays it, and the verdict it answers with is the tree's own.
        probed, probe_sigs = await asyncio.to_thread(
            selective_probe_sidebar_state, specs, deep=force, task_probe=self.task_tree_activity)
      except BaseException:
        # A failed probe must not lose the dirty state it was serving.
        for session_id in probe_ids:
          sidebar_state.mark_sidebar_dirty(session_id)
        raise
      _store_probe_results(probed, probe_sigs)

    for meta in active_sessions:
      probed = sidebar_state.required_snapshot_entry(meta.id)
      derived[meta.id] = _sidebar_entry(
          include_running_status,
          include_pending_trigger_status,
          include_pending_plan_approval,
          running=bool(busy_since(meta.id)),
          trigger_count=probed[sidebar_state.PENDING_TRIGGER_COUNT],
          next_trigger_at=probed[sidebar_state.NEXT_TRIGGER_AT],
          plan_approval=bool(probed[sidebar_state.HAS_PENDING_PLAN_APPROVAL]),
          task_activity=probed.get(sidebar_state.TASK_TREE_ACTIVITY),
      )
    # Keyed at the generation read before the probe: a mark landing inside the
    # probe's await bumps past it, so a raced round can never be served for the
    # state it raced — the next poll re-derives and drains the mark.
    sidebar_state.store_derived_map((ids, flags, generation), derived)
    return derived

  def schedule_sidebar_sweep(self, sessions: list[SessionMetadata]) -> None:
    """Run the every-10th-poll self-heal sweep detached from the caller's request.

    Single-flight: a sweep still running covers the window, so a poll landing
    inside it schedules nothing and the next sweep slot tries again. Sessions
    carrying a dirty mark are left to the synchronous polls — a mark must be
    consumed only by a probe whose result lands in a response, so its
    freshness contract stays one poll.
    """
    global _sidebar_sweep_task
    if _sidebar_sweep_task is not None and not _sidebar_sweep_task.done():
      return

    def _spec(meta: SessionMetadata) -> SidebarProbeSpec:
      # A task-tree node whose stored verdict is running holds a launched Run
      # without a terminal fact: its liveness must be re-judged here even when
      # no covered file moved, because a process death writes nothing.
      activity = sidebar_state.snapshot_task_activity(meta.id)
      return self._probe_spec(meta, recheck_liveness=activity is not None and activity[1] == "running")

    specs_template = [_spec(meta) for meta in sessions]

    async def _run() -> None:
      specs = [spec for spec in specs_template if not sidebar_state.is_dirty(spec.session_id)]
      if not specs:
        return
      try:
        probed, probe_sigs = await asyncio.to_thread(
            selective_probe_sidebar_state, specs, deep=False, task_probe=self.task_tree_activity)
      except asyncio.CancelledError:
        # Loop-teardown cancellation is not a sweep failure: the caller that
        # would consume the results is gone and the probe thread's outcome is
        # discardable by design. Re-raise so the task ends cancelled — which
        # create_logged_task treats as clean — instead of logging a teardown
        # traceback as a failure.
        raise
      except BaseException:
        # A failed sweep must not lose the state it was serving: re-mark every
        # selected session so the next poll re-probes it.
        log.exception("sidebar_self_heal_sweep_failed")
        for spec in specs:
          sidebar_state.mark_sidebar_dirty(spec.session_id)
        return
      _store_probe_results(probed, probe_sigs)

    _sidebar_sweep_task = create_logged_task(_run(), name="sidebar-self-heal-sweep")

  async def populate_sidebar_state(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool,
      include_pending_trigger_status: bool,
      include_pending_plan_approval: bool = False,
  ) -> None:
    """Apply :meth:`resolve_sidebar_state`'s derived fields onto *sessions*.

    The callers hand copies they own, so the write may mutate them; a
    read-only consumer holding cache references must call
    :meth:`resolve_sidebar_state` directly instead.
    """
    derived = await self.resolve_sidebar_state(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
        include_pending_plan_approval=include_pending_plan_approval,
        force=False,
    )
    apply_sidebar_state(
        sessions, derived, include_running_status, include_pending_trigger_status, include_pending_plan_approval)


# The process owner of the sidebar block; built on the first ``sidebar()`` call.
_sidebar: SessionSidebar | None = None


def sidebar() -> SessionSidebar:
  """The process-wide session sidebar block."""
  global _sidebar
  if _sidebar is None:
    _sidebar = SessionSidebar(get_config(), session_store.store())
  return _sidebar

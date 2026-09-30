"""The v2 scheduled-task path: one bound node, stable firings, Runs and one report.

A scheduled task with an explicit ``session_id`` binding fires against that
stable task-tree node:

- ``mode: master`` admits one typed scheduled input (``SCHEDULED_TRIGGER``,
  server-owned provenance: the input id derives from the task name and the
  firing's due time, never from a payload bit a run-token caller can forge) to
  the bound manager; the shared dispatcher launches the manager's turn.
- worker mode (the default) creates ONE worker leaf per distinct
  firing under the bound manager. Steps share that leaf as ``scheduled_step``
  Runs with ``sequence_ref=(cron_steps, owner_ref=<firing>, position=<i>)``;
  the chain advances on a step's success and stops on its failure, exactly as
  the legacy steps controller did, and ONE report is delivered at the sequence
  boundary. Single-prompt and loop firings ride the ordinary work-Run delivery
  chain: a successful run auto-closes the leaf and reports to the manager
  through the common completion owner.

Boundaries this module pins:

- The firing identity is durable and source-derived (task name + the due
  occurrence's time), so a repeated scan or a crash between admission and the
  scheduler's bookkeeping keeps choosing the SAME firing — an intentional new
  firing (the next due occurrence, or a manual run) is distinct.
- A missing, non-task-tree, closed, or non-manager (for mode: master
  and leaf creation) binding fails the fire VISIBLY and never silently creates
  a replacement session.
- The bound node's backend resolution, per-step backends, allow_failure,
  stop-on-failure, prompt_file reloading, loop actions, overlap behavior and
  system handler mode are preserved; the handler mode keeps its inline system
  execution and only borrows the bound node for its bookkeeping.
- Recovery consumes the same pure functions: the durable step Runs and their
  sequence positions decide the frontier, so a repeated pass never duplicates
  a step, a report, or a close.
"""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

from src.core import event_types as ET
from src.core import review
from src.core.config import ScheduledTaskConfig, get_scheduled_tasks
from src.core.control_events import stable_run_id
from src.core.log_once import LazyStructlogLogger
from src.core.models import RunRecord, SequenceRef, SessionMetadata, TaskSpec, TaskType
from src.core.runs import RUN_EVENTS_NAME
from src.core.task_completion import RUN_REF_PREFIX

if TYPE_CHECKING:
  from src.core.task_execution import LaunchSettlement
  from src.core.task_sessions import TaskTreeManager

log = LazyStructlogLogger()


class ScheduledBindingError(RuntimeError):
  """The configured binding cannot serve this fire (never silently replaced)."""


def firing_ref_prefix(owner_ref: str) -> tuple[str, str] | None:
  """(task name, firing) from a cron_steps owner_ref.

  Cron task names never contain ':' (the loader's name guard), so the first
  separator after the 'cron:' scheme splits them; anything else is malformed
  and refused by the caller.
  """
  if not owner_ref.startswith("cron:"):
    return None
  rest = owner_ref[len("cron:"):]
  name, sep, firing = rest.partition(":")
  if not sep or not name or not firing:
    return None
  return name, firing


def load_bound_task(task_name: str, cfg: object) -> ScheduledTaskConfig | None:
  """The current config of one named scheduled task, or None when unconfigured.

  Reads the task's own cron.d file under this instance's home so recovery can
  serve a scoped instance without the process-global cron snapshot.
  """
  import logging

  from src.core.config import _load_cron_file

  path = cfg.charliebot_home / "config.d" / "cron.d" / f"{task_name}.yaml"
  if not path.is_file():
    return None
  try:
    task, _mtimes = _load_cron_file(path, cfg.charlie_bot_repo, task_name)
  except Exception as exc:
    logging.getLogger(__name__).warning(
        "cron_sequence_task_config_unreadable", task=task_name, path=str(path), error=str(exc))
    return None
  return task


def bound_task_name(session_id: str, tasks: list[ScheduledTaskConfig] | None = None) -> str | None:
  """The loaded scheduled task whose ``session_id`` binding names *session_id*.

  The one "is this node bound by a task" judgment, derived from the loaded task
  configs' ``session_id`` and never from a session's ``scheduled_task`` stamp:
  every duty that moves from the legacy cron session to the bound node keys on
  this one implementation. *tasks* injects the caller's snapshot so a consumer
  that already holds one (the sidebar lists' schedule join) reads a single
  generation; the default loads the current one.
  """
  for task_cfg in (tasks if tasks is not None else get_scheduled_tasks()):
    if task_cfg.session_id == session_id:
      return task_cfg.name
  return None


def chain_step_prompt(prompt: str, previous_name: str, previous_result: str) -> str:
  """One step's prompt with the previous step's result under the legacy heading.

  The heading is worker-facing contract: ``memory_reviewer.md`` in
  ``prompts/cron/memory_curator/`` tells a step to read its input under it,
  so the format lives only here.
  """
  return f"{prompt.rstrip()}\n\n## Result of the previous step ({previous_name})\n{previous_result}"


def firing_ref(task_cfg: ScheduledTaskConfig, firing: str) -> str:
  """The durable owner_ref of one firing: task name + the due occurrence."""
  return f"cron:{task_cfg.name}:{firing}"


def leaf_request_id(task_cfg: ScheduledTaskConfig, firing: str) -> str:
  return f"{firing_ref(task_cfg, firing)}:leaf"


def step_run_request_id(task_cfg: ScheduledTaskConfig, firing: str, position: int) -> str:
  return f"{firing_ref(task_cfg, firing)}:step:{position}"


def work_run_request_id(task_cfg: ScheduledTaskConfig, firing: str) -> str:
  return f"{firing_ref(task_cfg, firing)}:work"


async def resolve_binding(
    task_cfg: ScheduledTaskConfig,
    tree: TaskTreeManager,
) -> SessionMetadata:
  """Load the bound task-tree node; every invalid binding fails loudly.

  A bound task never discovers or creates a session: the binding IS the node.
  A missing session, a legacy (non-task-tree) session, or — for the paths that
  execute against it — a closed node stops the fire here.
  """

  if not task_cfg.session_id:
    raise ScheduledBindingError(f"scheduled task '{task_cfg.name}' carries no session_id binding")
  meta = await tree.load_meta(task_cfg.session_id)
  if meta is None:
    raise ScheduledBindingError(
        f"scheduled task '{task_cfg.name}' binds session {task_cfg.session_id}, which does not exist")
  if meta.profile is None:
    raise ScheduledBindingError(
        f"scheduled task '{task_cfg.name}' binds session {task_cfg.session_id}, "
        "which is not a task-tree node (legacy sessions keep the legacy path)")
  return meta


async def check_fireable_binding(
    task_cfg: ScheduledTaskConfig,
    tree: TaskTreeManager,
) -> SessionMetadata:
  """The binding a NEW cron execution may start against.

  Closed bound nodes generate no new cron execution (the configuration itself
  stays readable); a mode: master fire and a leaf creation additionally
  require a manager node. Nothing here ever creates a replacement.
  """
  meta = await resolve_binding(task_cfg, tree)
  state = tree.task_state(meta.id)
  if state != "open":
    raise ScheduledBindingError(
        f"scheduled task '{task_cfg.name}' binds task {meta.id}, which is {state}; "
        "a closed task generates no new cron execution")
  if task_cfg.mode == "master" and meta.profile != "manager":
    raise ScheduledBindingError(
        f"scheduled task '{task_cfg.name}' (mode master) binds task {meta.id}, "
        "which is not a manager")
  return meta


# ---------------------------------------------------------------------------
# Admission (mode: master)
# ---------------------------------------------------------------------------


async def fire_bound_master(
    task_cfg: ScheduledTaskConfig,
    meta: SessionMetadata,
    tree: TaskTreeManager,
    firing: str,
) -> dict:
  """Admit one typed scheduled input to the bound manager and dispatch it.

  The input id derives from the task name and the firing, so a crash between
  the durable admission and the scheduler's bookkeeping replays into the same
  input — never a duplicate wake. A manager turn that is already consuming
  inputs leaves this one pending for the next serialized turn (the existing
  overlap semantics, expressed by the dispatcher).
  """
  input_id = firing_ref(task_cfg, firing)
  admitted = await tree.dispatch.admit_input(
      meta.id,
      event_type=ET.SCHEDULED_TRIGGER,
      content=task_cfg.prompt or "",
      actor="system",
      input_id=input_id,
  )
  await tree.dispatch.dispatch_pending(meta.id)
  log.info("bound_master_fired", task=task_cfg.name, session=meta.id, firing=firing, input_id=str(admitted.get("id")))
  return {"session_id": meta.id, "firing": firing, "input_event_id": str(admitted.get("id"))}


# ---------------------------------------------------------------------------
# Leaf creation (worker modes)
# ---------------------------------------------------------------------------


async def ensure_firing_leaf(
    task_cfg: ScheduledTaskConfig,
    meta: SessionMetadata,
    tree: TaskTreeManager,
    firing: str,
    goal: str,
    task_type: TaskType | None = None,
) -> SessionMetadata:
  """Create (or re-admit) the ONE worker leaf this firing owns.

  Stable by (parent, firing): a replayed fire returns the original leaf. The
  leaf carries no task_type unless the caller passes one (the loop backlog's
  implement action delegates implementation, so its leaf is an implement task
  whose delivery rides the review + repository landing policy with the repo's
  default branch as the merge target) — the scheduled work's applicable
  evidence policy is otherwise the run's own result truth reported to the
  manager.

  The parent is the bound manager, or an unbound task's cron session: a legacy
  session (profile None) parents its firings' leaves in place, exactly as the
  delegation path parents a worker under it.
  """
  from src.core.task_sessions import TaskInvalidError

  if meta.profile not in ("manager", None):
    raise TaskInvalidError(
        f"scheduled task '{task_cfg.name}' cannot create a worker leaf under "
        f"{meta.id}: the parent is neither a manager nor the task's legacy cron session")
  return await tree.create_task(
      request_id=leaf_request_id(task_cfg, firing),
      task_parent_id=meta.id,
      profile="worker",
      task=TaskSpec(
          goal=goal,
          repo_path=task_cfg.repo,
          base_branch=None,
          task_type=task_type,
      ),
      name=f"{task_cfg.name} · {firing}",
      backend=None,
      caller="system",
  )


async def register_leaf_run(
    tree: TaskTreeManager,
    leaf_id: str,
    task_cfg: ScheduledTaskConfig,
    firing: str,
    *,
    kind: str,
    position: int | None,
    backend: str,
    model: str | None,
) -> RunRecord:
  """Register the firing's work Run (or one step's scheduled_step Run)."""
  if kind == "work":
    request_id = work_run_request_id(task_cfg, firing)
  else:
    assert position is not None
    request_id = step_run_request_id(task_cfg, firing, position)
  run_id = stable_run_id(leaf_id, request_id)
  existing = await tree.runs.get_run(leaf_id, run_id)
  if existing is not None:
    return existing
  record = RunRecord(
      id=run_id,
      session_id=leaf_id,
      kind=kind,  # type: ignore[arg-type]
      backend=backend,
      model=model,
      repo_path=task_cfg.repo,
      sequence_ref=(
          SequenceRef(kind="cron_steps", owner_ref=firing_ref(task_cfg, firing), position=position)
          if position is not None else None),
  )
  async with tree.control_lock:
    await tree.runs.register_run_locked(record, task_spec_text=None)
  fresh = await tree.runs.get_run(leaf_id, run_id)
  assert fresh is not None
  return fresh


async def run_result_text(tree: TaskTreeManager, leaf_id: str, run_id: str) -> str:
  """One Run's closing words from its translated events log ('' without any)."""
  events_path = tree.runs.run_dir(leaf_id, run_id) / RUN_EVENTS_NAME
  if not events_path.is_file():
    return ""
  return await asyncio.to_thread(review._worker_summary_from_events_log, events_path)


# ---------------------------------------------------------------------------
# Steps sequence (the existing controller's semantics over durable facts)
# ---------------------------------------------------------------------------


async def run_firing_steps(
    task_cfg: ScheduledTaskConfig,
    meta: SessionMetadata,
    tree: TaskTreeManager,
    firing: str,
    leaf_id: str,
) -> None:
  """Advance one firing's steps chain to its boundary and deliver ONE report.

  Each step is one ``scheduled_step`` Run on the shared leaf; a step's success
  launches the next (fed the previous result under the legacy heading), a
  failure stops the chain. The boundary report carries one block per executed
  step; a fully successful chain closes the leaf through the common completion
  owner (whose close delivers the report), a stopped chain reports the
  failure and keeps the leaf open with its evidence.
  """
  steps = task_cfg.steps or []
  conflict = distinct_backend_conflict(task_cfg, tree)
  if conflict is not None:
    # No step launches — not even the Run the admission already registered —
    # and the failure report names both steps and the shared backend.
    summary = f"Scheduled task '{task_cfg.name}' stopped before its first step: {conflict}."
    await deliver_boundary_report(tree, leaf_id, meta.id, task_cfg, firing, "failed", summary)
    return
  executed: list[tuple[int, str, str, str, str]] = []  # (position, run_id, name, backend, result)
  failed_at: int | None = None
  for position, step in enumerate(steps):
    backend, model = resolved_backend_model(task_cfg, tree, step.backend or task_cfg.backend)
    run = await register_leaf_run(
        tree, leaf_id, task_cfg, firing, kind="scheduled_step", position=position, backend=backend, model=model)
    outcome = await tree.runs.terminal_outcome_of(leaf_id, run.id)
    if outcome is None:
      prompt = step.prompt or ""
      if position > 0:
        previous = executed[-1][4] if executed else ""
        prompt = chain_step_prompt(prompt, steps[position - 1].name, previous)
      observation = await launch_and_settle(tree, leaf_id, run.id, prompt)
      if observation.withheld is not None:
        # No process started and no terminal fact will arrive: the chain
        # cannot advance and ends here. The launch itself recorded the
        # durable run_launch_withheld fact and delivered the ONE blocked
        # report to the parent (once, by stable id) — the controller only
        # releases the scheduler's overlap handle.
        return
      outcome = observation.outcome
      assert outcome is not None
    result = await run_result_text(tree, leaf_id, run.id)
    executed.append((position, run.id, step.name, backend, result))
    if outcome != "success" or not await step_advanced(tree, leaf_id, run.id):
      failed_at = position
      break

  blocks = [
      f"**{name} result ({backend}):**\n{result or '(no result)'}" for _pos, _rid, name, backend, result in executed
  ]
  if failed_at is None:
    last_run_id = executed[-1][1]
    summary = f"Scheduled task '{task_cfg.name}' completed all {len(executed)} step(s).\n\n" + "\n\n".join(blocks)
    try:
      await tree.completion.evaluate_automatic_completion(
          leaf_id,
          run_id=last_run_id,
          summary=summary,
          result_refs=[f"{RUN_REF_PREFIX}{rid}" for _pos, rid, _n, _b, _r in executed],
          request_id=f"auto:{firing_ref(task_cfg, firing)}",
      )
      return  # the close delivered the report to the bound manager
    except Exception as e:
      log.warning("cron_sequence_close_blocked", task=task_cfg.name, leaf=leaf_id, error=str(e))
      # Fall through to the explicit report so the boundary result still lands.
  else:
    summary = (
        f"Scheduled task '{task_cfg.name}' stopped at step '{steps[failed_at].name}' "
        f"(outcome {await tree.runs.terminal_outcome_of(leaf_id, executed[-1][1])}); no later step ran.")
  await deliver_boundary_report(
      tree, leaf_id, meta.id, task_cfg, firing, "failed" if failed_at is not None else "blocked", summary)


async def deliver_boundary_report(
    tree: TaskTreeManager,
    leaf_id: str,
    recipient: str,
    task_cfg: ScheduledTaskConfig,
    firing: str,
    outcome: str,
    summary: str,
) -> None:
  """The ONE firing failure report through the common report owner, then the wake.

  The delivered report is the recipient's new durable input: a task-tree
  manager drains it on its next serialized turn (dispatch_pending), a legacy
  cron session is woken through trigger_master — one wake per fire, the same
  shape the legacy path's master wake had. The stable report id dedups the
  delivery, and only a freshly created report wakes, so a recovery re-delivery
  never wakes twice for one firing.
  """
  source = tree.dispatch.report_source_event(leaf_id, "scheduled firing leaf")
  epoch = await tree.sessions.prime_aggregator(recipient)
  async with tree.control_lock:
    report, created = await tree.dispatch.deliver_child_report_locked(
        leaf_id,
        source_event=source,
        outcome=outcome,
        summary=summary,
        result_refs=[firing_ref(task_cfg, firing)],
        recipient=recipient,
    )
  if created:
    await tree.sessions.announce_appended_event(recipient, report, epoch=epoch)
    await tree.dispatch.wake_parent(recipient, report=report)
  log.info(
      "cron_sequence_report_delivered",
      task=task_cfg.name,
      leaf=leaf_id,
      firing=firing,
      outcome=outcome,
      created=created)


async def redrive_firing(leaf_id: str, tree: TaskTreeManager, cfg) -> None:
  """Re-drive one firing's chain or boundary from its leaf's durable facts.

  The single re-drive entry the startup recovery pass and every scheduled_step
  finish chain share: the step Runs' sequence positions decide the frontier, so
  a finished step re-launches the next permitted position or re-delivers the
  ONE boundary report — idempotent by stable Run/close/report ids, and never
  dependent on the next tick or restart.
  """
  records = tree.runs.list_run_records_sync(leaf_id)
  seq = next(
      (r.sequence_ref for r in records if r.sequence_ref is not None and r.sequence_ref.kind == "cron_steps"), None)
  if seq is None:
    return
  parsed = firing_ref_prefix(seq.owner_ref)
  if parsed is None:
    log.warning("cron_firing_ref_unparsable", leaf=leaf_id, owner_ref=seq.owner_ref)
    return
  task_name, firing = parsed
  task_cfg = load_bound_task(task_name, cfg)
  if task_cfg is None:
    log.warning("cron_firing_task_missing", leaf=leaf_id, task=task_name)
    return
  leaf_meta = await tree.load_meta(leaf_id)
  if leaf_meta is None or not leaf_meta.task_parent_id:
    return
  parent_meta = await tree.load_meta(leaf_meta.task_parent_id)
  if parent_meta is None:
    return
  await reconcile_bound_firings(task_cfg, parent_meta, tree, firing, leaf_id)


# ---------------------------------------------------------------------------
# Shared small helpers
# ---------------------------------------------------------------------------


def effective_backend(task_cfg: ScheduledTaskConfig, tree: TaskTreeManager) -> str:
  """The task's effective backend id, resolved strictly against the config."""
  from src.core.scheduler import effective_scheduled_task_backend
  return effective_scheduled_task_backend(task_cfg, tree._cfg)


def resolved_backend_option(
    task_cfg: ScheduledTaskConfig,
    tree: TaskTreeManager,
    backend_id: str | None,
):
  """The configured BackendOption one fire's backend id resolves to, strictly."""
  effective = backend_id or effective_backend(task_cfg, tree)
  option = tree._cfg.get_backend_option(effective)
  if option is None:
    raise ValueError(f"scheduled task '{task_cfg.name}' backend '{effective}' is not configured")
  return option


def resolved_backend_model(
    task_cfg: ScheduledTaskConfig,
    tree: TaskTreeManager,
    backend_id: str | None,
) -> tuple[str, str | None]:
  """(backend, model) for one fire's run: the step's or task's backend, resolved
  strictly to its configured default model (the same resolution the legacy
  scheduled worker spawn rode)."""
  from src.core.backend_models import option_default_model

  option = resolved_backend_option(task_cfg, tree, backend_id)
  return option.id, option_default_model(option, subject="scheduled task backend ")


def distinct_backend_conflict(task_cfg: ScheduledTaskConfig, tree: TaskTreeManager) -> str | None:
  """The first ``distinct_backend_from`` pair whose resolved (type, model) matches, as a
  report sentence; None when every declared pair stays distinct.

  Load time validated only the written ids (an unset effective backend loads);
  this resolves each pair exactly as a launch would and compares the option's
  routing type with its default model — an Antigravity-style backend that picks
  its own model is told apart by type alone.
  """
  from src.core.backend_models import option_default_model

  steps = task_cfg.steps or []
  positions = {step.name: i for i, step in enumerate(steps)}
  for step in steps:
    if step.distinct_backend_from is None:
      continue
    prior = steps[positions[step.distinct_backend_from]]
    option = resolved_backend_option(task_cfg, tree, step.backend or task_cfg.backend)
    prior_option = resolved_backend_option(task_cfg, tree, prior.backend or task_cfg.backend)
    signature = (option.type, option_default_model(option, subject="scheduled task backend "))
    prior_signature = (prior_option.type, option_default_model(prior_option, subject="scheduled task backend "))
    if signature == prior_signature:
      backend_note = (
          f"backend '{option.id}'"
          if option.id == prior_option.id else f"backends '{prior_option.id}' and '{option.id}'")
      return (
          f"steps '{prior.name}' and '{step.name}' declare distinct_backend_from but both resolve to "
          f"{backend_note} (type {signature[0]}, model {signature[1]})")
  return None


def _adapter_of(tree: TaskTreeManager) -> object:
  from src.core.task_execution import TaskExecutionAdapter

  adapter = tree.dispatch.executor
  if not isinstance(adapter, TaskExecutionAdapter):
    raise RuntimeError("task execution adapter is not installed; cannot fire a scheduled task")
  return adapter


def launch(tree: TaskTreeManager, leaf_id: str, run_id: str, prompt: str | None) -> None:
  """The scheduler-owned launch through the shared adapter."""
  _adapter_of(tree).launch(leaf_id, run_id, prompt=prompt)


async def launch_and_settle(
    tree: TaskTreeManager,
    leaf_id: str,
    run_id: str,
    prompt: str | None,
) -> LaunchSettlement:
  """The scheduler-owned launch followed to its settlement (the shared
  launch/wait observation): a durable terminal outcome, or an explicit
  withheld verdict when the launch precondition failed and no process
  started. A live process from a replayed registration is followed, never
  relaunched."""
  return await _adapter_of(tree).launch_and_settle(leaf_id, run_id, prompt=prompt)


async def step_advanced(tree: TaskTreeManager, leaf_id: str, run_id: str) -> bool:
  """Whether one step's durable record clears the chain's advance gate.

  The legacy chain advanced on the process exit code; the v2 record carries
  both signals, and the advance requires both to agree: the durable outcome is
  success AND the recorded exit is 0. A result event without a clean exit (or
  the reverse) stops the chain exactly like a failed step.
  """
  run = await tree.runs.get_run(leaf_id, run_id)
  if run is None:
    return False
  if await tree.runs.terminal_outcome_of(leaf_id, run_id) != "success":
    return False
  return run.exit_code == 0


# ---------------------------------------------------------------------------
# Recovery: re-drive a firing's boundary from durable facts (idempotent)
# ---------------------------------------------------------------------------


async def reconcile_bound_firings(
    task_cfg: ScheduledTaskConfig,
    meta: SessionMetadata,
    tree: TaskTreeManager,
    firing: str,
    leaf_id: str,
) -> None:
  """Complete a firing whose controller died mid-chain (restart recovery).

  Pure over the durable facts: the step Runs' sequence positions decide the
  frontier. A chain whose last executed step succeeded launches the next
  position (its Run id is stable, so a replayed launch binds to the same
  Run); a finished or failed chain re-delivers the ONE boundary report (the
  stable report id and close request id dedup). A leaf already closed does
  nothing.
  """

  def _schedule_recovered_settle(run_id: str, prompt: str) -> None:
    # The settle rides its own logged task, never inline: a live process's
    # follow must not hold this pass. A withheld launch records and reports
    # itself (once, by stable id), and a settled step Run's own finish chain
    # re-drives the frontier, so nothing is owed here after scheduling.
    from src.core.tasks import create_logged_task

    create_logged_task(launch_and_settle(tree, leaf_id, run_id, prompt), name=f"cron-recovered-step-{run_id[:8]}")

  if tree.task_state(leaf_id) != "open":
    return
  steps = task_cfg.steps or []
  if not steps:
    return  # single-prompt firings ride the ordinary work-Run delivery chain
  records = tree.runs.list_run_records_sync(leaf_id)
  events = tree.runs.load_events_sync(leaf_id)
  by_position: dict[int, RunRecord] = {}
  for run in records:
    if run.sequence_ref is not None and run.sequence_ref.kind == "cron_steps":
      by_position[run.sequence_ref.position] = run
  executed_positions = sorted(pos for pos, run in by_position.items() if tree.runs.run_has_terminal_fact(run, events))
  # A registered-but-unlaunched frontier step is the admitted product its
  # controller never got to launch (withheld precondition, or a crash before
  # the launch): the replay launches that SAME step Run — the retained pending
  # request — through the same checks; a live process is followed, never
  # relaunched, and the re-drive stays idempotent by stable ids.
  pending_positions = sorted(
      pos for pos, run in by_position.items() if not tree.runs.run_has_terminal_fact(run, events))
  if pending_positions:
    pos = pending_positions[0]
    queued_run = by_position[pos]
    if queued_run.pid is not None:
      return  # a live recovered process; its own finish chain re-drives
    conflict = distinct_backend_conflict(task_cfg, tree)
    if conflict is not None:
      summary = (
          f"Scheduled task '{task_cfg.name}' stopped before relaunching step '{steps[pos].name}': "
          f"{conflict}.")
      await deliver_boundary_report(tree, leaf_id, meta.id, task_cfg, firing, "failed", summary)
      return
    backend, model = resolved_backend_model(task_cfg, tree, steps[pos].backend or task_cfg.backend)
    run = await register_leaf_run(
        tree, leaf_id, task_cfg, firing, kind="scheduled_step", position=pos, backend=backend, model=model)
    prompt = steps[pos].prompt or ""
    if pos > 0 and executed_positions:
      previous_pos = executed_positions[-1]
      previous = await run_result_text(tree, leaf_id, by_position[previous_pos].id)
      prompt = chain_step_prompt(prompt, steps[previous_pos].name, previous)
    _schedule_recovered_settle(run.id, prompt)
    return
  if not executed_positions:
    return
  last_pos = executed_positions[-1]
  last_outcome = tree.runs.terminal_outcome(events, by_position[last_pos].id)
  if (last_pos < len(steps) - 1 and last_outcome == "success" and
      await step_advanced(tree, leaf_id, by_position[last_pos].id)):
    conflict = distinct_backend_conflict(task_cfg, tree)
    if conflict is not None:
      summary = (
          f"Scheduled task '{task_cfg.name}' stopped before launching step '{steps[last_pos + 1].name}': "
          f"{conflict}.")
      await deliver_boundary_report(tree, leaf_id, meta.id, task_cfg, firing, "failed", summary)
      return
    # The chain is mid-flight: launch the next position through the same
    # controller semantics (idempotent by stable run id).
    backend, model = resolved_backend_model(task_cfg, tree, steps[last_pos + 1].backend or task_cfg.backend)
    next_run = await register_leaf_run(
        tree, leaf_id, task_cfg, firing, kind="scheduled_step", position=last_pos + 1, backend=backend, model=model)
    if await tree.runs.terminal_outcome_of(leaf_id, next_run.id) is None and next_run.pid is None:
      prompt = steps[last_pos + 1].prompt or ""
      if last_pos + 1 > 0:
        previous = await run_result_text(tree, leaf_id, by_position[last_pos].id)
        prompt = chain_step_prompt(prompt, steps[last_pos].name, previous)
      _schedule_recovered_settle(next_run.id, prompt)
    return
  # The chain reached its boundary: re-deliver the ONE report (dedup by id).
  if last_outcome == "success" and last_pos == len(steps) - 1:
    await run_firing_steps_boundary_report(tree, task_cfg, meta, firing, leaf_id)
  else:
    blocks = []
    for pos in executed_positions:
      name = steps[pos].name if pos < len(steps) else f"step {pos}"
      blocks.append(f"**{name} result:**\n{await run_result_text(tree, leaf_id, by_position[pos].id) or '(no result)'}")
    summary = (
        f"Scheduled task '{task_cfg.name}' stopped at step '{steps[last_pos].name}' "
        f"(outcome {last_outcome}); no later step ran.\n\n" + "\n\n".join(blocks))
    await deliver_boundary_report(tree, leaf_id, meta.id, task_cfg, firing, "failed", summary)


async def run_firing_steps_boundary_report(
    tree: TaskTreeManager,
    task_cfg: ScheduledTaskConfig,
    meta: SessionMetadata,
    firing: str,
    leaf_id: str,
) -> None:
  """The successful boundary's close (which delivers the one report)."""
  from src.core.task_sessions import TaskConflictError

  records = tree.runs.list_run_records_sync(leaf_id)
  events = tree.runs.load_events_sync(leaf_id)
  chain = sorted(
      (r for r in records if r.sequence_ref is not None and r.sequence_ref.kind == "cron_steps"),
      key=lambda r: r.sequence_ref.position)  # type: ignore[union-attr]
  if not chain or not all(tree.runs.run_has_terminal_fact(r, events) for r in chain):
    return
  blocks = []
  for run in chain:
    position = run.sequence_ref.position if run.sequence_ref else 0
    steps = task_cfg.steps or []
    name = steps[position].name if position < len(steps) else f"step {position}"
    blocks.append(
        f"**{name} result ({run.backend}):**\n{await run_result_text(tree, leaf_id, run.id) or '(no result)'}")
  summary = f"Scheduled task '{task_cfg.name}' completed all {len(chain)} step(s).\n\n" + "\n\n".join(blocks)

  async def _deliver_blocked_close(e: Exception) -> None:
    # The blocked close keeps the leaf open with its evidence/worktree intact
    # and delivers the SAME stable blocked report the fresh chain delivers —
    # never a log-only boundary. A repaired close later follows the normal
    # completion/report policy (the stable report id dedups).
    log.warning("cron_sequence_close_blocked", task=task_cfg.name, leaf=leaf_id, error=str(e))
    blocked_summary = (
        f"Scheduled task '{task_cfg.name}' completed all {len(chain)} step(s) but the leaf's "
        f"automatic close is blocked ({e}); the task stays open with its evidence.\n\n" + "\n\n".join(blocks))
    await deliver_boundary_report(tree, leaf_id, meta.id, task_cfg, firing, "blocked", blocked_summary)

  try:
    await tree.completion.evaluate_automatic_completion(
        leaf_id,
        run_id=chain[-1].id,
        summary=summary,
        result_refs=[f"{RUN_REF_PREFIX}{r.id}" for r in chain],
        request_id=f"auto:{firing_ref(task_cfg, firing)}",
    )
  except TaskConflictError as e:
    blockers = list(getattr(e, "blockers", None) or [])
    if any("no longer open" in str(b) for b in blockers):
      # A concurrent owner landed this close and delivered the completed
      # report; there is no blocked boundary to deliver.
      return
    await _deliver_blocked_close(e)
  except Exception as e:
    await _deliver_blocked_close(e)

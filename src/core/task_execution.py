"""The task-tree execution adapter: binds v2 Runs to the real backend processes.

This module is the execution-glue owner of the session task tree. It holds the
one :class:`TaskExecutionAdapter` the application installs as the
:class:`~src.core.session_dispatch.TaskInputDispatcher` executor, plus the
common launch/resume interfaces the later controller stages (improve, cron,
delayed triggers) and startup recovery consume.

Ownership boundaries it never crosses:

- Input admission, dedup and batch claims stay in
  :mod:`src.core.session_dispatch`; run records, terminal facts and stop
  requests stay in :mod:`src.core.runs`; task metadata and tree relations stay
  in :mod:`src.core.task_sessions`; completion/evidence policy stays in
  :mod:`src.core.task_completion`. This module only binds a registered Run to
  the existing process harnesses — the master queue (``src.agents.master_cc``)
  for manager turns, the Worker/backend adapters (``src.agents.worker``) for
  work and review — and lands their outcomes through those owners.
- The one short control write lock is held only across durable reservation
  and identity writes. Backend, git, network and model work run outside it.
- A v2 Run is the sole new execution record: no second
  ``SessionMetadata.master_run`` and no second writable ``ThreadMetadata``
  file is ever written for a v2 launch. Legacy v1 sessions keep their
  existing paths untouched.

Serialization evidence: concurrent dispatch calls reserve the consumer under
the control lock — a fresh consumer binds the exact pending batch through
``claim_input_batch``, so the second caller's claim comes back empty and it
never spawns; a queued Run is launched through the in-process launch guard so
two dispatch calls cannot both start it. The spawned process identity
(``pid`` + ``pid_start``) lands on the Run before any call from its credential
is accepted (:meth:`RunStore.record_launch` runs inside the backend's
on_spawn callback, and the caller-identity dependency requires both fields).
"""

from __future__ import annotations

import asyncio
import dataclasses
import functools
import time
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING

from src.agents.worker import QuotaExhaustedException, Worker
from src.core import claude_relay, git, review, runs, spawner_prompt
from src.core import event_types as ET
from src.core.config import CharlieBotConfig, get_credentials
from src.core.control_events import sha256_hex, stable_run_id
from src.core.log_once import LazyStructlogLogger
from src.core.models import (
    SESSION_ID_ENV_VAR,
    BackendOption,
    BackendType,
    RunRecord,
    SessionMetadata,
    TaskType,
)
from src.core.run_token import RUN_TOKEN_ENV, RunTokenClaims, sign_run_token
from src.core.runs import RunNotFoundError, scan_result_exit
from src.core.sessions import SessionManager
from src.core.spawner_backends import resolve_backend_option
from src.core.task_sessions import (
    TaskConflictError,
    TaskInvalidError,
    TaskNotFoundError,
    canonical_task_spec_text,
)
from src.core.verify_trailer import VERIFY_RESULT_TRAILER_EXPECTED

if TYPE_CHECKING:
    from src.core.task_sessions import TaskTreeManager

log = LazyStructlogLogger()


@dataclasses.dataclass
class RunWorkerBinding:
    """The Run-backed worker identity a v2 launch hands to :class:`Worker`.

    Duck-types the ``ThreadMetadata`` fields the Worker reads (id, session_id,
    pid, pid_start, claude_session_id) without ever materializing a legacy
    thread record: the pid/pid_start pair is persisted onto the Run by the
    adapter's ``on_spawned`` callback, not onto a thread file.
    """

    id: str
    session_id: str
    pid: int | None = None
    pid_start: str | None = None
    claude_session_id: str | None = None


def compose_input_prompt(events: list[dict]) -> tuple[str, list[dict]]:
    """The manager-turn prompt body from its exact durable input batch.

    Real user input rides verbatim (its event is the durable fact the Run
    acknowledges — no second synthetic USER copy is persisted); relays,
    child reports and scheduled triggers keep their own typed framing so the
    manager sees the provenance the event carries. Attachments ride the turn.
    """
    parts: list[str] = []
    uploads: list[dict] = []
    for event in events:
        event_type = event.get("type")
        content = str(event.get("content") or "")
        if event_type == ET.USER:
            parts.append(content)
        elif event_type == ET.AGENT_MESSAGE:
            parts.append(
                f"[Message from session {event.get('from_session_name') or event.get('from_session') or 'unknown'}] "
                f"{content}")
        elif event_type == ET.CHILD_REPORT:
            parts.append(
                f"[Report from task {event.get('child_session_id')} | outcome {event.get('outcome')}] {content}")
        elif event_type == ET.SCHEDULED_TRIGGER:
            parts.append(f"[Scheduled trigger] {content}")
        else:
            parts.append(content)
        uploads.extend(event.get("uploaded_files") or [])
    return "\n\n".join(part for part in parts if part), uploads


class TaskExecutionAdapter:
    """Binds registered Runs to the existing master/worker execution harnesses."""

    def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager, tree: "TaskTreeManager") -> None:
        self._cfg = cfg
        self._sessions = session_mgr
        self._tree = tree
        # In-process launch guard: one execute task per run per process. The
        # durable (pid, pid_start) identity write is the cross-restart
        # backstop; this set closes the same-loop double-schedule window.
        self._launch_inflight: set[tuple[str, str]] = set()

    # ------------------------------------------------------------------
    # The dispatcher seam
    # ------------------------------------------------------------------

    async def __call__(
        self,
        session_id: str,
        pending: list[dict],
        *,
        launch_run_id: str | None = None,
    ) -> str | None:
        """Reserve the consumer for one node and schedule its launch.

        Fresh dispatch reserves a stable Run under the control lock and claims
        the exact pending batch; an empty claim means a concurrent dispatch
        won the reservation and this call schedules nothing. A queued Run
        (an explicit retry, or a run a crashed process registered) is launched
        as its own pending execution request. Both paths run the pre-launch
        rechecks before any process starts.
        """
        tree = self._tree
        async with tree.control_lock:
            meta = await tree.load_meta(session_id)
            tree._require_task(meta, session_id)
            assert meta is not None
            if tree.task_state(session_id) != "open" or meta.automation_paused:
                return None
            if launch_run_id is not None:
                run = await tree.runs.get_run(session_id, launch_run_id)
                if run is None:
                    raise TaskNotFoundError(f"run {launch_run_id} not found in task {session_id}")
                events = tree.runs.load_events_sync(session_id)
                if (tree.runs.run_has_terminal_fact(run, events) or run.pid is not None or
                        tree.runs.stop_requested(events, run.id)):
                    return None  # stopped, finished, or already launching: never a second process
                if run.kind == "manager_turn" and not run.input_event_ids and pending:
                    # A queued retry claims the node's pending batch when it
                    # launches — the retried failure released it, and this is
                    # the serialized turn that consumes it.
                    await tree.dispatch.claim_input_batch_locked(session_id, run.id)
                run_id = run.id
            else:
                backend, model = self._reserve_backend_model(meta)
                request_id = "dispatch:" + sha256_hex(
                    "\x00".join(sorted(str(e.get("id")) for e in pending)))
                run_id = stable_run_id(session_id, request_id)
                existing = await tree.runs.get_run(session_id, run_id)
                if existing is None:
                    kind = "manager_turn" if meta.profile == "manager" else "work"
                    record = RunRecord(
                        id=run_id, session_id=session_id, kind=kind,  # type: ignore[arg-type]
                        backend=backend, model=model)
                    # This lock hold already covers the reservation: the locked
                    # registration variant, then the locked batch claim.
                    await tree.runs.register_run_locked(
                        record, task_spec_text=canonical_task_spec_text(meta.task))
                try:
                    bound = await tree.dispatch.claim_input_batch_locked(session_id, run_id)
                except TaskConflictError as e:
                    log.info("dispatch_reservation_contested", session_id=session_id, run_id=run_id, error=str(e))
                    return None
                if not bound:
                    # A concurrent dispatch already reserved this exact batch.
                    log.info("dispatch_reservation_lost", session_id=session_id, run_id=run_id)
                    return None
            key = (session_id, run_id)
            if key in self._launch_inflight:
                return run_id  # the concurrent winner schedules the launch
            self._launch_inflight.add(key)
        # This call owns the launch guard now; schedule directly (launch()
        # would early-return on the guard this reservation just took).
        self._schedule_launch(session_id, run_id)
        return run_id

    def _reserve_backend_model(self, meta: SessionMetadata) -> tuple[str, str | None]:
        """The task's backend+model for a fresh reservation, resolved strictly."""
        from src.core.spawner_backends import _resolve_session_default_backend_model
        return _resolve_session_default_backend_model(self._cfg, meta)

    # ------------------------------------------------------------------
    # Common launch interface
    # ------------------------------------------------------------------

    def launch(self, session_id: str, run_id: str) -> None:
        """Schedule one registered Run's execution (the common launch seam).

        The delegate path, the review chain, and the later controller stages
        (improve, cron, triggers) all enter here; the dispatcher's executor
        seam lands on the same code through :meth:`__call__`.
        """
        key = (session_id, run_id)
        if key in self._launch_inflight:
            return
        self._launch_inflight.add(key)
        self._schedule_launch(session_id, run_id)

    def _schedule_launch(self, session_id: str, run_id: str) -> None:
        """Fire-and-forget the actual execution; the reservation is already durable."""
        from src.core.tasks import create_logged_task

        async def _execute_and_release() -> None:
            try:
                await self.execute_run(session_id, run_id)
            finally:
                self._launch_inflight.discard((session_id, run_id))

        create_logged_task(_execute_and_release(), name=f"task-run-{run_id[:8]}")

    async def execute_run(self, session_id: str, run_id: str) -> None:
        """Pre-launch rechecks, then execute one Run on its kind's adapter.

        Rechecks run under the control lock immediately before the launch:
        role (the kind/profile pairing), open ancestors, pause, authorization
        and any durable stop request. The backend resolution afterwards is
        explicit — a missing or invalid backend fails visibly, never
        silently substituted.
        """
        tree = self._tree
        async with tree.control_lock:
            meta = await tree.load_meta(session_id)
            tree._require_task(meta, session_id)
            assert meta is not None
            run = await tree.runs.get_run(session_id, run_id)
            if run is None:
                raise RunNotFoundError(f"run {run_id} not found in task {session_id}")
            if tree.task_state(session_id) != "open" or meta.automation_paused:
                log.info("run_launch_withheld", session_id=session_id, run_id=run_id)
                return
            await tree._require_open_ancestry(session_id)
            events = tree.runs.load_events_sync(session_id)
            if tree.runs.run_has_terminal_fact(run, events) or tree.runs.stop_requested(events, run_id):
                log.info("run_launch_refused_by_facts", session_id=session_id, run_id=run_id)
                return
            if meta.profile == "worker" and run.kind in ("work", "review") and meta.task_parent_id:
                # The nearest-user-ancestor gate re-judges at actual launch
                # (plan 4.2: pending execution requests re-judge where they start).
                await tree.check_task_authorization(meta.task_parent_id)
        option = self._resolve_run_backend(run)
        if meta.profile == "manager" and run.kind == "manager_turn":
            await self._execute_manager_turn(meta, run, option)
        elif meta.profile == "worker" and run.kind in ("work", "review"):
            await self._execute_worker_run(meta, run, option)
        else:
            raise TaskInvalidError(
                f"run {run_id} (profile={meta.profile}, kind={run.kind}) has no executable adapter")

    def _resolve_run_backend(self, run: RunRecord) -> BackendOption:
        """The Run's explicitly recorded backend/model, resolved strictly."""
        if not run.backend:
            raise ValueError(f"run {run.id} records no backend; refusing to substitute one")
        return resolve_backend_option(self._cfg, run.backend, run.model)

    def _child_env(self, session_id: str, run_id: str, agent_name: str) -> dict[str, str]:
        """The child CLI environment: its own identity, home and address.

        The child's session id and its own signed run token travel explicitly —
        never the parent's inherited identity — and CHARLIEBOT_HOME pins the
        selected home so the child CLI resolves this instance's config,
        credentials and server address.
        """
        key = str(get_credentials().get("charliebot", "access_key") or "")
        if not key:
            raise RuntimeError(
                "run-token signing requires credentials.yaml charliebot.access_key; "
                "agent child environments cannot be built without it")
        token = sign_run_token(
            RunTokenClaims(session_id=session_id, run_id=run_id, agent=agent_name or "worker"), key)
        return {
            SESSION_ID_ENV_VAR: session_id,
            RUN_TOKEN_ENV: token,
            "CHARLIEBOT_HOME": str(self._cfg.charliebot_home),
        }

    # ------------------------------------------------------------------
    # Manager turns
    # ------------------------------------------------------------------

    async def _execute_manager_turn(self, meta: SessionMetadata, run: RunRecord, option: BackendOption) -> None:
        """One manager turn on the existing per-session master queue.

        The turn's input is the exact durable batch the Run claimed; the
        existing queue serializes turns per node, streams events into the
        session chat, and keeps the native continuation anchor on the stable
        session. The Run is the sole new execution record: pid/pid_start land
        at spawn, native_session_id/model/raw log land at finish, and the
        terminal fact goes through ``dispatch.finish_run``.
        """
        from src.agents.master_cc import run_message
        from src.agents.master_cc_state import TaskRunBinding

        session_id, run_id = meta.id, run.id
        transport_dir = self._tree.runs.run_dir(session_id, run_id)
        batch_ids = set(run.input_event_ids)
        batch_events = [e for e in self._tree.fact_history(session_id) if str(e.get("id")) in batch_ids]
        content, uploaded_files = compose_input_prompt(batch_events)
        if not content:
            raise TaskInvalidError(f"run {run_id} claimed no consumable input; nothing to execute")

        async def on_task_spawn(pid: int, pid_start: str | None) -> None:
            if pid_start is None:
                raise RuntimeError(f"run {run_id} spawned without a pinned pid_start")
            await self._tree.runs.record_launch(session_id, run_id, pid=pid, pid_start=pid_start)

        async def on_task_finish(cc_session_id: str | None, exit_code: int, finish_extras: dict) -> None:
            await self._tree.runs.record_observation(
                session_id, run_id,
                native_session_id=cc_session_id,
                model=finish_extras.get("model") or option.model,
                raw_log_ref=str(transport_dir / runs.RAW_LOG_NAME),
                result_ref=str(transport_dir / runs.RAW_LOG_NAME),
            )
            await self._tree.dispatch.finish_run(
                session_id, run_id,
                outcome="success" if exit_code == 0 else "failed",
                exit_code=exit_code,
            )

        log.info("manager_turn_launching", session_id=session_id, run_id=run_id,
                 backend=option.id, inputs=len(run.input_event_ids))
        await run_message(
            self._cfg,
            meta,
            content,
            self._sessions.callbacks(),
            skip_user_event=True,
            auto_trigger=any(e.get("type") == ET.SCHEDULED_TRIGGER for e in batch_events),
            backend_option=option,
            uploaded_files=uploaded_files or None,
            task_run=TaskRunBinding(
                session_id=session_id, run_id=run_id, transport_dir=str(transport_dir)),
            on_task_spawn=on_task_spawn,
            on_task_finish=on_task_finish,
            extra_env=self._child_env(session_id, run_id, meta.name),
        )

    # ------------------------------------------------------------------
    # Worker work and review runs
    # ------------------------------------------------------------------

    async def _execute_worker_run(self, meta: SessionMetadata, run: RunRecord, option: BackendOption) -> None:
        """One work or review Run on the existing Worker/backend adapter."""
        session_id, run_id = meta.id, run.id
        run_dir = self._tree.runs.run_dir(session_id, run_id)
        events_log = run_dir / "events.jsonl"

        task = meta.task
        task_type = task.task_type if task is not None else TaskType.IMPLEMENT
        review_worktree: str | None = None
        if run.kind == "review":
            work_run = await self._tree.runs.get_run(session_id, run.review_of_run_id or "")
            if work_run is None:
                raise TaskInvalidError(f"review run {run_id} names no recorded work Run")
            # The review reuses the work Run's exact repo, branch and worktree.
            review_worktree = work_run.worktree_path
            prompt = await self._build_review_prompt(session_id, run, work_run)
        else:
            prompt = await self._build_work_prompt(meta, run, task_type)

        binding = RunWorkerBinding(id=run_id, session_id=session_id)
        if option.type == BackendType.CC_CLAUDE:
            binding.claude_session_id = str(uuid.uuid4())

        async def on_spawned(spawned: RunWorkerBinding) -> None:
            if spawned.pid is None or spawned.pid_start is None:
                raise RuntimeError(f"run {run_id} spawned without a pinned process identity")
            await self._tree.runs.record_launch(
                session_id, run_id, pid=spawned.pid, pid_start=spawned.pid_start)

        error = ""
        exit_code = -1
        worker: Worker | None = None
        try:
            working_dir = Path(review_worktree) if review_worktree else run_dir
            worker = Worker(
                binding,  # type: ignore[arg-type]
                working_dir,
                events_log,
                prompt,
                self._cfg,
                backend_option=option,
                on_spawned=on_spawned,
                extra_env=self._child_env(session_id, run_id, meta.name),
            )
            # Session-level notices (a pool login that needs re-login) reach
            # the session chat through the successor chain, exactly as the
            # legacy worker path delivers them.
            worker.on_session_event = functools.partial(self._sessions.deliver_to_successor, session_id)
            exit_code = await worker.run()
        except claude_relay.PoolExhaustedError as exc:
            if worker is not None:
                await worker.terminate()
            error = str(exc)
            log.warning("task_run_pool_exhausted", session_id=session_id, run_id=run_id, error=error)
        except QuotaExhaustedException as exc:
            if worker is not None:
                await worker.terminate()
            error = str(exc)
            log.warning("task_run_quota_exhausted", session_id=session_id, run_id=run_id, error=error)
        except Exception as exc:  # setup/transport failure: the run failed loudly
            log.error("task_run_failed", session_id=session_id, run_id=run_id,
                      error=str(exc), exc_info=True)
            if worker is not None:
                await worker.terminate()
            error = str(exc)

        durable_outcome = await self._finalize_worker_run(
            meta, run, option, exit_code=exit_code, error=error)
        await self._after_worker_run(meta, run, durable_outcome)

    async def _finalize_worker_run(
        self,
        meta: SessionMetadata,
        run: RunRecord,
        option: BackendOption,
        *,
        exit_code: int,
        error: str,
    ) -> str:
        """Land one worker Run's observation and terminal fact; returns the durable outcome.

        Successful process exit is only one input: the durable outcome requires
        a successful result event in the run's raw transport log — empty
        output or a missing result keeps the task at attention with the
        evidence retained. The first terminal fact wins: a stop request that
        observed the exit first stands, and this finish reconciles against it.
        """
        session_id, run_id = meta.id, run.id
        run_dir = self._tree.runs.run_dir(session_id, run_id)
        raw_path = run_dir / runs.RAW_LOG_NAME
        native_session_id = await self._native_session_id(raw_path, option)
        outcome = await self._worker_outcome(run_dir, option)
        await self._tree.runs.record_observation(
            session_id, run_id,
            native_session_id=native_session_id,
            model=option.model,
            raw_log_ref=str(raw_path),
            events_ref=str(run_dir / "events.jsonl"),
            result_ref=str(raw_path),
        )
        await self._tree.dispatch.finish_run(
            session_id, run_id, outcome=outcome, exit_code=exit_code if not error else -1)
        durable = self._tree.runs.terminal_outcome(
            self._tree.runs.load_events_sync(session_id), run_id) or outcome
        if error:
            log.warning("task_run_error", session_id=session_id, run_id=run_id, error=error[:500])
        return durable

    async def _worker_outcome(self, run_dir: Path, option: BackendOption) -> str:
        """The terminal-status judgment from the Run's own transport records.

        The raw stream is the primary record; the run's translated events log
        (the same stream's durable projection) carries the equivalent RESULT
        event and serves when a backend wrote no raw file. Successful process
        exit alone never decides: no result event anywhere is a failure with
        the evidence retained.
        """
        raw_path = run_dir / runs.RAW_LOG_NAME
        if raw_path.is_file():
            translate = self._fresh_translate(option)
            _events, result, _code = await asyncio.to_thread(scan_result_exit, raw_path, translate)
            if result is not None and runs.result_success(result):
                return "success"
        events_log = run_dir / "events.jsonl"
        if events_log.is_file():
            _found, success = await asyncio.to_thread(self._events_log_result_success, events_log)
            if success:
                return "success"
        return "failed"

    @staticmethod
    def _events_log_result_success(events_log: Path) -> "tuple[bool, bool]":
        """(found, success) of the last RESULT event in a translated events log."""
        found = False
        success = False
        for event in runs.parse_raw_lines(events_log.read_bytes()):
            if event.get("type") == ET.RESULT:
                found = True
                success = runs.result_success(event)
        return found, success

    async def _native_session_id(self, raw_path: Path, option: BackendOption) -> str | None:
        """The backend's native session id from the run's raw stream, when one exists."""
        if not raw_path.is_file():
            return None
        translate = self._fresh_translate(option)

        def _scan() -> str | None:
            for event in runs.parse_raw_lines(raw_path.read_bytes()):
                for translated in translate(event):
                    if translated.get("type") == ET.SESSION_ATTACHED and translated.get("session_id"):
                        return str(translated["session_id"])
            return None

        return await asyncio.to_thread(_scan)

    def _fresh_translate(self, option: BackendOption) -> "Callable[[dict], list[dict]]":
        """A fresh translate callable for one whole-file scan (stateful translates need one instance)."""
        from src.agents.backends.registry import build_backend
        try:
            return build_backend(option, self._cfg).translate_event
        except Exception as e:
            # Translate-only construction may lack the CLI binary; the scan
            # degrades to the raw claude shape the same way restart recovery's
            # translate fallback does — never a crash of the finalize path.
            log.warning("task_run_translate_unresolved", backend=option.id, error=str(e))
            return lambda event: [event]

    # ------------------------------------------------------------------
    # Prompts (existing sources; the launch text rides the Run as evidence)
    # ------------------------------------------------------------------

    async def _build_work_prompt(self, meta: SessionMetadata, run: RunRecord, task_type: TaskType) -> str:
        """The work Run's prompt from the existing worker prompt sources.

        The task spec body is the description; direct node input (the Run's
        claimed batch) rides after it. The actual launch text is persisted to
        the Run as the prompt-snapshot seam's evidence (the v4 snapshot schema
        itself lands with the context stage).
        """
        from src.core.spawner_prompt import _build_worker_prompt
        task = meta.task
        spec_text = task.goal if (task is not None and task.goal.strip()) else ""
        batch_ids = set(run.input_event_ids)
        batch = [e for e in self._tree.fact_history(meta.id) if str(e.get("id")) in batch_ids]
        content, _uploads = compose_input_prompt(batch) if batch else ("", [])
        description = "\n\n".join(part for part in (spec_text, content) if part)
        if not description.strip():
            raise TaskInvalidError(f"run {run.id} has no task spec and no input; nothing to execute")

        if not (task is not None and task.repo_path):
            # Repo-less work run (verify or a repo-less task): the run dir is
            # the working directory; no worktree, no review artifacts.
            if task_type == TaskType.VERIFY:
                prompt = self._build_verify_prompt(description)
            else:
                prompt = description
            await self._persist_launch_text(meta.id, run.id, prompt)
            return prompt

        assert task.repo_path is not None
        repo_path = Path(task.repo_path).resolve()
        base_branch = run.base_branch or task.base_branch
        branch_name = run.branch_name
        worktree_path = run.worktree_path
        start_point: str | None = None
        if not (base_branch and branch_name and worktree_path):
            base_branch, branch_name, worktree_path, start_point = await self._prepare_worktree(
                meta, run, repo_path)
        assert base_branch and branch_name and worktree_path
        prompt = _build_worker_prompt(
            description,
            repo_path,
            base_branch,
            branch_name,
            worktree_path,
            meta,
            self._cfg,
            task_type=task_type,
            loop_dir=None,
            iteration_number=None,
            is_continuation=run.retry_of_run_id is not None and run.worktree_path is not None,
            keep_worktree=bool(task.keep_worktree),
            start_point=start_point,
        )
        await self._persist_launch_text(meta.id, run.id, prompt)
        return prompt

    def _build_verify_prompt(self, description: str) -> str:
        """The repo-less verify contract from the existing verify prompt source."""
        sections = spawner_prompt._load_prompt_sections(
            self._cfg.charlie_bot_repo / "prompts" / "verify.md",
            spawner_prompt._REQUIRED_VERIFY_PROMPT_SECTIONS,
            extraction="verify-prompt")
        contract = spawner_prompt._substitute_tokens(
            "\n".join(
                sections[section_id].strip("\n")
                for section_id in spawner_prompt._REQUIRED_VERIFY_PROMPT_SECTIONS), {
                    "{{result_trailer_expected}}": VERIFY_RESULT_TRAILER_EXPECTED,
                    "{{canonical_template_path}}": str(
                        (self._cfg.charlie_bot_repo / "prompts" / "plan_template.html").resolve()),
                })
        spawner_prompt._require_tokens_resolved(contract, prompt="verify")
        return f"{contract}\n\n{description}"

    async def _build_review_prompt(self, session_id: str, run: RunRecord, work_run: RunRecord) -> str:
        """The review Run's prompt: the existing review builder over the exact work context."""
        assert work_run.branch_name and work_run.worktree_path and work_run.repo_path
        user_request, worker_summary = await review.extract_review_context(
            session_id, work_run.id, self._cfg.sessions_dir,
            worker_log_path=self._tree.runs.run_dir(session_id, work_run.id) / "events.jsonl")
        prompt = review.build_review_prompt(
            work_run.branch_name,
            work_run.worktree_path,
            work_run.base_branch or "main",
            cfg=self._cfg,
            session_id=session_id,
            original_thread_id=work_run.id,
            sessions_dir=self._cfg.sessions_dir,
            user_request=user_request,
            worker_summary=worker_summary,
            worker_log_path=self._tree.runs.run_dir(session_id, work_run.id) / "events.jsonl",
        )
        await self._persist_launch_text(session_id, run.id, prompt)
        return prompt

    async def _persist_launch_text(self, session_id: str, run_id: str, prompt: str) -> None:
        """Retain the actual launch text on the Run as the snapshot seam's evidence.

        The Run's ``prompt_snapshot_ref`` points at this file; the v4 snapshot
        schema (ordered blocks, sources, delivery modes) is the context
        stage's contract and is deliberately not fabricated here.
        """
        from src.core.json_utils import atomic_write_text
        path = self._tree.runs.run_dir(session_id, run_id) / "launch_prompt.md"
        atomic_write_text(path, prompt)
        await self._tree.runs.record_observation(session_id, run_id, prompt_snapshot_ref=str(path))

    async def _prepare_worktree(
        self, meta: SessionMetadata, run: RunRecord, repo_path: Path
    ) -> tuple[str, str, str, str | None]:
        """Create the Run's isolated worktree from the requested base; records it on the Run.

        An unattended launch starts from the remote's published default branch
        when no base was requested; a requested base is used verbatim. The
        created branch, worktree and canonical base land on the Run record.
        """
        remote_tip: str | None = None
        if run.base_branch:
            base_branch = run.base_branch
        elif meta.task is not None and meta.task.base_branch:
            base_branch = meta.task.base_branch
        else:
            default_branch, remote_tip = await git.git_remote_default_branch_and_tip(repo_path)
            base_branch = f"origin/{default_branch}"
        branch_name = run.branch_name or f"charliebot/task-{int(time.time())}-{run.id[:8]}"
        wt_path = Path(self._cfg.paths.worktree_dir) / git.git_worktree_dir_name(branch_name)
        Path(self._cfg.paths.worktree_dir).mkdir(parents=True, exist_ok=True)
        resolution = await git.git_create_worktree(
            repo_path, base_branch, branch_name, wt_path, remote_tip=remote_tip)
        # The recorded base is the REQUESTED verification target exactly as
        # asked (an origin/ target verifies against the published tip); the
        # worktree's canonical start point stays in the creation resolution.
        await self._tree.runs.record_observation(
            meta.id, run.id,
            repo_path=str(repo_path),
            base_branch=base_branch,
            branch_name=branch_name,
            worktree_path=str(wt_path.resolve()),
        )
        return base_branch, branch_name, str(wt_path.resolve()), resolution.start_point

    # ------------------------------------------------------------------
    # Resume interface (the startup-recovery stage's re-attach entry)
    # ------------------------------------------------------------------

    async def resume_run(self, session_id: str, run_id: str, *, is_alive: "Callable[[], bool] | None" = None) -> None:
        """Re-attach one launched Run and follow it to its terminal fact.

        The comprehensive startup-recovery retirement is the next bounded
        task; this is the common resume interface it consumes. Liveness comes
        from the caller's (pid, pid_start) judgment — when omitted, the run's
        recorded identity is judged against the host. Manager turns re-attach
        through the same per-session queue (the follow drains before any
        queued turn spawns); worker runs re-attach through Worker.resume's
        tail-follow. Both land on the same finalize glue as a fresh run.
        """
        tree = self._tree
        run = await tree.runs.get_run(session_id, run_id)
        if run is None:
            raise RunNotFoundError(f"run {run_id} not found in task {session_id}")
        if run.pid is None or run.pid_start is None:
            raise TaskInvalidError(
                f"run {run_id} records no launched process identity; nothing to re-attach")
        events = tree.runs.load_events_sync(session_id)
        if tree.runs.run_has_terminal_fact(run, events):
            return
        if is_alive is None:
            from src.core.runs import is_run_alive, read_host_boot_time
            alive = is_run_alive(run.pid, run.pid_start, run.started_at, read_host_boot_time())
        else:
            alive = is_alive()
        meta = await tree.load_meta(session_id)
        if meta is None:
            raise TaskNotFoundError(f"task {session_id} not found")
        option = self._resolve_run_backend(run)
        if meta.profile == "manager" and run.kind == "manager_turn":
            await self._resume_manager_turn(meta, run, option, alive)
            return
        if meta.profile == "worker" and run.kind in ("work", "review"):
            await self._resume_worker_run(meta, run, option, alive)
            return
        raise TaskInvalidError(f"run {run_id} (kind={run.kind}) has no resume adapter")

    async def _resume_manager_turn(
        self, meta: SessionMetadata, run: RunRecord, option: BackendOption, alive: bool
    ) -> None:
        """Re-attach a v2 manager turn through the per-session queue's follow path."""
        from src.agents.master_cc import enqueue_master_resume
        from src.agents.master_cc_state import MasterRunRecord, TaskRunBinding

        transport_dir = self._tree.runs.run_dir(meta.id, run.id)
        record = MasterRunRecord(
            pid=run.pid,
            pid_start=run.pid_start,
            started_at=run.started_at,
            raw_log=str(transport_dir / runs.RAW_LOG_NAME),
        )

        async def on_task_spawn(pid: int, pid_start: str | None) -> None:
            raise RuntimeError(
                f"resume follow of run {run.id} must not spawn a process")

        async def on_task_finish(cc_session_id: str | None, exit_code: int, finish_extras: dict) -> None:
            await self._tree.runs.record_observation(
                meta.id, run.id,
                native_session_id=cc_session_id,
                model=finish_extras.get("model") or option.model,
                raw_log_ref=str(transport_dir / runs.RAW_LOG_NAME),
                result_ref=str(transport_dir / runs.RAW_LOG_NAME),
            )
            await self._tree.dispatch.finish_run(
                meta.id, run.id,
                outcome="success" if exit_code == 0 else "failed",
                exit_code=exit_code,
            )

        await enqueue_master_resume(
            self._cfg, meta, record, self._sessions.callbacks(),
            is_alive=lambda: alive,
            task_run=TaskRunBinding(session_id=meta.id, run_id=run.id, transport_dir=str(transport_dir)),
            on_task_spawn=on_task_spawn,
            on_task_finish=on_task_finish,
            extra_env=self._child_env(meta.id, run.id, meta.name),
        )

    async def _resume_worker_run(
        self, meta: SessionMetadata, run: RunRecord, option: BackendOption, alive: bool
    ) -> None:
        """Re-attach a worker Run through Worker.resume's tail-follow."""
        session_id, run_id = meta.id, run.id
        run_dir = self._tree.runs.run_dir(session_id, run_id)
        events_log = run_dir / "events.jsonl"
        binding = RunWorkerBinding(
            id=run_id, session_id=session_id, pid=run.pid, pid_start=run.pid_start,
            claude_session_id=run.native_session_id if option.type == BackendType.CC_CLAUDE else None)
        worker = Worker(
            binding,  # type: ignore[arg-type]
            Path(run.worktree_path) if run.worktree_path else run_dir,
            events_log,
            "",  # the follow builds nothing: the prompt text was the launch's
            self._cfg,
            backend_option=option,
        )
        # Session-level notices reach the session chat exactly as a fresh run's do.
        worker.on_session_event = functools.partial(self._sessions.deliver_to_successor, session_id)
        exit_code = await worker.resume(is_alive=lambda: alive)
        durable_outcome = await self._finalize_worker_run(
            meta, run, option, exit_code=exit_code, error="")
        await self._after_worker_run(meta, run, durable_outcome)

    # ------------------------------------------------------------------
    # Post-finish delivery chain
    # ------------------------------------------------------------------

    async def _after_worker_run(self, meta: SessionMetadata, run: RunRecord, durable_outcome: str) -> None:
        """The delivery chain one worker Run's durable outcome drives.

        A work Run finishing is not delivery: an implement task's review and
        target-branch landing must complete first, and a failed or blocked
        outcome reports the parent with durable stable evidence while the task
        and its worktree stay available for the explicit retry.
        """
        session_id = meta.id
        if run.kind == "review":
            await self._after_review_run(meta, run, durable_outcome)
            return
        if durable_outcome == "success":
            task_type = meta.task.task_type if meta.task is not None else TaskType.IMPLEMENT
            if task_type == TaskType.IMPLEMENT and run.repo_path:
                await self._maybe_spawn_review(session_id, run)
            else:
                await self._cleanup_worktree_if_delivered(session_id, run)
            return
        if durable_outcome == "failed":
            await self._report_failure_to_parent(session_id, run, durable_outcome)

    async def _after_review_run(self, meta: SessionMetadata, run: RunRecord, durable_outcome: str) -> None:
        from src.core.task_completion import (
            LANDING_REF_PREFIX,
            REVIEW_REF_PREFIX,
            RUN_REF_PREFIX,
            SPEC_REF_PREFIX,
            CompletionEvidence,
            LandingEvidence,
        )
        from src.core.task_sessions import TaskConflictError

        session_id = meta.id
        work_run = await self._tree.runs.get_run(session_id, run.review_of_run_id or "")
        if work_run is None:
            raise TaskInvalidError(f"review run {run.id} names no recorded work Run")
        work_run = work_run
        if durable_outcome == "failed":
            retried = await self._maybe_spawn_review(session_id, work_run)
            if retried is None:
                await self._report_failure_to_parent(
                    session_id, work_run, "blocked",
                    summary=f"review of work run {work_run.id} failed on every configured reviewer backend")
            return
        if durable_outcome != "success":
            return
        landing = await self._landing_for_work(work_run)
        if landing is None:
            await self._report_failure_to_parent(
                session_id, work_run, "blocked",
                summary=f"work run {work_run.id} passed review but its branch did not land on "
                        f"{work_run.base_branch or 'the requested base'}")
            return
        branch, commit, repo_path = landing
        refs = [f"{RUN_REF_PREFIX}{work_run.id}"]
        if work_run.task_spec_hash:
            refs.append(f"{SPEC_REF_PREFIX}{work_run.task_spec_hash}")
        refs.append(f"{REVIEW_REF_PREFIX}{run.id}")
        refs.append(f"{LANDING_REF_PREFIX}{branch}@{commit}")
        evidence = CompletionEvidence(
            summary=f"work run {work_run.id} delivered after review {run.id} landed {commit[:12]} on {branch}",
            result_refs=refs,
            run_ids=[work_run.id],
            review_run_ids=[run.id],
            landing=LandingEvidence(branch=branch, commit=commit, repo_path=repo_path),
        )
        try:
            await self._tree.completion.evaluate_automatic_completion(
                session_id, run_id=work_run.id, evidence=evidence)
        except TaskConflictError as e:
            log.warning("task_delivery_close_blocked",
                        session_id=session_id, run_id=work_run.id, blockers=getattr(e, "blockers", None))
        finally:
            await self._cleanup_worktree_if_delivered(session_id, work_run)

    async def _maybe_spawn_review(self, session_id: str, work_run: RunRecord) -> str | None:
        """Spawn the work Run's review on the same task, repo, branch and worktree.

        Idempotent by provenance: an existing non-terminal review of this work
        Run means one is already queued or running (recovery and repeated
        finalize never spawn a second). A failed reviewer retries down the
        existing preference policy with distinct Run records; exhausted
        retries keep the worktree and report blocked.
        """
        tree = self._tree
        async with tree.control_lock:
            events = tree.runs.load_events_sync(session_id)
            if tree.runs.terminal_outcome(events, work_run.id) != "success":
                return None
            # The caller's in-memory record predates the launch's observation
            # writes (worktree facts, native id): the durable record is the
            # review's provenance source.
            fresh = await tree.runs.get_run(session_id, work_run.id)
            if fresh is not None:
                work_run = fresh
            existing = [
                r for r in tree.runs.list_run_records_sync(session_id)
                if r.kind == "review" and r.review_of_run_id == work_run.id]
            if any(not tree.runs.run_has_terminal_fact(r, events) for r in existing):
                return existing[0].id
            attempts = len(existing)
            selection = review.select_reviewer_backend(
                self._cfg, work_run.backend or "", work_run.model,
                [r.backend for r in existing if r.backend])
            if selection is None:
                log.warning("reviewer_backends_exhausted", session_id=session_id, work_run=work_run.id)
                return None
            resolved_backend, resolved_model, _tried = selection
            run_id = stable_run_id(session_id, f"review:{work_run.id}:{attempts + 1}")
            record = RunRecord(
                id=run_id,
                session_id=session_id,
                kind="review",
                review_of_run_id=work_run.id,
                backend=resolved_backend,
                model=resolved_model,
                repo_path=work_run.repo_path,
                base_branch=work_run.base_branch,
                branch_name=work_run.branch_name,
                worktree_path=work_run.worktree_path,
            )
            await tree.runs.register_run_locked(
                record, task_spec_text=f"review of work run {work_run.id}")
        self.launch(session_id, run_id)
        return run_id

    async def _landing_for_work(self, work_run: RunRecord) -> "tuple[str, str, str] | None":
        """The (branch, commit, repo) landing evidence of a reviewed work Run, or None.

        The commit is the work branch's tip in its repository; the check
        requires that commit to exist and be an ancestor of the requested
        target branch there (an origin/ target is fetched first).
        """
        if not (work_run.repo_path and work_run.branch_name and work_run.base_branch):
            return None
        commit = await git.git_rev_parse(Path(work_run.repo_path), work_run.branch_name)
        if commit is None:
            return None
        from src.core.git import git_verify_commit_landed
        landed, reason = await git_verify_commit_landed(
            Path(work_run.repo_path), work_run.base_branch, commit)
        if not landed:
            return None
        return work_run.base_branch, commit, work_run.repo_path

    async def _cleanup_worktree_if_delivered(self, session_id: str, work_run: RunRecord) -> None:
        """Remove the shared worktree only once the task actually delivered.

        Failed, blocked and unproven outcomes keep the worktree; keep_worktree
        pins it; a task that did not close keeps it for the explicit retry.
        """
        meta = await self._tree.load_meta(session_id)
        if meta is None or meta.task is None or meta.task.keep_worktree:
            return
        if self._tree.task_state(session_id) != "completed":
            return
        await git.git_worktree_remove_reporting(
            work_run.repo_path, work_run.worktree_path, work_run.branch_name, work_run.id,
            Path(self._cfg.paths.worktree_dir),
            log_fields={"run_id": work_run.id, "session": session_id},
            label="Task worktree",
            fail_event="task_worktree_cleanup_failed",
            remove_failed_event="task_worktree_remove_failed",
        )

    async def _report_failure_to_parent(
        self, session_id: str, run: RunRecord, outcome: str, *, summary: str | None = None
    ) -> None:
        """Persist one failed/blocked child_report with stable source/recipient evidence.

        The source event is the Run's durable run_finished fact and the
        recipient is the close-time fixed parent, so the stable report id
        dedups across recovery and repeated finalize without ever relying on
        the legacy master_woke_after_summary judgment.
        """
        meta = await self._tree.load_meta(session_id)
        if meta is None or not meta.task_parent_id:
            return
        events = self._tree.runs.load_events_sync(session_id)
        source = next(
            (e for e in reversed(events)
             if e.get("type") == ET.RUN_FINISHED and e.get("run_id") == run.id), None)
        if source is None:
            return
        if summary is None:
            summary = await self._worker_failure_summary(session_id, run)
        await self._tree.dispatch.deliver_child_report(
            session_id,
            source_event=source,
            outcome=outcome,
            summary=summary,
            result_refs=[f"run:{run.id}"],
            recipient=meta.task_parent_id,
        )


    async def _worker_failure_summary(self, session_id: str, run: RunRecord) -> str:
        """The failed run's own closing words (or the bare outcome) for the parent report."""
        events_log = self._tree.runs.run_dir(session_id, run.id) / "events.jsonl"
        if events_log.is_file():
            text = await asyncio.to_thread(review._worker_summary_from_events_log, events_log)
            if text:
                return text
        return f"run {run.id} failed without a reportable output"

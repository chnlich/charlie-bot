"""The Claude CLI lifecycles: how a run of a Claude CLI backend places, watches and relays its processes.

Two lifecycles serve the Claude CLI family:

- ``ClaudeCliLifecycle`` serves a CLI variant without a login pool (cc-kimi, cc-openai-compatible,
  and a cc-claude entry when ``accounts.claude`` is empty). It runs one process per run. For a turn
  it checks that the held conversation has a transcript on disk before it resumes, asks the CLI to
  keep per-machine sections out of the system prompt, and reports a reply that a model outside the
  pinned family wrote.
- ``ClaudeCodeLifecycle`` serves cc-claude on the account pool (src/backends/claude_code/claude_accounts.py).
  It also picks the login, watches the events for a quota signal, and relays the run to the next
  login (src/backends/claude_code/master_cc_relay.py, src/backends/claude_code/claude_relay.py).

Both build ``ClaudeLaunch``: a ``Launch`` that also carries the relay count and the watch of its process.
"""

from __future__ import annotations

import asyncio
import dataclasses
from pathlib import Path
from typing import Any

from src.backends.claude_code import claude_accounts, claude_code, claude_metadata, claude_relay, master_cc_relay
from src.backends.claude_code.claude_config import ClaudeAccount
from src.backends.claude_code.login_dirs import claude_config_dir
from src.infra import config, log_once, models
from src.infra import event_types as ET
from src.runtime.hooks import backend_lifecycle

log = log_once.LazyStructlogLogger()

# Moves per-machine sections (cwd, env info, memory paths, git status) out of the system prompt into
# the first user message, so the system prompt stays stable across sessions and cross-run
# prompt-cache reuse improves. Only the Claude Code CLI family supports this flag.
EXCLUDE_DYNAMIC_SECTIONS_FLAG = "--exclude-dynamic-system-prompt-sections"


@dataclasses.dataclass(frozen=True)
class ClaudeLaunch(backend_lifecycle.Launch):
  """A ``Launch`` of a Claude CLI process.

  ``account`` is the pool login of the process and ``relay_watch`` watches it; both are None
  outside the pool. ``relays`` is the number of relays that led to this process.
  """
  account: ClaudeAccount | None = None
  relay_watch: claude_relay.RelayWatch | None = None
  relays: int = 0


def cc_transcript_exists(config_dir: Path, cc_session_id: str) -> bool:
  """True when *config_dir* holds a resumable transcript for *cc_session_id*."""
  return bool(claude_accounts.transcript_matches(config_dir, cc_session_id))


def _launch_args(ctx: backend_lifecycle.LaunchContext, *, first_process: bool) -> dict[str, Any]:
  """The factory arguments of one process: every process of a turn carries the dynamic-sections flag, and the
  first process of a task opens the conversation id the runtime chose (a later one resumes it)."""
  if ctx.kind == "turn":
    return {"extra_flags": [EXCLUDE_DYNAMIC_SECTIONS_FLAG]}
  if ctx.kind == "task":
    if first_process and ctx.preassigned_native_id:
      return {"claude_session_id": ctx.preassigned_native_id}
    return {}
  raise ValueError(f"unknown launch kind {ctx.kind!r}")


class ClaudeCliLifecycle(backend_lifecycle.BackendLifecycle):
  """A Claude CLI variant without a login pool: one process per run."""

  async def place(self, ctx: backend_lifecycle.LaunchContext) -> backend_lifecycle.Launch:
    """Plan the one process of the run.

    turn: resume the held conversation when the default login directory holds its transcript, and
    start fresh with a warning when it does not. The process also gets the dynamic-sections flag.
    task: start a fresh conversation; no flag is added.
    """
    return self._launch(ctx, resume_id=self._transcript_checked(ctx))

  def _transcript_checked(self, ctx: backend_lifecycle.LaunchContext) -> str | None:
    """The held conversation id when its transcript is on disk, else None."""
    cc_session_id = ctx.held_native_id
    if not cc_session_id:
      return None
    config_dir = claude_config_dir()
    if cc_transcript_exists(config_dir, cc_session_id):
      return cc_session_id
    log.warning(
        "master_cc_resume_transcript_missing",
        session=ctx.session_meta.id,
        cc_session_id=cc_session_id,
        config_dir=str(config_dir),
    )
    return None

  def _launch(self, ctx: backend_lifecycle.LaunchContext, *, resume_id: str | None) -> ClaudeLaunch:
    return ClaudeLaunch(
        backend_kwargs=_launch_args(ctx, first_process=True),
        resume_id=resume_id,
        prompt=None,
        account_label=None,
        account=None,
        relay_watch=None,
        relays=0)

  def round_notices(self, option: models.BackendOption, events: list[dict]) -> list[dict]:
    """The notice for a round whose visible reply a model outside the pinned family wrote."""
    served_models = claude_code.out_of_family_served_models(events, option.model)
    if not served_models:
      return []
    return [
        {
            "type": ET.MODEL_FALLBACK_NOTICE,
            "backend": option.id,
            "configured_model": option.model,
            "served_models": served_models,
        }
    ]


class ClaudeCodeLifecycle(ClaudeCliLifecycle):
  """cc-claude: on the account pool, a run picks a login and relays to the next login on quota."""

  account_source = "claude_account"
  account_subject = "Claude account"

  def continuation_domain(self, option: models.BackendOption, cfg: config.CharlieBotConfig) -> str:
    """The account pool when one is configured, else the default login directory."""
    return claude_accounts.continuation_domain(option, cfg)

  async def place(self, ctx: backend_lifecycle.LaunchContext) -> backend_lifecycle.Launch:
    """Plan the first process of the run; without a pool this is the ``ClaudeCliLifecycle`` plan.

    turn: look for the held transcript in the session's own login and then in every pool login,
    pick the login (the session's own while its prompt cache is warm), move the transcript to it,
    and compact a large Fable context when the cache is cold. A pool without a login raises
    ``LaunchRefused`` with ``quota_exhausted`` True.
    task: pick the pool login with the most headroom; a pool without a login raises
    ``LaunchRefused`` with ``quota_exhausted`` True.
    """
    if not claude_accounts.is_pooled(ctx.option, ctx.cfg):
      return await super().place(ctx)
    if ctx.kind == "turn":
      return await self._place_turn(ctx)
    if ctx.kind == "task":
      return self._place_task(ctx)
    raise ValueError(f"unknown launch kind {ctx.kind!r}")

  async def _place_turn(self, ctx: backend_lifecycle.LaunchContext) -> backend_lifecycle.Launch:
    resume_id = self._pooled_resume_id(ctx)
    context_tokens, last_request_at = await ctx.context_state()
    account = await master_cc_relay.place_turn(ctx, resume_id, context_tokens, last_request_at)
    # A moved transcript is re-resolved under the chosen account. A fresh turn placed with no
    # held id has no transcript to move, and re-resolving would hand the old id to a backend
    # outside its continuation domain.
    if ctx.held_native_id is not None:
      resume_id = self._pooled_resume_id(ctx)
    return self._pooled_launch(ctx, account, resume_id=resume_id, prompt=None, relays=0)

  def _place_task(self, ctx: backend_lifecycle.LaunchContext) -> backend_lifecycle.Launch:
    # The login with the most headroom, so the relay loop can move the run when that login is
    # rejected mid-run.
    account_pool = claude_accounts.option_pool(ctx.option)
    account = claude_accounts.select(ctx.cfg, ctx.option.model, account_pool=account_pool)
    if account is None:
      raise backend_lifecycle.LaunchRefused(
          claude_relay.pool_exhausted_message(ctx.cfg, account_pool=account_pool), quota_exhausted=True)
    return self._pooled_launch(ctx, account, resume_id=ctx.held_native_id, prompt=None, relays=0)

  def _pooled_resume_id(self, ctx: backend_lifecycle.LaunchContext) -> str | None:
    """The held conversation id when a pool login holds its transcript, else None.

    Each pool account has its own login directory and cannot see another's conversations, so
    resuming an id recorded under a different account always fails. The lookup tries the
    session's own account first and then every pool login, and writes a hit elsewhere back onto
    the session's account label; that is how sessions created before the pool migrate
    without a metadata rewrite.
    """
    cc_session_id = ctx.held_native_id
    if not cc_session_id:
      return None
    session_meta = ctx.session_meta
    current = claude_accounts.account_by_label(ctx.cfg, claude_metadata.account_of(session_meta))
    if current is not None and cc_transcript_exists(Path(current.config_dir), cc_session_id):
      return cc_session_id
    found = claude_accounts.find_transcript_account(ctx.cfg, cc_session_id)
    if found is not None:
      log.info(
          "master_cc_resume_transcript_found_in_pool",
          session=session_meta.id,
          cc_session_id=cc_session_id,
          account=found.label,
          previous_account=claude_metadata.account_of(session_meta),
      )
      claude_metadata.set_account(session_meta, found.label)
      return cc_session_id
    log.warning(
        "master_cc_resume_transcript_missing",
        session=session_meta.id,
        cc_session_id=cc_session_id,
        config_dir=current.config_dir if current is not None else None,
        pool=[account.label for account in claude_accounts.pool(ctx.cfg)],
    )
    return None

  def _pooled_launch(
      self,
      ctx: backend_lifecycle.LaunchContext,
      account: ClaudeAccount,
      *,
      resume_id: str | None,
      prompt: str | None,
      relays: int,
  ) -> ClaudeLaunch:
    return ClaudeLaunch(
        backend_kwargs={
            "claude_account": account,
            **_launch_args(ctx, first_process=relays == 0)
        },
        resume_id=resume_id,
        prompt=prompt,
        account_label=account.label,
        account=account,
        relay_watch=claude_relay.RelayWatch(account.label, ctx.option.model),
        relays=relays)

  def watch(
      self, ctx: backend_lifecycle.LaunchContext,
      launch: backend_lifecycle.Launch) -> backend_lifecycle.LaunchWatch | None:
    """The relay watch of a pooled process; None outside the pool."""
    assert isinstance(launch, ClaudeLaunch)
    return launch.relay_watch

  async def next_launch(
      self,
      ctx: backend_lifecycle.LaunchContext,
      launch: backend_lifecycle.Launch,
      native_id: str | None,
  ) -> backend_lifecycle.Launch:
    """Plan the process that continues the run on the next pool login.

    A login failure first marks the login unhealthy and tells the operator. The relay limit
    raises ``LaunchRefused`` with ``quota_exhausted`` False. The transcript moves to the login with
    the most headroom, a large Fable context is compacted there, and the next process resumes
    the same session with the continuation prompt. A pool without a login raises ``LaunchRefused``
    with ``quota_exhausted`` True; a run without a session id and a failed copy raise it with
    ``quota_exhausted`` False.
    turn: a copy that the newer-transcript guard refused is not an error: the destination holds
    the newer transcript, so the turn adopts that login and continues from it. The new login is
    persisted on the session.
    task: a refused copy ends the run.
    """
    assert isinstance(launch, ClaudeLaunch)
    assert launch.account is not None and launch.relay_watch is not None
    current = launch.account
    decision = launch.relay_watch.decided
    if decision == claude_relay.LOGIN_FAILED:
      await master_cc_relay.report_login_failure(ctx, current)
    if launch.relays >= claude_relay.MAX_RELAYS_PER_TURN:
      raise backend_lifecycle.LaunchRefused(claude_relay.relay_limit_message(), quota_exhausted=False)
    relays = launch.relays + 1
    next_account = await master_cc_relay.prepare_relay(ctx, current, native_id, decision, relays)
    if ctx.kind == "turn":
      # The relay's label persist point: disk carries the new account from the moment the
      # continuation is built, not at round end (an unchanged account skips the write inside
      # the funnel).
      claude_metadata.set_account(ctx.session_meta, next_account.label)
      await ctx.record_account(next_account.label)
    return self._pooled_launch(
        ctx, next_account, resume_id=native_id, prompt=claude_relay.CONTINUATION_PROMPT, relays=relays)

  async def after_round(self, ctx: backend_lifecycle.LaunchContext, native_id: str | None, succeeded: bool) -> None:
    """Collapse the pool's copies of the transcript to the newest two after a sound turn.

    Every relay leaves its source copy behind. A failed round keeps every copy as its fallback.
    turn: runs after the round's account label is persisted.
    task: does nothing.
    """
    if ctx.kind == "turn" and succeeded and native_id and claude_metadata.account_of(ctx.session_meta):
      await asyncio.to_thread(claude_accounts.retire_transcript_copies, ctx.cfg, native_id)

  def account_label(self, meta: models.SessionMetadata) -> str | None:
    return claude_metadata.account_of(meta)

  def record_account_label(self, meta: models.SessionMetadata, label: str | None) -> bool:
    if claude_metadata.account_of(meta) == label:
      return False
    claude_metadata.set_account(meta, label)
    return True

  def thread_native_id(self, thread: models.ThreadMetadata) -> str | None:
    return claude_metadata.session_id_of(thread)

  def assign_thread_native_id(self, thread: models.ThreadMetadata, native_id: str | None) -> None:
    claude_metadata.set_session_id(thread, native_id)

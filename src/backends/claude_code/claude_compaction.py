"""Sonnet compaction of a Claude Code transcript at the moments its prompt cache is already cold.

A Fable session's prompt cache lives one hour and belongs to one organization
(each pool account is its own organization), and Claude Code compacts with the
model that invoked it. The two moments a Fable session has to re-read its whole
context anyway, an account relay (a new organization) and a user message that
arrives more than an hour after the previous request (the cache has expired),
are therefore the moments a compaction adds no cache cost, and the compaction is
handed to Sonnet: it reads the transcript at a fifth of Fable's price and its
tokens land in the shared total bucket instead of Fable's own weekly bucket. A
warm cache, a context below the configured floor, or a non-Fable session runs
without compaction; nothing is compacted speculatively at the end of a turn,
where the cache is still warm.

``run_compaction`` runs a Claude Code command line with ``/compact`` on stdin
inside the account's login directory and judges success from two facts: the
transcript gained a ``compact_boundary`` row, and the models that took part in
the run — those whose token counters grew over the transcript's newest
``cost-state`` row — are the expected model family alone. ``compact_with_sonnet``
runs the Sonnet command line, ``claude -p --resume <uuid> --model claude-sonnet-5``.
The idle compaction (src/backends/claude_code/claude_idle_compaction.py) runs its
session's own command line through the same runner. Growth over that row is the fact that counts, not presence in ``modelUsage``: ``--resume`` restores
the session's per-model totals from the transcript's ``cost-state`` rows, so
the run's ``modelUsage`` always repeats the session's earlier turns' usage next
to the run's own. Success and failure surface through the same
``context_compacted`` / ``context_compact_failed`` chat events the Claude Code
auto compaction path emits, with ``model`` naming the compacting model. A
failed run leaves the caller continuing without compaction; one rejected after
its boundary row landed (the model check) leaves the compacted transcript in
place.
"""

from __future__ import annotations

import asyncio
import dataclasses
import datetime
import json
import os
import pathlib
import re
from collections.abc import Awaitable, Callable

from src.backends.claude_code import claude_accounts, claude_code, claude_launch
from src.infra import config, log_once, models, process
from src.infra import event_types as ET
from src.runtime import model_family, resource_class
from src.runtime.agent_process import base

log = log_once.LazyStructlogLogger()

# Claude Code writes one-hour prompt-cache entries only, and every hit renews the
# entry, so a request more than an hour after the previous one finds nothing cached.
CACHE_TTL = datetime.timedelta(minutes=60)

# Whole Claude Code /compact run. A 70K-token compaction measured 21 s; the
# ceiling leaves room for a 400K one.
CLAUDE_COMPACTION_TIMEOUT = 900.0  # seconds

COMPACTION_MODEL = "claude-sonnet-5"
COMPACTION_FAMILY = "sonnet"
# The family whose own weekly bucket the compaction spares.
FABLE_FAMILY = "fable"

# /compact needs no tool; every tool Claude Code could reach is disallowed so the
# run can only read the transcript and write the summary.
COMPACTION_DISALLOWED_TOOLS = ",".join(
    (
        claude_code.HEADLESS_DISALLOWED_TOOLS,
        "Bash,Read,Write,Edit,MultiEdit,NotebookEdit,Glob,Grep,WebSearch,WebFetch,TodoWrite,Skill,ToolSearch",
        "KillShell,BashOutput,AskUserQuestion,ExitPlanMode,Task",
    ))

COMPACT_PROMPT = "/compact\n"

# The JSON-quoted subtype the raw transcript lines carry; built from the
# constant so the byte needle stays in sync with the persisted wire value.
_BOUNDARY_MARKER = f'"{ET.COMPACT_BOUNDARY}"'

# The JSON-quoted type the raw cost-state lines carry; the transcript writes it
# with no shared constant, so the literal is the needle.
_COST_STATE_MARKER = '"cost-state"'

# ---------------------------------------------------------------------------
# Trigger decisions (pure)
# ---------------------------------------------------------------------------


def is_fable(model: str | None) -> bool:
  return model_family.model_family(model) == FABLE_FAMILY


def cache_expired(last_request_at: datetime.datetime | None, now: datetime.datetime | None) -> bool:
  """True when the previous request is more than CACHE_TTL old; None (no request yet) is not expired."""
  return last_request_at is not None and claude_accounts.now_or(now) - last_request_at > CACHE_TTL


def expired_cache_compaction_wanted(
    cfg: config.CharlieBotConfig,
    model: str | None,
    context_tokens: int | None,
    last_request_at: datetime.datetime | None,
    now: datetime.datetime | None,
) -> bool:
  """A Fable turn starting on an expired cache with a context at or above the floor."""
  return (
      is_fable(model) and context_tokens is not None and
      context_tokens >= cfg.accounts.claude_compaction.expired_cache_tokens and cache_expired(last_request_at, now))


def relay_compaction_wanted(cfg: config.CharlieBotConfig, model: str | None, context_tokens: int | None) -> bool:
  """A Fable session about to relay to another account with a context at or above the floor."""
  return is_fable(
      model) and context_tokens is not None and context_tokens >= cfg.accounts.claude_compaction.relay_tokens


# ---------------------------------------------------------------------------
# Transcript facts
# ---------------------------------------------------------------------------


def _boundary_rows(transcript: pathlib.Path) -> list[dict]:
  rows: list[dict] = []
  with transcript.open(encoding="utf-8", errors="replace") as handle:
    for line in handle:
      if _BOUNDARY_MARKER not in line:
        continue
      try:
        row = json.loads(line)
      except ValueError:
        continue
      if isinstance(row, dict) and row.get("subtype") == ET.COMPACT_BOUNDARY:
        rows.append(row)
  return rows


def count_compact_boundaries(transcript: pathlib.Path) -> int:
  """Number of ``compact_boundary`` rows in an on-disk transcript."""
  return len(_boundary_rows(transcript))


def _newest_cost_state_model_usage(transcript: pathlib.Path) -> dict:
  """The newest ``cost-state`` row's ``modelUsage``, the session's running
  per-model totals; {} when the transcript carries no such row."""
  newest: dict = {}
  with transcript.open(encoding="utf-8", errors="replace") as handle:
    for line in handle:
      if _COST_STATE_MARKER not in line:
        continue
      try:
        row = json.loads(line)
      except ValueError:
        continue
      if isinstance(row, dict) and row.get("type") == "cost-state":
        usage = row.get("modelUsage")
        newest = usage if isinstance(usage, dict) else {}
  return newest


# A camelCase/lowercase-or-digit boundary, i.e. every place a snake_case name
# would carry an underscore: the transcript's camelCase keys convert by this one
# rule, so a key the upstream adds later converts without a change here.
_CAMEL_TO_SNAKE = re.compile(r"(?<=[a-z0-9])(?=[A-Z])")


def _snake_case_keys(meta: dict) -> dict:
  """Rename every key camelCase to snake_case, the form the streaming payload uses."""
  return {_CAMEL_TO_SNAKE.sub("_", key).lower(): value for key, value in meta.items()}


def _newest_boundary_compact_metadata(transcript: pathlib.Path) -> dict | None:
  """The newest boundary row's ``compactMetadata`` with snake_case keys; None when
  the transcript has no boundary row carrying one."""
  rows = _boundary_rows(transcript)
  if not rows:
    return None
  meta = rows[-1].get("compactMetadata")
  return _snake_case_keys(meta) if isinstance(meta, dict) else None


# ---------------------------------------------------------------------------
# The compaction run
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class CompactionOutcome:
  ok: bool
  error: str | None = None
  models: tuple[str, ...] = ()
  # The post-compaction size the new boundary row carries; None when the row carries none.
  post_tokens: int | None = None
  # Cache-read tokens the run added over the transcript's restored totals, summed over ``models``.
  cache_read_tokens: int | None = None


def compaction_command(cc_session_id: str, session_id: str) -> list[str]:
  return [
      claude_code.BASE_COMMAND[0],
      "-p",
      "--resume",
      cc_session_id,
      "--model",
      COMPACTION_MODEL,
      "--output-format",
      "json",
      base.DISALLOWED_TOOLS_FLAG,
      COMPACTION_DISALLOWED_TOOLS,
      # The compaction subprocess is a CharlieBot-spawned Claude session too,
      # so connector sync stays off here as well (claude_launch.DISABLE_CONNECTOR_SETTINGS).
      "--settings",
      json.dumps(
          {
              **claude_launch.DISABLE_CONNECTOR_SETTINGS,
              **claude_launch.compaction_context_hook_settings(session_id),
          },
          separators=(",", ":")),
  ]


def compaction_env(config_dir: str | pathlib.Path, run_kind: models.RunKind) -> dict[str, str]:
  """Headless Claude Code environment of a *run_kind* run, pinned to one login directory.

  The host's own CLAUDE_CONFIG_DIR never reaches the child: the pool chose the
  directory and an inherited value would silently pick another account.
  """
  env = {**claude_code.claude_supervisor_env(os.environ), **claude_launch.headless_claude_env(run_kind)}
  env.pop(config.CLAUDE_CONFIG_DIR_ENV_VAR, None)
  env[config.CLAUDE_CONFIG_DIR_ENV_VAR] = str(pathlib.Path(config_dir).expanduser())
  return env


# The token counters a run's modelUsage carries; growth in any one past the
# baseline says the model served this run.
_USAGE_COUNTERS = ("inputTokens", "outputTokens", "cacheReadInputTokens", "cacheCreationInputTokens")


def _models_that_grew(result: dict, baseline: dict) -> tuple[str, ...]:
  """The models whose token counters grew over the baseline's restored totals;
  the run's ``modelUsage`` covers the whole session, so presence alone names
  the earlier turns' models too."""
  grew = []
  for name, counters in (result.get("modelUsage") or {}).items():
    restored = baseline.get(name) or {}
    if any(counters.get(counter, 0) > restored.get(counter, 0) for counter in _USAGE_COUNTERS):
      grew.append(str(name))
  return tuple(grew)


def _result_object(stdout: bytes) -> dict | None:
  """The run's result object: the JSON on the last non-empty stdout line.

  ``json`` output is that one line; ``stream-json`` output ends with it.
  """
  lines = [line for line in stdout.decode("utf-8", errors="replace").splitlines() if line.strip()]
  if not lines:
    return None
  try:
    result = json.loads(lines[-1])
  except ValueError:
    return None
  return result if isinstance(result, dict) else None


def _cache_read_growth(result: dict, baseline: dict, names: tuple[str, ...]) -> int:
  """Cache-read tokens the run added over the restored totals, summed over the models in *names*."""
  usage = result.get("modelUsage") or {}
  return sum(
      usage[name].get("cacheReadInputTokens", 0) - (baseline.get(name) or {}).get("cacheReadInputTokens", 0)
      for name in names)


def _judge(returncode: int, stdout: bytes, baseline: dict, before: int, after: int, family: str) -> CompactionOutcome:
  result = _result_object(stdout)
  if result is None:
    return CompactionOutcome(ok=False, error=f"exit {returncode}, no JSON result on stdout")
  models = _models_that_grew(result, baseline)
  if returncode != 0 or result.get("is_error"):
    detail = str(result.get("result") or "").strip()[:200]
    return CompactionOutcome(ok=False, error=f"exit {returncode}: {detail or 'run reported an error'}", models=models)
  if after <= before:
    return CompactionOutcome(ok=False, error="transcript gained no compact_boundary row", models=models)
  if not models or any(model_family.model_family(name) != family for name in models):
    return CompactionOutcome(ok=False, error=f"compaction served by {', '.join(models) or 'no model'}", models=models)
  return CompactionOutcome(ok=True, models=models, cache_read_tokens=_cache_read_growth(result, baseline, models))


async def run_compaction(
    *,
    argv: list[str],
    env: dict[str, str],
    cwd: str,
    config_dir: str | pathlib.Path,
    cc_session_id: str,
    family: str,
    model: str,
    trigger: str,
    failure_trigger: str | None,
    pre_tokens: int | None,
    persist_and_broadcast: Callable[[dict], Awaitable[None]],
    log_context: dict,
    timeout: float = CLAUDE_COMPACTION_TIMEOUT,
    cgroup_session_id: str | None = None,
) -> CompactionOutcome:
  """Run ``/compact`` on *cc_session_id*'s transcript with the command line *argv*; judge the run.

  *family* is the model family that must have served the run, and *model* names the
  compacting model on the chat event. A successful run emits one ``context_compacted``
  with *trigger*; a failed run emits one ``context_compact_failed`` with *failure_trigger*,
  and None there omits the key. Never raises for a failed run. A run rejected after its
  boundary row landed (the model check) leaves the compacted transcript in place.

  *cgroup_session_id* is the owning CharlieBot session, so the compaction process lands in
  that session's cgroup; None (no session home) spawns without one.
  """
  transcript = claude_accounts.transcript_path(config_dir, cc_session_id)
  if transcript is None:
    return await _fail(
        persist_and_broadcast,
        log_context,
        f"no transcript for {cc_session_id} under {config_dir}",
        model=model,
        trigger=failure_trigger)
  before = count_compact_boundaries(transcript)
  # The run's modelUsage covers the whole session (--resume restores the newest
  # cost-state row), so the judge compares against these pre-run totals.
  baseline = _newest_cost_state_model_usage(transcript)
  log.info("claude_compaction_starting", cc_session_id=cc_session_id, pre_tokens=pre_tokens, **log_context)
  cfg = config.get_config()
  session_cgroup = None
  if cgroup_session_id is not None and cfg.server.session_memory_max_mb > 0:
    spawn_class = await resource_class.resolve(cgroup_session_id)
    session_cgroup = process.prepare_session_cgroup(
        cgroup_session_id,
        spawn_class,
        memory_max_mb=cfg.server.session_memory_max_mb,
        swap_max_mb=cfg.server.session_swap_max_mb,
        tasks_max=cfg.server.session_tasks_max,
    )
  try:
    proc = await asyncio.create_subprocess_exec(
        *argv,
        cwd=cwd,
        env=env,
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        start_new_session=True,
        preexec_fn=process.make_session_cgroup_preexec(session_cgroup),
    )
  except OSError as exc:
    return await _fail(
        persist_and_broadcast, log_context, f"could not start {argv[0]}: {exc}", model=model, trigger=failure_trigger)
  try:
    stdout, stderr = await asyncio.wait_for(proc.communicate(COMPACT_PROMPT.encode("utf-8")), timeout)
  except TimeoutError:
    if proc.pid is not None:
      process.kill_process_group(proc.pid)
    await proc.wait()
    return await _fail(
        persist_and_broadcast, log_context, f"timed out after {int(timeout)} s", model=model, trigger=failure_trigger)
  after = count_compact_boundaries(transcript)
  outcome = _judge(proc.returncode or 0, stdout, baseline, before, after, family)
  if not outcome.ok:
    stderr_tail = stderr.decode("utf-8", errors="replace").strip()[-300:]
    log.warning(
        "claude_compaction_failed",
        cc_session_id=cc_session_id,
        error=outcome.error,
        models=list(outcome.models),
        stderr=stderr_tail,
        **log_context,
    )
    await persist_and_broadcast(
        base.make_context_compact_failed_event(outcome.error, model=model, trigger=failure_trigger))
    return outcome
  # The boundary row's payload travels whole; for the pre count the caller's own reading wins.
  # When the row carries no readable compactMetadata the payload stays None and the line renders
  # from what the caller contributed alone.
  compact_metadata = _newest_boundary_compact_metadata(transcript)
  if pre_tokens is not None:
    compact_metadata = {**(compact_metadata or {}), ET.COMPACT_PRE_TOKENS: pre_tokens}
  post_tokens = compact_metadata.get(ET.COMPACT_POST_TOKENS) if compact_metadata else None
  log.info(
      "claude_compaction_done",
      cc_session_id=cc_session_id,
      pre_tokens=compact_metadata.get(ET.COMPACT_PRE_TOKENS) if compact_metadata else None,
      post_tokens=post_tokens,
      models=list(outcome.models),
      **log_context,
  )
  await persist_and_broadcast(base.make_context_compacted_event(trigger, compact_metadata, model=model))
  return dataclasses.replace(outcome, post_tokens=post_tokens)


async def compact_with_sonnet(
    *,
    cc_session_id: str,
    cwd: str,
    config_dir: str | pathlib.Path,
    pre_tokens: int | None,
    persist_and_broadcast: Callable[[dict], Awaitable[None]],
    log_context: dict,
    run_kind: models.RunKind,
    timeout: float = CLAUDE_COMPACTION_TIMEOUT,
    cgroup_session_id: str,
) -> bool:
  """Compact *cc_session_id*'s transcript under *config_dir* with Sonnet; True on success.

  *run_kind* is the kind of Run whose session is compacted: the subprocess declares that
  kind's auto-compact window, as the session's own processes do.

  Emits one ``context_compacted`` (``model`` = Sonnet; ``compact_metadata`` is the new
  boundary row's payload, with the caller's pre-compaction reading winning for
  ``pre_tokens``) or one ``context_compact_failed`` (``error`` names the cause). Never
  raises for a failed run; see :func:`run_compaction`.
  """
  outcome = await run_compaction(
      argv=compaction_command(cc_session_id, cgroup_session_id),
      env=compaction_env(config_dir, run_kind),
      cwd=cwd,
      config_dir=config_dir,
      cc_session_id=cc_session_id,
      family=COMPACTION_FAMILY,
      model=COMPACTION_MODEL,
      # The chat line has always named this compaction "manual". Its failure carries no
      # trigger: a failure that names one renders as that trigger's compaction, the idle line.
      trigger="manual",
      failure_trigger=None,
      pre_tokens=pre_tokens,
      persist_and_broadcast=persist_and_broadcast,
      log_context=log_context,
      timeout=timeout,
      cgroup_session_id=cgroup_session_id,
  )
  return outcome.ok


async def _fail(
    persist_and_broadcast: Callable[[dict], Awaitable[None]],
    log_context: dict,
    error: str,
    *,
    model: str,
    trigger: str | None,
) -> CompactionOutcome:
  log.warning("claude_compaction_failed", error=error, **log_context)
  await persist_and_broadcast(base.make_context_compact_failed_event(error, model=model, trigger=trigger))
  return CompactionOutcome(ok=False, error=error)

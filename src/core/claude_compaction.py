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

The module runs ``claude -p --resume <uuid> --model claude-sonnet-5`` with
``/compact`` on stdin inside the account's login directory and judges success
from two facts: the transcript gained a ``compact_boundary`` row and the run's
``modelUsage`` names Sonnet alone. Success and failure surface through the same
``context_compacted`` / ``context_compact_failed`` chat events the Claude Code
auto compaction path emits, with ``model`` naming the compacting model. A failed
run leaves the transcript as it was and the caller continues without compaction.
"""

from __future__ import annotations

import asyncio
import json
import os
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

import structlog

from src.agents.backends.claude_code import (
    BASE_COMMAND,
    claude_supervisor_env,
    headless_claude_env,
)
from src.core import event_types as ET
from src.core.claude_accounts import model_family, transcript_path
from src.core.config import CharlieBotConfig
from src.core.process import kill_process_group

log = structlog.get_logger()

# Claude Code writes one-hour prompt-cache entries only, and every hit renews the
# entry, so a request more than an hour after the previous one finds nothing cached.
CACHE_TTL = timedelta(minutes=60)

COMPACTION_MODEL = "claude-sonnet-5"
COMPACTION_FAMILY = "sonnet"
# The family whose own weekly bucket the compaction spares.
FABLE_FAMILY = "fable"

# A 70K-token compaction measured 21 s; the ceiling leaves room for a 400K one.
COMPACTION_TIMEOUT_SECONDS = 900.0

# /compact needs no tool; every tool Claude Code could reach is disallowed so the
# run can only read the transcript and write the summary.
COMPACTION_DISALLOWED_TOOLS = ",".join(
    (
        BASE_COMMAND[BASE_COMMAND.index("--disallowed-tools") + 1],
        "Bash,Read,Write,Edit,MultiEdit,NotebookEdit,Glob,Grep,WebSearch,WebFetch,TodoWrite,Skill,ToolSearch",
        "KillShell,BashOutput,AskUserQuestion,ExitPlanMode,Task",
    ))

COMPACT_PROMPT = "/compact\n"

_BOUNDARY_MARKER = '"compact_boundary"'


def _now(now: datetime | None) -> datetime:
  return now if now is not None else datetime.now(UTC)


# ---------------------------------------------------------------------------
# Trigger decisions (pure)
# ---------------------------------------------------------------------------


def is_fable(model: str | None) -> bool:
  return model_family(model) == FABLE_FAMILY


def cache_expired(last_request_at: datetime | None, now: datetime | None = None) -> bool:
  """True when the previous request is more than CACHE_TTL old; None (no request yet) is not expired."""
  return last_request_at is not None and _now(now) - last_request_at > CACHE_TTL


def expired_cache_compaction_wanted(
    cfg: CharlieBotConfig,
    model: str | None,
    context_tokens: int | None,
    last_request_at: datetime | None,
    now: datetime | None = None,
) -> bool:
  """A Fable turn starting on an expired cache with a context at or above the floor."""
  return (
      is_fable(model) and context_tokens is not None and
      context_tokens >= cfg.claude_compaction.expired_cache_tokens and cache_expired(last_request_at, now))


def relay_compaction_wanted(cfg: CharlieBotConfig, model: str | None, context_tokens: int | None) -> bool:
  """A Fable session about to relay to another account with a context at or above the floor."""
  return is_fable(model) and context_tokens is not None and context_tokens >= cfg.claude_compaction.relay_tokens


# ---------------------------------------------------------------------------
# Transcript facts
# ---------------------------------------------------------------------------


def _boundary_rows(transcript: Path) -> list[dict]:
  rows: list[dict] = []
  with transcript.open(encoding="utf-8", errors="replace") as handle:
    for line in handle:
      if _BOUNDARY_MARKER not in line:
        continue
      try:
        row = json.loads(line)
      except ValueError:
        continue
      if isinstance(row, dict) and row.get("subtype") == "compact_boundary":
        rows.append(row)
  return rows


def count_compact_boundaries(transcript: Path) -> int:
  """Number of ``compact_boundary`` rows in an on-disk transcript."""
  return len(_boundary_rows(transcript))


def _newest_boundary_pre_tokens(transcript: Path) -> int | None:
  rows = _boundary_rows(transcript)
  if not rows:
    return None
  meta = rows[-1].get("compactMetadata")
  value = meta.get("preTokens") if isinstance(meta, dict) else None
  return value if isinstance(value, int) and not isinstance(value, bool) else None


# ---------------------------------------------------------------------------
# The compaction run
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class CompactionOutcome:
  ok: bool
  error: str | None = None
  models: tuple[str, ...] = ()


def compaction_command(cc_session_id: str) -> list[str]:
  return [
      BASE_COMMAND[0],
      "-p",
      "--resume",
      cc_session_id,
      "--model",
      COMPACTION_MODEL,
      "--output-format",
      "json",
      "--disallowed-tools",
      COMPACTION_DISALLOWED_TOOLS,
  ]


def compaction_env(config_dir: str | Path) -> dict[str, str]:
  """Headless Claude Code environment pinned to one login directory.

  The host's own CLAUDE_CONFIG_DIR never reaches the child: the pool chose the
  directory and an inherited value would silently pick another account
  (LESSONS 2026-08-05).
  """
  env = {**claude_supervisor_env(os.environ), **headless_claude_env()}
  env.pop("CLAUDE_CONFIG_DIR", None)
  env["CLAUDE_CONFIG_DIR"] = str(Path(config_dir).expanduser())
  return env


def _judge(returncode: int, stdout: bytes, before: int, after: int) -> CompactionOutcome:
  try:
    result = json.loads(stdout.decode("utf-8", errors="replace") or "null")
  except ValueError:
    result = None
  if not isinstance(result, dict):
    return CompactionOutcome(ok=False, error=f"exit {returncode}, no JSON result on stdout")
  models = tuple(str(name) for name in (result.get("modelUsage") or {}))
  if returncode != 0 or result.get("is_error"):
    detail = str(result.get("result") or "").strip()[:200]
    return CompactionOutcome(ok=False, error=f"exit {returncode}: {detail or 'run reported an error'}", models=models)
  if after <= before:
    return CompactionOutcome(ok=False, error="transcript gained no compact_boundary row", models=models)
  if not models or any(model_family(name) != COMPACTION_FAMILY for name in models):
    return CompactionOutcome(ok=False, error=f"compaction served by {', '.join(models) or 'no model'}", models=models)
  return CompactionOutcome(ok=True, models=models)


async def compact_with_sonnet(
    *,
    cc_session_id: str,
    cwd: str,
    config_dir: str | Path,
    pre_tokens: int | None,
    persist_and_broadcast: Callable[[dict], Awaitable[None]],
    log_context: dict,
    timeout: float = COMPACTION_TIMEOUT_SECONDS,
) -> bool:
  """Compact *cc_session_id*'s transcript under *config_dir* with Sonnet; True on success.

  Emits one ``context_compacted`` (``model`` = Sonnet, ``pre_tokens`` = the
  caller's pre-compaction reading, else the boundary row's own count) or one
  ``context_compact_failed`` (``error`` names the cause). Never raises for a
  failed run: the caller proceeds on the untouched transcript.
  """
  transcript = transcript_path(config_dir, cc_session_id)
  if transcript is None:
    return await _fail(persist_and_broadcast, log_context, f"no transcript for {cc_session_id} under {config_dir}")
  before = count_compact_boundaries(transcript)
  cmd = compaction_command(cc_session_id)
  log.info("claude_compaction_starting", cc_session_id=cc_session_id, pre_tokens=pre_tokens, **log_context)
  try:
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        env=compaction_env(config_dir),
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        start_new_session=True,
    )
  except OSError as exc:
    return await _fail(persist_and_broadcast, log_context, f"could not start {cmd[0]}: {exc}")
  try:
    stdout, stderr = await asyncio.wait_for(proc.communicate(COMPACT_PROMPT.encode("utf-8")), timeout)
  except TimeoutError:
    if proc.pid is not None:
      kill_process_group(proc.pid)
    await proc.wait()
    return await _fail(persist_and_broadcast, log_context, f"timed out after {int(timeout)} s")
  after = count_compact_boundaries(transcript)
  outcome = _judge(proc.returncode or 0, stdout, before, after)
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
        {
            "type": ET.CONTEXT_COMPACT_FAILED,
            "error": outcome.error,
            "model": COMPACTION_MODEL,
        })
    return False
  event_pre_tokens = pre_tokens if pre_tokens is not None else _newest_boundary_pre_tokens(transcript)
  log.info(
      "claude_compaction_done",
      cc_session_id=cc_session_id,
      pre_tokens=event_pre_tokens,
      models=list(outcome.models),
      **log_context,
  )
  await persist_and_broadcast(
      {
          "type": ET.CONTEXT_COMPACTED,
          "trigger": "manual",
          "pre_tokens": event_pre_tokens,
          "model": COMPACTION_MODEL,
      })
  return True


async def _fail(persist_and_broadcast: Callable[[dict], Awaitable[None]], log_context: dict, error: str) -> bool:
  log.warning("claude_compaction_failed", error=error, **log_context)
  await persist_and_broadcast({"type": ET.CONTEXT_COMPACT_FAILED, "error": error, "model": COMPACTION_MODEL})
  return False

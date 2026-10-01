#!/usr/bin/env python3
"""Isolated live acceptance for the context-reset note a backend switch puts in
front of a session's first message: after the switch, the new backend's prompt
must carry the clone-style read-the-log instruction, and a switch back to the
producing backend before the next message must continue the native conversation
with no note.

This is the execution stage's live validation entry. It is opt-in (never part
of the default test run) and must be reviewed for isolation before it runs:

- Isolation. A fresh temporary CharlieBot home outside production carries the
  synthetic config (only the four named backend entries, no Claude account
  pool — so both cc-claude options resolve to the one process login directory
  and share a continuation domain), a synthetic operator key, and the trial
  sessions. The production service is never started, stopped or contacted: the
  server binds one explicit unused local port, runs without the normal lifespan
  (no crash recovery, scheduler or trigger scans), and its uvicorn instance is
  torn down at the end. Inherited production identity/credential environment
  variables (INHERITED_IDENTITY_ENV_VARS) are cleared from the harness
  environment; the shell HOME variable is never repurposed. Native CLC session
  state is isolated too: the harness installs the proven ``--session-dir``
  seam (session_tree_preview.install_native_session_isolation) so every
  charlie-code build in this process rides the adapter ``extra_flags`` kwarg
  with ``--session-dir <home>/clc-sessions``.
- Native claude/codex state. Claude and Codex have no proven session-dir
  override, so their native conversations land in their usual host directories
  (the Claude login directory's projects tree, the Codex sessions tree). The
  script records every native conversation id it creates and, after all
  assertions are read, deletes exactly those: the Codex ``rollout-*<id>.jsonl``
  files under the Codex sessions directory, and the Claude
  ``projects/<slug of the trial session cwd>/`` directories under the login
  directory. Before any deletion the script verifies the target sits inside
  the expected tree and holds only transcripts of its own recorded ids; it
  refuses to delete and fails the run otherwise. Nothing else in those
  directories is touched.
- Assertions. Six legs, each one session: turn 1 fixes the cb_/2-space/
  annotations/docstring conventions, the backend switches, turn 2 asks for a
  seconds-to-HH:MM:SS function. The four convention legs require the turn-2
  code to follow all three conventions and the prompt the new backend received
  (v2: the run's launch_prompt.md; v1: the receiving backend's own transcript
  or rollout record of the user prompt) to carry the instruction. The mis-click
  leg (switch away and back before turn 2) and the same-login leg (one Claude
  login, model to model) require turn 2 to keep turn 1's native conversation id
  and the received prompt to carry no ``[Context reset`` note. Each turn-2
  reply's opening sentences are recorded verbatim, never asserted — the wording
  is the model's. A network or backend failure is an explicit leg failure,
  never a mocked pass.
- Results. ``--out`` (required) receives one JSON record per leg: the backend
  route, pass/fail per assertion, the native conversation ids, and the reply
  opening. The exit code is nonzero when any assertion (or the native-file
  cleanup) fails. ``--keep`` keeps the trial home for inspection.

Run:  uv run python scripts/live_accept_switch_context.py --out PATH [--keep]
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
  sys.path.insert(0, str(REPO_ROOT))

import argparse  # noqa: E402
import ast  # noqa: E402
import asyncio  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import re  # noqa: E402
import shutil  # noqa: E402
import subprocess  # noqa: E402
import tempfile  # noqa: E402
import time  # noqa: E402
from dataclasses import dataclass, field  # noqa: E402
from typing import NoReturn  # noqa: E402

from scripts.browser_harness_session_tree import pick_free_port  # noqa: E402
from scripts.live_smoke_task_tree import (  # noqa: E402
    arequest,
    deps_tree,
    log,
    redact,
    register_secret,
    shutdown_servers,
    start_server,
    tree_run_dir,
)
from src.core import event_types as ET  # noqa: E402
from src.core.constants import INHERITED_IDENTITY_ENV_VARS  # noqa: E402
from src.core.sessions import CONTEXT_RESET_INSTRUCTION  # noqa: E402

# The four backend ids the legs switch between; both cc-claude entries must
# share one login directory (no account pool in the synthetic config) so they
# form one continuation domain, and the codex/charlie-code entries are each
# their own domain.
CLAUDE_SONNET = "claude-sonnet-5"
CLAUDE_OPUS = "claude-opus-5"
CODEX_LUNA = "codex-gpt-luna"
CLC_GLM53 = "charlie-code-glm53-flash"
BACKEND_IDS = (CLAUDE_SONNET, CLAUDE_OPUS, CODEX_LUNA, CLC_GLM53)

TURN1_CONVENTIONS = (
    "Let's fix some coding conventions for this session and follow them from now on: "
    "1) prefix every function name with cb_; 2) indent with 2 spaces; "
    "3) give every function type annotations and a one-line English docstring. "
    "Just confirm; do not call any tools.")
TURN2_TASK = "Write a Python function that formats a number of seconds as HH:MM:SS. Code only."

TURN_TIMEOUT_SECONDS = 600.0
REPLY_OPENING_CHARS = 400


def fail(message: str) -> NoReturn:
  raise SystemExit(f"SWITCH-ACCEPT FAILED: {redact(message)}")


# ---------------------------------------------------------------------------
# Preflight: every mechanism the legs depend on, before anything starts
# ---------------------------------------------------------------------------


def preflight() -> None:
  """Assert the source config, launchers, isolation seam and credentials exist.

    A failed check exits nonzero naming the missing mechanism. Reads the source
    (production) config and credentials only; the production server is never
    contacted.
    """
  from src.core.claude_accounts import credentials_present
  from src.core.config import claude_config_dir, load_config, load_credentials
  from src.core.home import CREDENTIALS_FILE
  from src.core.models import ClaudeAccount

  cfg = load_config()
  for backend_id in BACKEND_IDS:
    if cfg.get_backend_option(backend_id) is None:
      fail(f"backend option {backend_id!r} missing from the source config; "
           f"the switch legs cannot be built")
  from src.agents.backends.base import USER_LOCAL_BIN, resolve_binary
  binaries: dict[str, str] = {}
  for name in ("claude", "codex", "charlie-code"):
    try:
      binaries[name] = resolve_binary(name, USER_LOCAL_BIN)
    except FileNotFoundError as exc:
      fail(f"{name} launcher not found on PATH or {USER_LOCAL_BIN}: {exc}")
  help_text = subprocess.run([binaries["charlie-code"], "--help"], capture_output=True, text=True, check=False)
  if "--session-dir" not in help_text.stdout + help_text.stderr:
    fail("installed charlie-code does not support --session-dir; native CLC session "
         "isolation cannot be guaranteed")
  creds = load_credentials()
  for backend_id in BACKEND_IDS:
    entry = cfg.get_backend_option(backend_id)
    credential = getattr(entry, "credential", None)
    if credential and credential not in creds.sections:
      fail(
          f"backend {backend_id!r} references credential section {credential!r} "
          f"which is missing from credentials.yaml")
  login_dir = claude_config_dir()
  account = ClaudeAccount(label="preflight", config_dir=str(login_dir))
  if not credentials_present(account):
    fail(
        f"the claude login at {login_dir} carries no access token "
        f"({CREDENTIALS_FILE} missing or empty claudeAiOauth.accessToken); "
        f"the cc-claude legs cannot run")
  codex_auth = Path.home() / ".codex" / "auth.json"
  if not codex_auth.is_file():
    fail(f"codex login missing: {codex_auth} not found; the codex legs cannot run")


def load_source_entries() -> dict[str, dict]:
  """Read the four backend entries from the source config, redacting endpoints."""
  from src.core.config import load_config
  cfg = load_config()
  entries: dict[str, dict] = {}
  for backend_id in BACKEND_IDS:
    option = cfg.get_backend_option(backend_id)
    if option is None:
      fail(f"backend option {backend_id!r} is not configured in the source config")
    entries[backend_id] = json.loads(option.model_dump_json())
    register_secret(str(entries[backend_id].get("api_base") or ""), str(entries[backend_id].get("api_key") or ""))
  return entries


# ---------------------------------------------------------------------------
# Synthetic home and isolated server
# ---------------------------------------------------------------------------


def build_synthetic_home(home: Path, entries: dict[str, dict]) -> tuple[int, str]:
  """Write the synthetic home's config and credentials; return (port, access_key).

    Only the four backend entries reach the synthetic config — no triggers, no
    account pool, no production state. Without an account pool both cc-claude
    entries draw the process login directory, so they share one continuation
    domain exactly as the legs need.
    """
  port = pick_free_port()
  (home / "clc-sessions").mkdir(parents=True, exist_ok=True)
  from src.core.session_tree_preview import install_native_session_isolation
  install_native_session_isolation(home / "clc-sessions")
  config = {
      "server": {
          "port": port,
          "host": "127.0.0.1"
      },
      "backends": {
          "options": [entries[backend_id] for backend_id in BACKEND_IDS],
          "preference": list(BACKEND_IDS),
      },
  }
  (home / "config.yaml").write_text(json.dumps(config, indent=2), encoding="utf-8")
  access_key = "switch-accept-key-" + os.urandom(8).hex()
  (home / "credentials.yaml").write_text(f"charliebot:\n  access_key: {access_key}\n", encoding="utf-8")
  register_secret(access_key)
  return port, access_key


# ---------------------------------------------------------------------------
# Trial-state readers
# ---------------------------------------------------------------------------


def read_chat_events(home: Path, session_id: str) -> list[dict]:
  """The trial session's persisted chat events, oldest first."""
  from src.core.chat_events import chat_events_path
  path = chat_events_path(home / "sessions" / session_id)
  if not path.is_file():
    return []
  events: list[dict] = []
  for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
    try:
      events.append(json.loads(line))
    except ValueError:
      continue
  return events


def assistant_text(events: list[dict], after_index: int = -1) -> str:
  """The assistant reply text carried by the events after *after_index*.

    Groups the way MessageAggregator renders: an assistant event's text blocks
    join with no separator, consecutive assistant events are one message, and
    any other event closes the message; messages join with one newline.
    """
  messages: list[str] = []
  buffer: list[str] = []

  def flush() -> None:
    if buffer:
      messages.append("".join(buffer))
      buffer.clear()

  for event in events[after_index + 1:]:
    if event.get("type") != "assistant":
      flush()
      continue
    buffer.extend(
        str(block.get("text") or "")
        for block in (event.get("message") or {}).get("content") or []
        if isinstance(block, dict) and block.get("type") == "text")
  flush()
  return "\n".join(messages).strip()


def reply_opening(text: str, limit: int = REPLY_OPENING_CHARS) -> str:
  """The reply's opening sentences, verbatim (never asserted, only recorded)."""
  if len(text) <= limit:
    return text
  head = text[:limit]
  cut = max(head.rfind(". "), head.rfind(".\n"), head.rfind("! "), head.rfind("? "))
  if cut > limit // 2:
    head = head[:cut + 1]
  return head


async def wait_v2_run(session_id: str, known_run_ids: set[str], label: str) -> tuple[str, object, str]:
  """Wait for the dispatcher to reserve a fresh Run, then for its terminal fact."""
  tree = deps_tree()
  store = tree.runs
  deadline = time.monotonic() + 60
  run_id = None
  while time.monotonic() < deadline:
    registered = await asyncio.to_thread(store.list_run_records_sync, session_id)
    fresh = [r for r in registered if r.id not in known_run_ids]
    if fresh:
      run_id = fresh[0].id
      break
    await asyncio.sleep(0.2)
  if run_id is None:
    fail(f"{label}: the dispatcher never reserved a run for the admitted input")
  deadline = time.monotonic() + TURN_TIMEOUT_SECONDS
  while time.monotonic() < deadline:
    run = await asyncio.to_thread(store.read_run_sync, session_id, run_id)
    if run is None:
      fail(f"{label}: run {run_id} vanished from {session_id}")
    events = await asyncio.to_thread(store.load_events_sync, session_id)
    if store.run_has_terminal_fact(run, events):
      return run_id, run, str(store.terminal_outcome(events, run_id))
    await asyncio.sleep(1.0)
  fail(f"{label}: run {run_id} still running after {TURN_TIMEOUT_SECONDS:.0f}s")


async def wait_v1_round(home: Path, session_id: str, baseline_done: int, label: str) -> int:
  """Wait for one more settled v1 round (a master_done that leaves nothing running).

    Returns the new settled-done count. The anchor persist precedes the
    master_done broadcast, so a returned round guarantees the native id read.
    """
  deadline = time.monotonic() + TURN_TIMEOUT_SECONDS
  while time.monotonic() < deadline:
    events = await asyncio.to_thread(read_chat_events, home, session_id)
    settled = [e for e in events if e.get("type") == ET.MASTER_DONE and not e.get(ET.STILL_THINKING)]
    if len(settled) > baseline_done:
      done = settled[-1]
      if int(done.get("exit_code") or 0) != 0:
        fail(f"{label}: the round ended with exit_code={done.get('exit_code')}")
      return len(settled)
    await asyncio.sleep(1.0)
  fail(f"{label}: the round never settled within {TURN_TIMEOUT_SECONDS:.0f}s")


# ---------------------------------------------------------------------------
# Native-conversation record readers (claude transcript, codex rollout)
# ---------------------------------------------------------------------------


def codex_rollout_path(native_id: str) -> Path | None:
  """The Codex rollout file for *native_id* under the Codex sessions directory."""
  root = Path.home() / ".codex" / "sessions"
  if not root.is_dir():
    return None
  matches = sorted(root.rglob(f"rollout-*{native_id}.jsonl"), key=lambda p: p.stat().st_mtime)
  return matches[-1] if matches else None


def codex_user_prompts(native_id: str) -> list[str]:
  """The user-prompt texts the Codex rollout recorded for *native_id*."""
  path = codex_rollout_path(native_id)
  if path is None:
    return []
  prompts: list[str] = []
  for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if event.get("type") != "response_item":
      continue
    payload = event.get("payload") or {}
    if payload.get("type") != "message" or payload.get("role") != "user":
      continue
    prompts.extend(
        str(part["text"])
        for part in payload.get("content") or []
        if isinstance(part, dict) and part.get("type") == "input_text" and part.get("text"))
  return prompts


def claude_transcript_path(native_id: str) -> Path | None:
  """The Claude transcript for *native_id* under the login directory's projects tree."""
  from src.core.claude_accounts import transcript_matches
  from src.core.config import claude_config_dir
  matches = transcript_matches(claude_config_dir(), native_id)
  return matches[0] if matches else None


def claude_user_prompts(native_id: str) -> list[str]:
  """The user-prompt texts the Claude transcript recorded for *native_id*."""
  path = claude_transcript_path(native_id)
  if path is None:
    return []
  prompts: list[str] = []
  for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if event.get("type") != "user":
      continue
    message = event.get("message")
    if not isinstance(message, dict) or message.get("role") != "user":
      continue
    content = message.get("content")
    if isinstance(content, str):
      prompts.append(content)
    elif isinstance(content, list):
      prompts.extend(
          str(part["text"])
          for part in content
          if isinstance(part, dict) and part.get("type") == "text" and part.get("text"))
  return prompts


def received_prompt_record(family: str, native_id: str, needle: str) -> str | None:
  """The receiving backend's own record of one turn's user prompt.

    *needle* is the turn's raw message text; the record is the prompt the
    backend actually received (for a reset turn that includes the note).
    """
  prompts = codex_user_prompts(native_id) if family == "codex" else claude_user_prompts(native_id)
  for prompt in prompts:
    if needle in prompt:
      return prompt
  return None


# ---------------------------------------------------------------------------
# The reply-code conventions check
# ---------------------------------------------------------------------------


def reply_code_follows_conventions(reply: str) -> tuple[bool, str]:
    """True when the reply's Python code defines a cb_ function with the three
    turn-1 conventions: 2-space indentation, type annotations, a docstring."""
    fence = re.search(r"```(?:python|py)?[^\n]*\n(.*?)```", reply, re.DOTALL)
    code = fence.group(1) if fence else reply
    try:
        module = ast.parse(code)
    except SyntaxError as exc:
        return False, f"reply code does not parse: {exc}"
    functions = [n for n in module.body if isinstance(n, ast.FunctionDef) and n.name.startswith("cb_")]
    if not functions:
        return False, "no top-level cb_-prefixed function definition"
    func = functions[0]
    unannotated = [a.arg for a in func.args.args + func.args.kwonlyargs if a.annotation is None]
    if unannotated or func.returns is None:
        return False, f"missing type annotations: args={unannotated} returns={func.returns is not None}"
    if not ast.get_docstring(func):
        return False, "no docstring"
    indents = sorted({n.col_offset for n in func.body})
    if indents != [2]:
        return False, f"function body indents {indents}, expected 2 spaces"
    return True, func.name


# ---------------------------------------------------------------------------
# Legs
# ---------------------------------------------------------------------------


@dataclass
class LegSpec:
    name: str
    kind: str  # "v2" (task-tree manager) or "v1" (legacy session)
    route: list[str]  # [start backend, *switch targets, ...] in order
    expect: str  # "conventions" or "continue"


LEGS = [
    LegSpec("v2-claude-to-codex", "v2", [CLAUDE_SONNET, CODEX_LUNA], "conventions"),
    LegSpec("v2-codex-to-claude", "v2", [CODEX_LUNA, CLAUDE_SONNET], "conventions"),
    LegSpec("v2-claude-to-clc", "v2", [CLAUDE_SONNET, CLC_GLM53], "conventions"),
    LegSpec("v1-claude-to-codex", "v1", [CLAUDE_SONNET, CODEX_LUNA], "conventions"),
    LegSpec("v2-mis-click", "v2", [CLAUDE_SONNET, CODEX_LUNA, CLAUDE_SONNET], "continue"),
    LegSpec("v1-same-login", "v1", [CLAUDE_SONNET, CLAUDE_OPUS], "continue"),
]


@dataclass
class NativeRecord:
    family: str  # "claude" | "codex" | "charlie-code"
    session_id: str
    native_id: str
    cleanup: str = "pending"
    deleted: list[str] = field(default_factory=list)


def backend_family(backend_id: str) -> str:
    return {"claude-sonnet-5": "claude", "claude-opus-5": "claude",
            "codex-gpt-luna": "codex", "charlie-code-glm53-flash": "charlie-code"}[backend_id]


async def create_session(base: str, key: str, spec: LegSpec) -> str:
    if spec.kind == "v2":
        payload = {
            "request_id": f"switch-accept-{spec.name}",
            "profile": "manager",
            "name": spec.name,
            "backend": spec.route[0],
        }
    else:
        payload = {"name": spec.name, "backend": spec.route[0]}
    status, created = await arequest(base, key, "POST", "/api/sessions/", payload)
    if status != 200 or not created.get("id"):
        fail(f"leg {spec.name}: session create failed: {status} {created}")
    return str(created["id"])


async def run_leg(spec: LegSpec, base: str, key: str, home: Path,
                  natives: list[NativeRecord]) -> dict:
    """One leg: turn 1, the switch(es), turn 2, and its assertions. Never raises."""
    record: dict = {
        "leg": spec.name,
        "session_kind": spec.kind,
        "backends": list(spec.route),
        "session_id": None,
        "native_ids": [],
        "assertions": {},
        "reply_opening": None,
        "failures": [],
        "passed": False,
    }

    def note(assertion: str, passed: bool, detail: str) -> None:
        record["assertions"][assertion] = {"passed": passed, "detail": detail}
        if not passed:
            record["failures"].append(f"{assertion}: {detail}")

    try:
        session_id = await create_session(base, key, spec)
        record["session_id"] = session_id
        log(f"[{spec.name}] session {session_id} on {spec.route[0]}")

        # -- turn 1: fix the conventions -------------------------------------
        if spec.kind == "v2":
            known = {r.id for r in deps_tree().runs.list_run_records_sync(session_id)}
            status, posted = await arequest(
                base, key, "POST", f"/api/chat/{session_id}/message", {"content": TURN1_CONVENTIONS})
            if status != 202:
                fail(f"leg {spec.name}: turn-1 admission failed: {status} {posted}")
            _run1_id, run1, outcome1 = await wait_v2_run(session_id, known, f"{spec.name} turn 1")
            if outcome1 != "success":
                fail(f"leg {spec.name}: turn-1 run failed: outcome={outcome1}")
            native1 = run1.native_session_id
        else:
            events_before = await asyncio.to_thread(read_chat_events, home, session_id)
            baseline = len([
                e for e in events_before
                if e.get("type") == ET.MASTER_DONE and not e.get(ET.STILL_THINKING)])
            status, posted = await arequest(
                base, key, "POST", f"/api/chat/{session_id}/message", {"content": TURN1_CONVENTIONS})
            if status != 202:
                fail(f"leg {spec.name}: turn-1 admission failed: {status} {posted}")
            await wait_v1_round(home, session_id, baseline, f"{spec.name} turn 1")
            status, detail = await arequest(base, key, "GET", f"/api/sessions/{session_id}")
            if status != 200:
                fail(f"leg {spec.name}: session read failed: {status} {detail}")
            native1 = detail.get("cc_session_id")
        if not native1:
            fail(f"leg {spec.name}: turn 1 landed no native conversation id")
        record["native_ids"].append(native1)
        natives.append(NativeRecord(backend_family(spec.route[0]), session_id, str(native1)))
        log(f"[{spec.name}] turn 1 native id {str(native1)[:16]}... ({backend_family(spec.route[0])})")

        # -- the switch(es) ---------------------------------------------------
        for target in spec.route[1:]:
            status, switched = await arequest(
                base, key, "POST", f"/api/sessions/{session_id}/backend", {"backend": target})
            if status != 200:
                fail(f"leg {spec.name}: switch to {target} failed: {status} {switched}")
            log(f"[{spec.name}] switched to {target}")

        # -- turn 2: the task -------------------------------------------------
        if spec.kind == "v2":
            known = {r.id for r in deps_tree().runs.list_run_records_sync(session_id)}
            status, posted = await arequest(
                base, key, "POST", f"/api/chat/{session_id}/message", {"content": TURN2_TASK})
            if status != 202:
                fail(f"leg {spec.name}: turn-2 admission failed: {status} {posted}")
            run2_id, run2, outcome2 = await wait_v2_run(session_id, known, f"{spec.name} turn 2")
            if outcome2 != "success":
                fail(f"leg {spec.name}: turn-2 run failed: outcome={outcome2}")
            native2 = run2.native_session_id
            launch_path = tree_run_dir(session_id, run2_id) / "launch_prompt.md"
            received = launch_path.read_text(encoding="utf-8") if launch_path.is_file() else None
            if received is None:
                fail(f"leg {spec.name}: turn-2 launch_prompt.md missing at {launch_path}")
        else:
            events_before = await asyncio.to_thread(read_chat_events, home, session_id)
            baseline = len([
                e for e in events_before
                if e.get("type") == ET.MASTER_DONE and not e.get(ET.STILL_THINKING)])
            status, posted = await arequest(
                base, key, "POST", f"/api/chat/{session_id}/message", {"content": TURN2_TASK})
            if status != 202:
                fail(f"leg {spec.name}: turn-2 admission failed: {status} {posted}")
            await wait_v1_round(home, session_id, baseline, f"{spec.name} turn 2")
            status, detail = await arequest(base, key, "GET", f"/api/sessions/{session_id}")
            if status != 200:
                fail(f"leg {spec.name}: session read failed: {status} {detail}")
            native2 = detail.get("cc_session_id")
            family2 = backend_family(spec.route[-1])
            received = None if not native2 else received_prompt_record(family2, str(native2), TURN2_TASK)
            if received is None:
                fail(f"leg {spec.name}: the {family2} transcript/rollout for turn 2 "
                     f"(native id {native2}) carries no user prompt containing the task text")
        if not native2:
            fail(f"leg {spec.name}: turn 2 landed no native conversation id")
        record["native_ids"].append(native2)
        natives.append(NativeRecord(backend_family(spec.route[-1]), session_id, str(native2)))
        log(f"[{spec.name}] turn 2 native id {str(native2)[:16]}... ({backend_family(spec.route[-1])})")

        # -- the reply --------------------------------------------------------
        events = await asyncio.to_thread(read_chat_events, home, session_id)
        last_user = max((i for i, e in enumerate(events) if e.get("type") == "user"), default=-1)
        reply = assistant_text(events, last_user)
        record["reply_opening"] = reply_opening(reply)
        if not reply:
            fail(f"leg {spec.name}: turn 2 produced no assistant reply text")

        # -- assertions -------------------------------------------------------
        if spec.expect == "conventions":
            ok, detail = reply_code_follows_conventions(reply)
            note("reply_follows_conventions", ok, detail)
            has_instruction = received is not None and (
                "[Context reset" in received and CONTEXT_RESET_INSTRUCTION in received)
            note("prompt_carries_instruction", bool(has_instruction),
                 "the prompt the new backend received carries the reset note and instruction"
                 if has_instruction else
                 f"the received prompt lacks the reset note/instruction "
                 f"(head={'' if received is None else received[:120]!r})")
        else:
            note("native_conversation_continues",
                 bool(native1) and str(native1) == str(native2),
                 f"turn 2 native id {native2} vs turn 1 {native1}")
            no_note = received is not None and "[Context reset" not in received
            note("prompt_has_no_reset_note", bool(no_note),
                 "the received prompt carries no [Context reset note"
                 if no_note else "the received prompt carries a [Context reset note")
    except SystemExit as exc:
        record["failures"].append(redact(str(exc)))
    except Exception as exc:
        record["failures"].append(f"{type(exc).__name__}: {redact(str(exc))}")
    record["passed"] = not record["failures"]
    log(f"[{spec.name}] {'PASSED' if record['passed'] else 'FAILED'}")
    return record


# ---------------------------------------------------------------------------
# Native-file cleanup: exactly the recorded ids, nothing else
# ---------------------------------------------------------------------------


def cleanup_native_files(natives: list[NativeRecord], results: dict) -> None:
    """Delete the recorded native files only, verifying every target first."""
    from src.core.config import claude_config_dir

    codex_root = Path.home() / ".codex" / "sessions"
    projects_root = Path(claude_config_dir()).expanduser() / "projects"
    cleanup_ok = True

    claude_dirs: dict[Path, set[str]] = {}
    for entry in natives:
        if entry.family == "codex":
            if codex_root.is_dir():
                matches = sorted(codex_root.rglob(f"rollout-*{entry.native_id}.jsonl"))
                if not matches:
                    entry.cleanup = "rollout not found; nothing to delete"
                    continue
                for path in matches:
                    path.unlink()
                    entry.deleted.append(str(path))
                entry.cleanup = f"deleted {len(matches)} rollout file(s)"
            else:
                entry.cleanup = "codex sessions directory absent; nothing to delete"
        elif entry.family == "claude":
            from src.core.claude_accounts import transcript_matches
            matches = transcript_matches(claude_config_dir(), entry.native_id)
            if not matches:
                entry.cleanup = "transcript not found; nothing to delete"
                continue
            claude_dirs.setdefault(matches[0].parent, set()).add(entry.native_id)
        else:
            entry.cleanup = "charlie-code native state lives under the trial home; no host cleanup"

    deleted_dirs: list[str] = []
    for slug_dir, ids in claude_dirs.items():
        # Guard: the target must sit inside the login's projects tree and hold
        # only this trial's transcripts (its recorded ids plus the subagent
        # logs of its own turns). Anything else refuses the deletion.
        if projects_root not in slug_dir.parents:
            results["cleanup_failures"].append(
                f"refused to delete {slug_dir}: outside {projects_root}")
            cleanup_ok = False
            continue
        strangers = [
            p.name for p in slug_dir.glob("*.jsonl")
            if p.stem not in ids and not p.stem.startswith("agent-")]
        if strangers:
            results["cleanup_failures"].append(
                f"refused to delete {slug_dir}: holds non-trial transcripts {strangers}")
            cleanup_ok = False
            continue
        shutil.rmtree(slug_dir)
        deleted_dirs.append(str(slug_dir))
        for entry in natives:
            if entry.family == "claude" and entry.native_id in ids:
                entry.cleanup = "slug directory deleted"
                entry.deleted.append(str(slug_dir))
    results["native_cleanup"] = {
        "codex_rollouts_deleted": sorted(
            path for entry in natives for path in entry.deleted if entry.family == "codex"),
        "claude_dirs_deleted": deleted_dirs,
        "native_records": [
            {"family": e.family, "session_id": e.session_id, "native_id": e.native_id,
             "cleanup": e.cleanup} for e in natives],
    }
    if not cleanup_ok:
        results["cleanup_failures"].append("native-file cleanup refused a target; manual review required")


# ---------------------------------------------------------------------------
# Entry
# ---------------------------------------------------------------------------


async def accept(out_path: Path, keep: bool) -> int:
    preflight()
    entries = load_source_entries()

    home = Path(tempfile.mkdtemp(prefix="charliebot-switch-accept-"))
    port, access_key = build_synthetic_home(home, entries)
    os.environ["CHARLIEBOT_HOME"] = str(home)
    for var in INHERITED_IDENTITY_ENV_VARS:
        os.environ.pop(var, None)

    base = f"http://127.0.0.1:{port}"
    log(f"trial home: {home}")
    log(f"isolated server: {base}")

    results: dict = {
        "legs": [],
        "cleanup_failures": [],
        "harness_error": None,
        "trial_home": str(home),
        "passed": False,
    }
    natives: list[NativeRecord] = []
    try:
        try:
            await start_server(port, "charliebot-live-switch-accept")
        except RuntimeError as exc:
            fail(str(exc))
        for spec in LEGS:
            results["legs"].append(await run_leg(spec, base, access_key, home, natives))
    except SystemExit as exc:
        results["harness_error"] = redact(str(exc))
    except Exception as exc:
        results["harness_error"] = f"{type(exc).__name__}: {redact(str(exc))}"
    finally:
        await shutdown_servers()

    cleanup_native_files(natives, results)
    results["passed"] = (
        results["harness_error"] is None and not results["cleanup_failures"]
        and all(leg["passed"] for leg in results["legs"]))
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(results, indent=2), encoding="utf-8")
    log(f"results written to {out_path}")

    if keep:
        log(f"trial home kept for inspection: {home}")
    else:
        shutil.rmtree(home, ignore_errors=True)
        log("trial home purged")
    if results["harness_error"]:
        log(f"HARNESS ERROR: {results['harness_error']}")
    for failure in results["cleanup_failures"]:
        log(f"CLEANUP FAILURE: {failure}")
    log("SWITCH-ACCEPT PASSED" if results["passed"] else "SWITCH-ACCEPT FAILED")
    return 0 if results["passed"] else 1


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Isolated live acceptance: the context-reset note a backend switch "
                    "puts in front of a session's first message")
    parser.add_argument("--out", required=True, help="Path the JSON results are written to")
    parser.add_argument("--keep", action="store_true", help="Keep the trial home for inspection")
    args = parser.parse_args()
    raise SystemExit(asyncio.run(accept(Path(args.out).expanduser(), args.keep)))


if __name__ == "__main__":
    main()

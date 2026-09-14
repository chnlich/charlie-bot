#!/usr/bin/env python3
"""Isolated live smoke for the v2 session task tree: one real manager turn and
one real worker run on the configured charlie-code backend.

This is the execution stage's live validation entry. It is opt-in (never part
of the default test run) and must be reviewed for isolation before it runs:

- Isolation. A fresh temporary CharlieBot home outside production carries the
  synthetic config, synthetic operator key, sessions, transport and work dirs.
  The production service is never started, stopped or contacted: the server
  binds one explicit unused local port, runs without the normal lifespan (no
  crash recovery, scheduler or trigger scans), and its uvicorn instance is
  torn down at the end. Native CLC session state is isolated too: the
  configured backend entry rides ``extra_flags: ["--session-dir", ...]`` (a
  supported ``charlie-code --help`` override), so both real runs persist their
  native session state under the synthetic home, never in the production CLC
  state. Inherited production credentials are cleared from the harness
  environment; the shell HOME variable is never repurposed.
- Credentials. The selected production backend entry is read only to build the
  test config (its model/api-base/credential references). The synthetic key
  stays in the synthetic home's credentials file; this script never prints or
  commits it, and its log lines redact both key and endpoint.
- Assertions. Two real adapter runs (manager turn, worker work run) must
  persist native session ids and models on their Run records, acknowledge the
  exact claimed input, and land successful result refs. The worker must close
  and auto-archive on its delivered report; the manager must remain open. The
  compatibility aliases must resolve both run ids to the same Run. A network
  or backend failure is an explicit test failure, never a mocked pass.

Run:  uv run python scripts/live_smoke_task_tree.py [--backend ID] [--purge]
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

import argparse  # noqa: E402
import asyncio  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import shutil  # noqa: E402
import socket  # noqa: E402
import subprocess  # noqa: E402
import tempfile  # noqa: E402
import time  # noqa: E402
import urllib.error  # noqa: E402
import urllib.request  # noqa: E402
from typing import NoReturn  # noqa: E402

SMOKE_PHRASE = "SMOKE-TASK-TREE-OK-7Q4F"
RUN_TIMEOUT_SECONDS = 420.0
_REDACTIONS: list[str] = []


def redact(text: str) -> str:
    """Redact the synthetic key and the backend endpoint out of any log line."""
    for secret in _REDACTIONS:
        if secret:
            text = text.replace(secret, "<redacted>")
    return text


def log(message: str) -> None:
    print(redact(message), flush=True)


def fail(message: str) -> NoReturn:
    raise SystemExit(f"LIVE SMOKE FAILED: {redact(message)}")


def pick_free_port() -> int:
    """One explicit unused local address for the isolated server."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def load_production_backend_entry(backend_id: str) -> dict:
    """Read the selected backend entry from the production config (redacted in logs).

    Only this entry is copied into the synthetic home; nothing else from the
    production config (triggers, accounts, session state) reaches it.
    """
    from src.core.config import load_config
    option = load_config().get_backend_option(backend_id)
    if option is None:
        fail(f"backend option {backend_id!r} is not configured in the production config")
    return json.loads(option.model_dump_json())


def build_synthetic_home(home: Path, backend_id: str, entry: dict) -> tuple[int, str]:
    """Write the synthetic home's config and credentials; return (port, access_key)."""
    port = pick_free_port()
    entry = dict(entry)
    clc_sessions = home / "clc-sessions"
    clc_sessions.mkdir(parents=True, exist_ok=True)
    flags = list(entry.get("extra_flags") or [])
    flags += ["--session-dir", str(clc_sessions)]
    entry["extra_flags"] = flags
    config = {
        "server": {"port": port, "host": "127.0.0.1"},
        "backends": {"options": [entry], "preference": [backend_id]},
    }
    (home / "config.yaml").write_text(json.dumps(config, indent=2), encoding="utf-8")
    access_key = "smoke-operator-key-" + os.urandom(8).hex()
    (home / "credentials.yaml").write_text(
        f"charliebot:\n  access_key: {access_key}\n", encoding="utf-8")
    for secret in (access_key, str(entry.get("api_base") or ""), str(entry.get("api_key") or "")):
        if secret:
            _REDACTIONS.append(secret)
    return port, access_key


def preflight(backend_id: str) -> None:
    """Assert the mechanisms this smoke depends on before anything starts."""
    binary = shutil.which("charlie-code")
    if binary is None:
        fail("charlie-code binary is not installed; the live smoke cannot run")
    help_text = subprocess.run(
        [binary, "--help"], capture_output=True, text=True, check=False)
    if "--session-dir" not in help_text.stdout + help_text.stderr:
        fail("installed charlie-code does not support --session-dir; native session "
             "isolation cannot be guaranteed")
    from src.core.config import load_config
    if load_config().get_backend_option(backend_id) is None:
        fail(f"backend option {backend_id!r} missing from the production config")


def request(base: str, method: str, path: str, key: str, payload: dict | None = None,
            timeout: float = 30.0) -> tuple[int, dict | list]:
    body = None
    headers = {"Authorization": f"Bearer {key}"}
    if payload is not None:
        body = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(base + path, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read().decode("utf-8") or "{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read().decode("utf-8") or "{}")


def deps_tree():
    from src.api.deps import task_manager
    return task_manager()


_SERVERS: list = []


async def start_server(port: int) -> None:
    """Start the isolated API server on the reserved port, without the normal lifespan."""
    import uvicorn
    from fastapi import FastAPI

    from src.api import chat, internal, sessions, threads
    from src.api.auth import AuthMiddleware

    app = FastAPI(title="charliebot-live-smoke")
    app.add_middleware(AuthMiddleware)
    app.include_router(sessions.router, prefix="/api/sessions", tags=["sessions"])
    app.include_router(chat.router, prefix="/api/chat", tags=["chat"])
    app.include_router(threads.router, prefix="/api/threads", tags=["threads"])
    app.include_router(internal.router, prefix="/api/internal", tags=["internal"])
    config = uvicorn.Config(app, host="127.0.0.1", port=port, log_level="warning", lifespan="off")
    server = uvicorn.Server(config)
    task = asyncio.get_running_loop().create_task(server.serve())
    deadline = time.monotonic() + 30
    while not server.started:
        if task.done():
            fail(f"isolated server failed to start: {task.exception()!r}")
        if time.monotonic() > deadline:
            fail("isolated server did not start within 30s")
        await asyncio.sleep(0.05)
    _SERVERS.append(server)


async def wait_for_terminal_run(session_id: str, run_id: str, label: str) -> tuple[object, str]:
    """Poll one Run until it carries a terminal fact; the timeout is an explicit failure."""
    store = deps_tree().runs
    deadline = time.monotonic() + RUN_TIMEOUT_SECONDS
    last_pid = None
    while time.monotonic() < deadline:
        run = await asyncio.to_thread(store.read_run_sync, session_id, run_id)
        if run is None:
            fail(f"{label}: run {run_id} vanished from {session_id}")
        events = await asyncio.to_thread(store.load_events_sync, session_id)
        if store.run_has_terminal_fact(run, events):
            return run, str(store.terminal_outcome(events, run_id))
        last_pid = run.pid
        await asyncio.sleep(1.0)
    fail(f"{label}: run {run_id} still running after {RUN_TIMEOUT_SECONDS}s (pid={last_pid})")


async def wait_for_dispatch_run(session_id: str, label: str) -> str:
    """Wait for the dispatcher to reserve its consumer Run for the admitted input.

    The reservation id is deterministic (the dispatch request binds the sorted
    pending-batch ids), so the poll reads the same fact the dispatcher wrote.
    """
    from src.core.control_events import sha256_hex, stable_run_id

    tree = deps_tree()
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        pending = tree.dispatch.pending_inputs(session_id)
        ids = sorted(str(e.get("id")) for e in pending)
        if ids:
            predicted = stable_run_id(session_id, "dispatch:" + sha256_hex("\x00".join(ids)))
            run = await tree.runs.get_run(session_id, predicted)
            if run is not None:
                return run.id
        await asyncio.sleep(0.2)
    fail(f"{label}: the dispatcher never reserved a run for the admitted input")


async def smoke(backend_id: str, purge: bool) -> None:
    preflight(backend_id)
    entry = load_production_backend_entry(backend_id)

    home = Path(tempfile.mkdtemp(prefix="charliebot-live-smoke-"))
    port, access_key = build_synthetic_home(home, backend_id, entry)
    os.environ["CHARLIEBOT_HOME"] = str(home)
    # Clear inherited production identity: the smoke's children route through
    # the synthetic home only.
    for var in ("CHARLIEBOT_SESSION_ID", "CHARLIEBOT_RUN_TOKEN"):
        os.environ.pop(var, None)

    base = f"http://127.0.0.1:{port}"
    log(f"smoke home: {home}")
    log(f"isolated server: {base}")

    await start_server(port)
    try:
        # -- the manager task and its real manager turn ----------------------
        status, manager = request(base, "POST", "/api/sessions/", access_key, {
            "request_id": "smoke-manager-create",
            "profile": "manager",
            "name": "smoke-manager",
        })
        if status != 201:
            fail(f"manager task create failed: {status} {manager}")
        manager_id = manager["session_id"]
        log(f"manager task: {manager_id}")

        phrase_instruction = (
            "Take off. This is a bounded live smoke instruction: reply with exactly the "
            f"fixed synthetic phrase {SMOKE_PHRASE} and nothing else. Do not run any tool "
            "and do not delegate.")
        status, posted = request(base, "POST", f"/api/sessions/{manager_id}/message", access_key,
                                 {"content": phrase_instruction})
        if status != 202:
            fail(f"manager input admission failed: {status} {posted}")
        input_event_id = posted.get("input_event_id")
        if not input_event_id:
            fail(f"manager input admission returned no durable input id: {posted}")
        log(f"manager input event: {input_event_id}")

        run_id = await wait_for_dispatch_run(manager_id, "manager")
        manager_run, manager_outcome = await wait_for_terminal_run(
            manager_id, run_id, "manager turn")
        if manager_outcome != "success":
            fail(f"manager turn failed: outcome={manager_outcome} "
                 f"raw_log={manager_run.raw_log_ref}")
        if not manager_run.native_session_id:
            fail(f"manager run {run_id} did not persist a native session id")
        if not manager_run.model:
            fail(f"manager run {run_id} did not persist its model")
        if not manager_run.raw_log_ref or not Path(manager_run.raw_log_ref).is_file():
            fail(f"manager run {run_id} raw log missing: {manager_run.raw_log_ref}")
        pending = deps_tree().dispatch.pending_inputs(manager_id)
        if any(str(e.get("id")) == input_event_id for e in pending):
            fail("manager input batch was not acknowledged by the successful turn")
        log(f"manager run {run_id}: native={str(manager_run.native_session_id)[:12]}... "
            f"model={manager_run.model} outcome=success")

        # -- the delegation and its real worker run --------------------------
        repo = home / "smoke-repo"
        repo.mkdir()
        subprocess.run(["git", "init", "-q", "-b", "main", str(repo)], check=True)
        subprocess.run(["git", "-C", str(repo), "config", "user.email", "smoke@example.com"], check=True)
        subprocess.run(["git", "-C", str(repo), "config", "user.name", "smoke"], check=True)
        (repo / "README.md").write_text("live smoke synthetic repo\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "seed"], check=True)

        spec_path = home / "smoke-spec.md"
        spec_path.write_text(
            "## Goal\n\nReport the fixed synthetic phrase "
            f"{SMOKE_PHRASE} as your final answer.\n"
            "## Required Behavior\n\nNo repository edits, no external actions: answer and stop.\n"
            "## Acceptance Tests\n\nThe final report contains the exact phrase.\n"
            "## Out of Scope\n\nEverything else.\n",
            encoding="utf-8")
        status, delegated = request(base, "POST", "/api/internal/delegate", access_key, {
            "session_id": manager_id,
            "description": spec_path.read_text(encoding="utf-8"),
            "repo_path": str(repo),
            "base_branch": "main",
            "task_type": "quick-edit",
            "keep_worktree": False,
            "request_id": "smoke-delegate-1",
        })
        if status != 200:
            fail(f"delegate failed: {status} {delegated}")
        child_id = delegated.get("session_id")
        worker_run_id = delegated.get("run_id")
        if delegated.get("parent_session_id") != manager_id or not child_id or not worker_run_id:
            fail(f"delegate returned an unexpected contract: {delegated}")
        if delegated.get("thread_id") != worker_run_id:
            fail(f"delegate thread_id is not the run compatibility alias: {delegated}")
        log(f"worker child task: {child_id} run: {worker_run_id}")

        worker_run, worker_outcome = await wait_for_terminal_run(
            child_id, worker_run_id, "worker run")
        if worker_outcome != "success":
            fail(f"worker run failed: outcome={worker_outcome} raw_log={worker_run.raw_log_ref}")
        if not worker_run.native_session_id:
            fail(f"worker run {worker_run_id} did not persist a native session id")
        if not worker_run.raw_log_ref or not Path(worker_run.raw_log_ref).is_file():
            fail(f"worker run {worker_run_id} raw log missing: {worker_run.raw_log_ref}")
        log(f"worker run {worker_run_id}: native={str(worker_run.native_session_id)[:12]}... "
            f"model={worker_run.model} outcome=success")

        # -- the delivery facts ----------------------------------------------
        tree = deps_tree()
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            child_meta = await tree.load_meta(child_id)
            if child_meta is not None and child_meta.archived_of is not None:
                break
            await asyncio.sleep(1.0)
        else:
            fail(f"worker task {child_id} was not archived after its delivered report")
        if tree.task_state(manager_id) != "open":
            fail("the manager task did not remain open after the worker delivered")
        manager_events = tree.events.load_events(manager_id)
        reports = [e for e in manager_events
                   if e.get("type") == "child_report" and e.get("child_session_id") == child_id]
        if not reports or reports[-1].get("outcome") != "completed":
            fail(f"the manager never received the worker's completed report: {reports}")
        raw_tail = Path(worker_run.raw_log_ref).read_text(encoding="utf-8", errors="replace")[-6000:]
        if SMOKE_PHRASE not in raw_tail and SMOKE_PHRASE not in json.dumps(reports[-1]):
            fail("the worker's real output did not contain the synthetic phrase")

        # -- the compatibility aliases resolve to the same Run ---------------
        for owner in (child_id, manager_id):
            status, row = request(base, "GET", f"/api/threads/{owner}/threads/{worker_run_id}", access_key)
            if status != 200 or row.get("id") != worker_run_id:
                fail(f"alias {owner}/{worker_run_id} did not resolve to the run: {status} {row}")

        # -- native CLC session state stayed under the synthetic home --------
        clc_sessions = home / "clc-sessions"
        natives = [p.name for p in clc_sessions.iterdir()] if clc_sessions.is_dir() else []
        if not natives:
            fail(f"no native CLC session state under {clc_sessions}; --session-dir did not isolate")

        report = {
            "backend": backend_id,
            "manager_task": manager_id,
            "manager_run": run_id,
            "manager_native_session_id": manager_run.native_session_id,
            "manager_input_event": input_event_id,
            "worker_task": child_id,
            "worker_run": worker_run_id,
            "worker_native_session_id": worker_run.native_session_id,
            "worker_model": worker_run.model,
            "smoke_home": str(home),
            "manager_raw_log": manager_run.raw_log_ref,
            "worker_raw_log": worker_run.raw_log_ref,
        }
        (home / "smoke-report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        log("LIVE SMOKE PASSED")
        log(json.dumps(report, indent=2))
        if purge:
            shutil.rmtree(home, ignore_errors=True)
            log("smoke home purged")
        else:
            log(f"smoke home kept for inspection: {home}")
    finally:
        for server in _SERVERS:
            server.should_exit = True
        await asyncio.sleep(0.5)


def main() -> None:
    parser = argparse.ArgumentParser(description="Isolated live smoke for the v2 session task tree")
    parser.add_argument(
        "--backend", default="charlie-code-glm53-flash",
        help="The configured backend option id the smoke runs on (default: charlie-code-glm53-flash)")
    parser.add_argument("--purge", action="store_true", help="Remove the synthetic home after the run")
    args = parser.parse_args()
    asyncio.run(smoke(args.backend, args.purge))


if __name__ == "__main__":
    main()

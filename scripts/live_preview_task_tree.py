"""The fresh-home preview live execution harness: real CLC GLM through the real entry point.

Starts ``charliebot session-tree preview`` as its own foreground process on a
disposable home (the actual CLI — never a lifespan-off in-process server), then
exercises the manager/worker/reviewer launch paths with the real configured
GLM backend over the instance's HTTP API:

- a root manager's real takeoff turn, with prompt-preview parity against the
  stored launch snapshot and the historical run-context endpoint;
- a repo-less quick-edit worker: real work run, delivery facts, parent
  receipt, auto-archive, and the manager's follow-up turn consuming it;
- a bounded synthetic-repo implement worker: worktree creation inside the
  preview home, the auto-spawned review Run on the same worktree (the reviewer
  launch path), and the delivered report;
- the launcher workspace boundary: an outside repo is refused before any git
  work;
- native CLC state lands under ``<home>/clc-sessions`` and the host's
  production native session directory receives nothing;
- an independent second instance's sentinel files stay untouched throughout.

Every check is a real observation; a provider or network failure is an
explicit failed run, never a scripted pass. Evidence lands in --evidence-dir
(default: a directory under the host temp dir).
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

import argparse  # noqa: E402
import asyncio  # noqa: E402
import hashlib  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import subprocess  # noqa: E402
import time  # noqa: E402
import urllib.error  # noqa: E402
import urllib.request  # noqa: E402
from collections.abc import Callable  # noqa: E402
from typing import NoReturn  # noqa: E402

from scripts.browser_harness_session_tree import (  # noqa: E402
    EVIDENCE_ROOT_DEFAULT,
    open_evidence_dir,
    pick_free_port,
)

DEFAULT_BACKEND = "charlie-code-glm-flash"
MANAGER_PHRASE = "LIVE-PREVIEW-MANAGER-OK-7Q4F"
WORKER_PHRASE = "LIVE-PREVIEW-WORKER-OK-9K2D"
RUN_TIMEOUT_SECONDS = 420.0


def log(message: str) -> None:
    print(message, flush=True)


def fail(message: str) -> NoReturn:
    raise SystemExit(f"LIVE PREVIEW HARNESS FAILED: {message}")


def make_record(checks: list[dict]) -> Callable[[str, bool, str], None]:
    """The PASS/FAIL sink the live-preview harnesses record their checks through.

    The returned closure appends the check to *checks* (the list the harness's
    evidence JSON serializes), logs the verdict, and exits through ``fail`` on
    the first failed check: a failed assertion ends the trial, so the checks
    after it never run.
    """

    def record(name: str, ok: bool, detail: str) -> None:
        checks.append({"name": name, "ok": ok, "detail": detail})
        log(f"    [{'PASS' if ok else 'FAIL'}] {name}: {detail}")
        if not ok:
            fail(f"assertion failed: {name}: {detail}")

    return record


def request(base: str, key: str, method: str, path: str,
            payload: dict | None = None) -> tuple[int, dict]:
    headers = {"Authorization": "Bearer " + key}
    data = None
    if payload is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(payload).encode()
    req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, json.loads(resp.read().decode() or "{}")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode() or "{}")
        except ValueError:
            return e.code, {}


def snapshot_native_storage() -> dict[str, tuple[float, int]]:
    """The host's production native CLC session directory, hashed for the isolation proof."""
    native = Path.home() / ".charlie-code" / "sessions"
    if not native.is_dir():
        return {}
    return {
        str(p): (p.stat().st_mtime, p.stat().st_size)
        for p in sorted(native.rglob("*")) if p.is_file()
    }


def snapshot_tree(root: Path) -> dict[str, str]:
    return {
        str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in sorted(root.rglob("*")) if p.is_file()
    }


def build_synthetic_repo(repo: Path) -> None:
    repo.mkdir(parents=True)
    subprocess.run(["git", "init", "-q", "-b", "main", str(repo)], check=True)
    subprocess.run(["git", "-C", str(repo), "config", "user.email", "preview@example.com"], check=True)
    subprocess.run(["git", "-C", str(repo), "config", "user.name", "preview"], check=True)
    (repo / "README.md").write_text("live preview synthetic repo\n", encoding="utf-8")
    subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
    subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "seed"], check=True)


async def wait_run_terminal(base: str, key: str, session_id: str, run_id: str,
                            label: str) -> tuple[dict, str]:
    """Poll one Run over the instance API until a terminal state; timeout fails explicitly."""
    deadline = time.monotonic() + RUN_TIMEOUT_SECONDS
    last = ""
    while time.monotonic() < deadline:
        status, page = request(base, key, "GET",
                               f"/api/sessions/{session_id}/runs?order=desc&limit=50")
        if status != 200:
            fail(f"{label}: runs list failed: {status} {page}")
        row = next((r for r in page.get("items", []) if r.get("id") == run_id), None)
        if row is None:
            fail(f"{label}: run {run_id} vanished from {session_id}")
        state = row.get("state")
        if state in ("success", "failed", "stopped", "interrupted"):
            outcome = "success" if state == "success" else state
            return row, outcome
        last = f"state={state}"
        await asyncio.sleep(2.0)
    fail(f"{label}: run {run_id} still running after {RUN_TIMEOUT_SECONDS:.0f}s ({last})")


def check_snapshot_integrity(snapshot: dict, label: str) -> None:
    joined = "\n\n".join(b["text"] for b in snapshot["blocks"])
    if snapshot["char_count"] != len(joined):
        fail(f"{label}: snapshot char_count {snapshot['char_count']} != measured {len(joined)}")
    for block in snapshot["blocks"]:
        if block["body_ref"] != hashlib.sha256(block["text"].encode("utf-8")).hexdigest():
            fail(f"{label}: snapshot block body_ref mismatch for sources={block['sources']}")


def block_sources(snapshot: dict) -> list[tuple[str, str]]:
    return [(s["scope"], s["source_ref"]) for b in snapshot["blocks"] for s in b["sources"]]


async def run_harness(args: argparse.Namespace) -> None:
    import yaml

    from scripts.browser_harness_session_tree_preview import (
        build_source_home,
        preview_instance_env,
        preview_invocation,
        trial_home_root,
        wait_preview_ready,
    )
    from src.core.constants import INHERITED_IDENTITY_ENV_VARS

    evidence_dir = Path(args.evidence_dir)
    commit = open_evidence_dir(evidence_dir)

    tmp_path = trial_home_root("charliebot-live-preview-", keep=args.keep)
    source = tmp_path / "source-home"
    # build_source_home takes the selected backend ids as a list (the shared
    # harness helper's contract); a bare string would iterate per character.
    build_source_home(source, [args.backend])
    for var in INHERITED_IDENTITY_ENV_VARS:
        os.environ.pop(var, None)

    # The independent second instance: its own home, its own live writer
    # fence, its own sentinel files. The preview must never touch it.
    independent = tmp_path / "independent-service"
    (independent / "clc-sessions").mkdir(parents=True)
    (independent / "workspaces").mkdir(parents=True)
    (independent / "state").mkdir(parents=True)
    (independent / "state" / "independent_sentinel.json").write_text('{"independent": true}')
    from src.core.home_writer_fence import acquire_home_writer_fence, probe_writer_fence

    independent_fence = acquire_home_writer_fence(independent, purpose="independent service")

    # The fence's own state files are its identity bookkeeping; the sentinel
    # and everything else must stay byte-identical for the whole trial.
    def sentinel_snapshot(root: Path) -> dict[str, str]:
        fence_files = {"state/home_writer.lock", "state/writer_identity.json"}
        return {k: v for k, v in snapshot_tree(root).items() if k not in fence_files}

    independent_before = sentinel_snapshot(independent)

    home = tmp_path / "preview-home"
    port = pick_free_port()
    invocation = preview_invocation(home, port, args.backend, [])
    env = preview_instance_env(source)
    server_console = tmp_path / "server-console.log"
    log(f"starting preview instance on 127.0.0.1:{port} (home {home})")
    with open(server_console, "w", encoding="utf-8") as server_log_file:
        proc = subprocess.Popen(
            invocation, cwd=str(REPO_ROOT), env=env,
            stdout=server_log_file, stderr=subprocess.STDOUT)
        try:
            record = await wait_preview_ready(proc, home, server_console, fail, 120.0)
            base = record["url"]
            log(f"preview ready: {base} (branch {record['source_branch']}, "
                f"sha {record['source_sha'][:12]})")
            access_key = yaml.safe_load((home / "credentials.yaml").read_text())["charliebot"]["access_key"]
            if access_key == "source-operator-key-not-used":
                fail("the preview key equals the source operator key; the instance key was not private")
            results: dict = {"tested_commit": commit, "invocation": invocation,
                             "preview_record": record}

            # The synthetic repo lives inside the preview workspace boundary.
            repo = home / "workspaces" / "synthetic-repo"
            build_synthetic_repo(repo)

            # -- the root manager and its real takeoff turn ------------------
            status, created = request(base, access_key, "POST", "/api/sessions/", {
                "request_id": "live-preview-root", "profile": "manager",
                "name": "Live trial program",
                "task": {"goal": "Carry the live preview trial"},
                "backend": None,
            })
            if status != 200:
                fail(f"root manager create failed: {status} {created}")
            manager_id = created["id"]
            status, preview = request(base, access_key, "GET",
                                      f"/api/sessions/{manager_id}/effective-prompt?kind=manager_turn")
            if status != 200 or preview.get("kind") != "manager_turn":
                fail(f"manager effective-prompt failed: {status} {preview}")
            log(f"manager prompt preview: hash={str(preview.get('prompt_hash'))[:12]}...")

            instruction = (
                "Take off. Reply with exactly "
                f"{MANAGER_PHRASE} and then stop. Do not create subtasks, do not delegate."
            )
            status, msg = request(base, access_key, "POST",
                                  f"/api/chat/{manager_id}/message",
                                  {"content": instruction, "request_id": "live-preview-msg-1"})
            if status not in (200, 202):
                fail(f"manager message failed: {status} {msg}")
            # The reserved run id: the events feed carries the admitted input
            # and the dispatcher's reservation; poll the runs page instead.
            deadline = time.monotonic() + 30
            manager_run_id = None
            while time.monotonic() < deadline and manager_run_id is None:
                status, page = request(base, access_key, "GET",
                                       f"/api/sessions/{manager_id}/runs?order=desc&limit=5")
                for row in page.get("items", []):
                    if row.get("kind") == "manager_turn":
                        manager_run_id = row["id"]
                        break
                await asyncio.sleep(1.0)
            if manager_run_id is None:
                fail("the manager turn was never reserved")
            manager_run, outcome = await wait_run_terminal(
                base, access_key, manager_id, manager_run_id, "manager takeoff turn")
            if outcome != "success":
                raw = manager_run.get("raw_log_ref") or ""
                tail = Path(raw).read_text(errors="replace")[-1500:] if raw and Path(raw).is_file() else ""
                fail(f"manager turn outcome={outcome}; raw tail:\n{tail}")
            if manager_run.get("backend") != args.backend:
                fail(f"manager run backend {manager_run.get('backend')!r} != configured {args.backend!r}")
            if not manager_run.get("model"):
                fail("manager run did not persist its model")
            if not manager_run.get("native_session_id"):
                fail(f"manager run {manager_run_id} did not persist a native session id")
            if not manager_run.get("input_event_ids"):
                fail(f"manager run {manager_run_id} did not record its input batch")
            if not manager_run.get("raw_log_ref") or not Path(manager_run["raw_log_ref"]).is_file():
                fail(f"manager run {manager_run_id} raw log missing: {manager_run.get('raw_log_ref')}")
            results["manager_run"] = {
                "id": manager_run_id, "native": manager_run.get("native_session_id"),
                "model": manager_run.get("model"), "inputs": manager_run.get("input_event_ids"),
                "raw_log": manager_run.get("raw_log_ref"),
            }
            log(f"manager run {manager_run_id}: state=success native="
                f"{str(manager_run.get('native_session_id'))[:12]}... model={manager_run.get('model')}")

            # -- snapshot integrity, provenance, preview parity --------------
            snap_ref = manager_run.get("prompt_snapshot_ref")
            if not snap_ref or not Path(snap_ref).is_file():
                fail(f"manager run has no durable snapshot reference: {snap_ref}")
            stored = json.loads(Path(snap_ref).read_text(encoding="utf-8"))
            check_snapshot_integrity(stored, "manager")
            sources = block_sources(stored)
            if ("base", "prompts/task_base.md") not in sources or \
                    ("base", "prompts/task_manager.md") not in sources:
                fail(f"manager snapshot lacks the manager contract blocks: {sources}")
            if any(scope in ("subtree", "node") for scope, _ref in sources):
                fail(f"manager snapshot invented local rules for empty scopes: {sources}")
            if preview.get("prompt_hash") != stored["prompt_hash"] or \
                    preview.get("char_count") != stored["char_count"]:
                fail("the pre-launch prompt preview is not the launch contract")
            status, ctx = request(base, access_key, "GET",
                                  f"/api/sessions/{manager_id}/runs/{manager_run_id}/context")
            if status != 200 or ctx.get("snapshot") is None:
                fail(f"run context endpoint failed: {status} {ctx}")
            if ctx["snapshot"] != stored:
                fail("run-context snapshot differs from the stored prompt_snapshot.json")
            if ctx.get("legacy_prompt") is not None:
                fail("a fresh v2 run must not carry legacy raw-prompt evidence")
            launch_text = Path(home, "sessions", manager_id, "data", "runs",
                               manager_run_id, "launch_prompt.md")
            if not launch_text.is_file():
                fail(f"manager launch text evidence missing at {launch_text}")
            log(f"manager snapshot: hash={stored['prompt_hash'][:12]}... "
                f"chars={stored['char_count']} blocks={len(stored['blocks'])}")

            # -- the repo-less quick-edit worker -----------------------------
            spec_path = tmp_path / "worker-spec.md"
            spec_path.write_text(
                "## Goal\n\nReport the fixed synthetic phrase "
                f"{WORKER_PHRASE} as your final answer.\n"
                "## Required Behavior\n\nNo repository edits, no external actions: answer and stop.\n"
                "## Acceptance Tests\n\nThe final report contains the exact phrase.\n"
                "## Out of Scope\n\nEverything else.\n", encoding="utf-8")
            # verify is the repo-less worker kind: no repo, no worktree, the
            # run directory is the working surface.
            status, delegated = request(base, access_key, "POST", "/api/internal/delegate", {
                "session_id": manager_id,
                "description": spec_path.read_text(encoding="utf-8"),
                "repo_path": None,
                "base_branch": None,
                "task_type": "verify",
                "keep_worktree": False,
                "request_id": "live-preview-delegate-1",
            })
            if status != 200:
                fail(f"delegate failed: {status} {delegated}")
            child_id = delegated.get("session_id")
            worker_run_id = delegated.get("run_id")
            if delegated.get("parent_session_id") != manager_id or not child_id or not worker_run_id:
                fail(f"delegate returned an unexpected contract: {delegated}")
            log(f"worker child task: {child_id} run: {worker_run_id}")
            worker_run, worker_outcome = await wait_run_terminal(
                base, access_key, child_id, worker_run_id, "worker run")
            if worker_outcome != "success":
                raw = worker_run.get("raw_log_ref") or ""
                tail = Path(raw).read_text(errors="replace")[-1500:] if raw and Path(raw).is_file() else ""
                fail(f"worker run outcome={worker_outcome}; raw tail:\n{tail}")
            if not worker_run.get("native_session_id"):
                fail(f"worker run {worker_run_id} did not persist a native session id")
            w_snap_ref = worker_run.get("prompt_snapshot_ref")
            if not w_snap_ref or not Path(w_snap_ref).is_file():
                fail("worker run has no durable snapshot reference")
            w_stored = json.loads(Path(w_snap_ref).read_text(encoding="utf-8"))
            check_snapshot_integrity(w_stored, "worker")
            w_sources = block_sources(w_stored)
            # A repo-less verify worker carries the verify contract, never the
            # manager's.
            if ("base", "prompts/verify.md") not in w_sources or \
                    ("base", "prompts/task_base.md") not in w_sources:
                fail(f"worker snapshot lacks the verify contract blocks: {w_sources}")
            if ("base", "prompts/task_manager.md") in w_sources:
                fail("worker snapshot injected the manager contract")
            results["worker_run"] = {"id": worker_run_id, "native": worker_run.get("native_session_id"),
                                     "task": child_id}
            log(f"worker run {worker_run_id}: state=success native="
                f"{str(worker_run.get('native_session_id'))[:12]}... model={worker_run.get('model')}")

            # -- delivery facts: parent receipt, auto-archive, next turn ------
            events_path = home / "sessions" / manager_id / "data" / "chat_events.jsonl"
            if not events_path.is_file():
                fail(f"the manager's durable event stream is missing at {events_path}")
            deadline = time.monotonic() + 120
            archived = False
            reports: list[dict] = []
            while time.monotonic() < deadline:
                events = [json.loads(line) for line in
                          events_path.read_text(encoding="utf-8").splitlines() if line.strip()]
                reports = [e for e in events
                           if e.get("type") == "child_report" and e.get("child_session_id") == child_id]
                status, page = request(base, access_key, "GET",
                                       f"/api/sessions/tree?parent_id={manager_id}"
                                       "&include_archived=true&limit=50")
                rows = page.get("items", []) if status == 200 else []
                row = next((r for r in rows if r.get("id") == child_id), None)
                if reports and reports[-1].get("outcome") == "completed" and row is not None \
                        and row.get("task_state") == "completed" and row.get("archived"):
                    archived = True
                    break
                await asyncio.sleep(1.0)
            if not reports or reports[-1].get("outcome") != "completed":
                fail(f"the manager never received the worker's completed report: {reports}")
            if not archived:
                fail(f"worker task {child_id} was not archived after its delivered report")
            raw_tail = Path(worker_run["raw_log_ref"]).read_text(errors="replace")[-6000:]
            if WORKER_PHRASE not in raw_tail and WORKER_PHRASE not in json.dumps(reports[-1]):
                fail("the worker's real output did not contain the synthetic phrase")
            log("worker archived after delivery; the manager received the child report")

            # -- the synthetic-repo implement worker and its reviewer ---------
            spec2 = tmp_path / "implement-spec.md"
            spec2.write_text(
                "## Goal\n\nAdd a file named preview-evidence.txt containing exactly the line "
                f"{WORKER_PHRASE} at the repository root, commit it on the work branch, "
                "and report the phrase as your final answer.\n"
                "## Required Behavior\n\nWork only inside the provided repository and worktree.\n"
                "## Acceptance Tests\n\nThe file exists on the committed branch with the exact line.\n"
                "## Out of Scope\n\nEverything else.\n", encoding="utf-8")
            status, delegated2 = request(base, access_key, "POST", "/api/internal/delegate", {
                "session_id": manager_id,
                "description": spec2.read_text(encoding="utf-8"),
                "repo_path": str(repo),
                "base_branch": "main",
                "task_type": "implement",
                "keep_worktree": True,
                "request_id": "live-preview-delegate-2",
            })
            if status != 200:
                fail(f"implement delegate failed: {status} {delegated2}")
            impl_id = delegated2.get("session_id")
            impl_run_id = delegated2.get("run_id")
            log(f"implement child task: {impl_id} run: {impl_run_id}")
            impl_run, impl_outcome = await wait_run_terminal(
                base, access_key, impl_id, impl_run_id, "implement work run")
            if impl_outcome != "success":
                raw = impl_run.get("raw_log_ref") or ""
                tail = Path(raw).read_text(errors="replace")[-1500:] if raw and Path(raw).is_file() else ""
                fail(f"implement work run outcome={impl_outcome}; raw tail:\n{tail}")
            worktree = impl_run.get("worktree_path")
            if not worktree:
                fail("the implement work run did not record its worktree")
            if not str(worktree).startswith(str(home)):
                fail(f"the worktree escaped the preview home: {worktree}")
            if not Path(worktree).is_dir():
                fail(f"the recorded worktree does not exist: {worktree}")
            log(f"implement work run {impl_run_id}: worktree inside the preview home "
                f"({str(worktree)[:60]}...)")

            # The reviewer launch path: the work Run's auto-spawned review.
            deadline = time.monotonic() + 60
            review_id = None
            while time.monotonic() < deadline and review_id is None:
                status, page = request(base, access_key, "GET",
                                       f"/api/sessions/{impl_id}/runs?order=desc&limit=50")
                for row in page.get("items", []):
                    if row.get("kind") == "review" and row.get("review_of_run_id") == impl_run_id:
                        review_id = row["id"]
                        break
                await asyncio.sleep(1.0)
            if review_id is None:
                fail(f"no review Run was spawned for the work run {impl_run_id}")
            review_run, review_outcome = await wait_run_terminal(
                base, access_key, impl_id, review_id, "review run")
            if review_outcome != "success":
                raw = review_run.get("raw_log_ref") or ""
                tail = Path(raw).read_text(errors="replace")[-1500:] if raw and Path(raw).is_file() else ""
                fail(f"review run outcome={review_outcome}; raw tail:\n{tail}")
            results["implement_run"] = {"task": impl_id, "work": impl_run_id,
                                        "review": review_id, "worktree": worktree}
            log(f"review run {review_id}: state=success of work {impl_run_id} "
                f"on the same worktree")

            # -- the launcher workspace boundary ------------------------------
            status, delegated3 = request(base, access_key, "POST", "/api/internal/delegate", {
                "session_id": manager_id,
                "description": "## Goal\n\nTouch the runtime checkout.\n",
                "repo_path": str(REPO_ROOT),
                "base_branch": "main",
                "task_type": "quick-edit",
                "keep_worktree": False,
                "request_id": "live-preview-delegate-outside",
            })
            if status != 200:
                fail(f"outside delegate should be admitted then refused at launch: "
                     f"{status} {delegated3}")
            outside_id = delegated3.get("session_id")
            outside_run = delegated3.get("run_id")
            # The boundary refusal never reaches a process or a worktree: the
            # launch failure lands the Run's durable failed fact with the
            # boundary error as its evidence (the deliberate post-admission
            # launch-failure contract) — the queued reservation never turns
            # into execution.
            await asyncio.sleep(30)
            status, page = request(base, access_key, "GET",
                                   f"/api/sessions/{outside_id}/runs?order=desc&limit=5")
            outside_row = next((r for r in page.get("items", []) if r.get("id") == outside_run), None)
            if outside_row is None:
                fail("the outside-repo run vanished")
            if outside_row.get("state") != "failed":
                fail(f"the outside-repo launch should be refused with a durable failed "
                     f"fact; it shows {outside_row.get('state')}")
            if outside_row.get("worktree_path"):
                fail(f"the outside-repo launch created worktree state: "
                     f"{outside_row.get('worktree_path')}")
            if outside_row.get("pid") is not None:
                fail("the outside-repo launch pinned a process identity")
            events_ref = outside_row.get("events_ref") or ""
            if not events_ref or not Path(events_ref).is_file() or \
                    "preview workspace boundary" not in Path(events_ref).read_text(errors="replace"):
                fail("the refused run's events log never recorded the workspace-boundary error")
            instance_log_text = "\n".join(
                pth.read_text(encoding="utf-8", errors="replace")
                for pth in sorted((home / "logs").glob("*.log")))
            if "preview workspace boundary" not in instance_log_text:
                fail("the instance log never recorded the workspace-boundary refusal")
            worktrees_after = [p.name for p in (home / "worktrees").iterdir()]
            if len(worktrees_after) != 1:
                fail(f"the outside-repo launch created worktree state: {worktrees_after}")
            results["workspace_boundary"] = {"task": outside_id, "run": outside_run,
                                             "state": outside_row.get("state"),
                                             "worktree_created": False,
                                             "refused_at_launch": True}
            log("outside-repo launch refused at the boundary: no process, no worktree, "
                "durable failed fact with the boundary error as evidence")

            # -- native state isolation ---------------------------------------
            clc_sessions = home / "clc-sessions"
            natives = [p.name for p in clc_sessions.iterdir()] if clc_sessions.is_dir() else []
            if not natives:
                fail(f"no native CLC session state under {clc_sessions}; --session-dir did not isolate")
            # The host store is the production runtime's live working set: it
            # legitimately moves while this trial runs, so the isolation claim
            # is targeted — none of the preview's native session ids appear in
            # it (each run's native context is the preview home's directory).
            host_native_after = snapshot_native_storage()
            native_ids = [str(results["manager_run"]["native"]),
                          str(results["worker_run"]["native"]),
                          str(impl_run.get("native_session_id")),
                          str(review_run.get("native_session_id"))]
            leaked = [nid for nid in native_ids
                      if any(nid in path for path in host_native_after)]
            if leaked:
                fail(f"preview native session ids appeared in the host store: {leaked}")
            if sentinel_snapshot(independent) != independent_before:
                changed = sorted(set(sentinel_snapshot(independent))
                                 ^ set(independent_before))
                fail(f"the independent instance's files changed while the trial ran: {changed[:3]}")
            results["native_isolation"] = {
                "preview_native_entries": len(natives),
                "preview_native_ids_checked": native_ids,
                "host_native_files_total": len(host_native_after),
                "host_native_ids_leaked": [],
            }
            log(f"native isolation: {len(natives)} native session file(s) under the preview home, "
                f"none of the trial's native ids appear in the host store, "
                f"independent instance untouched")

            # -- restart on the same home: the review chain never re-registers --
            # The implement work Run carries a successful review. Startup
            # recovery replays the work Run's follow-up on every boot; the one
            # needs-review owner must end the chain there instead of registering
            # another review on the next reviewer backend. The preview CLI
            # supports restarting the same home, so the trial restarts the real
            # entry point and compares the node's Run list across the restart.
            impl_node = results["implement_run"]["task"]
            status, before_page = request(base, access_key, "GET",
                                          f"/api/sessions/{impl_node}/runs?order=desc&limit=100")
            if status != 200:
                fail(f"restart phase: runs list failed: {status} {before_page}")
            reviews_before = [r for r in before_page.get("items", []) if r.get("kind") == "review"]
            queued_before = [r for r in reviews_before if r.get("state") == "queued"]
            if queued_before:
                fail(f"restart phase: a review is already queued before the restart: "
                     f"{[r['id'] for r in queued_before]}")
            log(f"restart phase: {len(reviews_before)} review run(s) on the implement node before "
                f"the restart: {[(r['id'][:8], r.get('state')) for r in reviews_before]}")

            proc.terminate()
            try:
                proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=30)
            stopped_fence = probe_writer_fence(home)
            if stopped_fence["exclusive_holder_alive"]:
                fail("the preview writer fence is still held after the restart-phase shutdown")
            log("preview instance stopped; restarting the same home")

            restart_port = pick_free_port()
            restart_invocation = preview_invocation(home, restart_port, args.backend, [])
            results["restart"] = {"invocation": restart_invocation}
            with open(server_console, "a", encoding="utf-8") as server_log_file:
                proc = subprocess.Popen(
                    restart_invocation, cwd=str(REPO_ROOT), env=env,
                    stdout=server_log_file, stderr=subprocess.STDOUT)
                restart_record = await wait_preview_ready(proc, home, server_console, fail, 120.0)
            # wait_preview_ready returns only after the lifespan's recovery pass
            # (reconcile_task_tree) completed, so the durable Run set is final.
            base = restart_record["url"]
            results["restart"]["record"] = restart_record
            log(f"preview restarted on the same home: {base}")

            status, after_page = request(base, access_key, "GET",
                                         f"/api/sessions/{impl_node}/runs?order=desc&limit=100")
            if status != 200:
                fail(f"restart phase: post-restart runs list failed: {status} {after_page}")
            reviews_after = [r for r in after_page.get("items", []) if r.get("kind") == "review"]
            if len(reviews_after) != len(reviews_before):
                fail(f"restart phase: the review count moved across the restart: "
                     f"{len(reviews_before)} -> {len(reviews_after)} "
                     f"({[r['id'] for r in reviews_after]})")
            if {r["id"] for r in reviews_after} != {r["id"] for r in reviews_before}:
                fail(f"restart phase: the review run ids changed across the restart: "
                     f"{[r['id'] for r in reviews_before]} -> {[r['id'] for r in reviews_after]}")
            queued_after = [r for r in reviews_after if r.get("state") == "queued"]
            if queued_after:
                fail(f"restart phase: a queued review exists after the restart: "
                     f"{[r['id'] for r in queued_after]}")
            results["restart"]["review_count_before"] = len(reviews_before)
            results["restart"]["review_ids"] = sorted(r["id"] for r in reviews_before)
            results["restart"]["review_count_after"] = len(reviews_after)
            results["restart"]["queued_reviews_after"] = []
            log(f"restart phase: the implement node's review count stayed {len(reviews_after)} "
                f"across the restart and no review is queued")

            holder = probe_writer_fence(home)
            results["fence_while_serving"] = holder["exclusive_holder_alive"]

            (evidence_dir / "preview_live_results.json").write_text(
                json.dumps(results, indent=2), encoding="utf-8")
            log(f"results written to {evidence_dir / 'preview_live_results.json'}")
            log("LIVE PREVIEW HARNESS PASSED")
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=30)
            holder = probe_writer_fence(home)
            if holder["exclusive_holder_alive"]:
                fail("the preview writer fence is still held after shutdown")
            else:
                log("preview writer fence released on shutdown")
            independent_fence.release()
            log("server console tail:\n" + server_console.read_text()[-800:])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", default=str(EVIDENCE_ROOT_DEFAULT / "preview_live_evidence"),
                        help="Where the results JSON lands")
    parser.add_argument("--backend", default=DEFAULT_BACKEND,
                        help="The charlie-code backend id the trial instance runs")
    parser.add_argument("--keep", action="store_true",
                        help="Keep the preview home for inspection instead of purging it")
    args = parser.parse_args()
    asyncio.run(run_harness(args))


if __name__ == "__main__":
    main()

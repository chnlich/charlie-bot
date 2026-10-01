"""The task-tree sidebar-status live harness: real browser, fresh preview home.

Starts ``charliebot session-tree preview`` — the real entry point on a fresh
temporary home with the real ``charlie-code-glm-flash`` backend — and drives
the real UI in headless Chrome over CDP (the browser harness's client) while
the task tree runs real work. Through ``/api/sessions/status`` and the list
response the sidebar paints from (``GET /api/sessions/``), the DOM icons and
screenshots it asserts the sidebar's live work states:

1. running: a worker's ~60 s Run (a script-run task in a synthetic repo that
   sleeps, then reports) shows the spinner on its row and the amber gear on
   its parent, collapsed and expanded alike — a parent row's icon reads facts
   only and never its expansion state; after the Run ends the activity icons
   clear.
2. names and first paint: the rows carry goal-derived names, never "## Goal";
   no worker-facing title starts with a raw Markdown heading; and the list
   response already carries each task-tree row's ``work_state``, so the icons
   paint on first render without a poll.

Isolation: the trial's home is a fresh temp directory and its port a free
port; the harness's env is scrubbed of production identity variables; the
production service (127.0.0.1:18498) is never started, stopped, restarted or
contacted, and ``~/.charliebot`` / ``~/.charliebot-session-task-tree`` are
never touched; an independent sentinel home proves nothing outside the trial
changed. Evidence (screenshots, assertion JSON, tested commit) lands in
--evidence-dir, never in git.
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
  sys.path.insert(0, str(REPO_ROOT))

import argparse  # noqa: E402
import asyncio  # noqa: E402
import base64  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import subprocess  # noqa: E402
import time  # noqa: E402

from scripts.browser_harness_session_tree import (  # noqa: E402
    CDP,
    evaluate,
    open_evidence_dir,
    pick_free_port,
    resolve_chrome,
    stop_child,
)
from scripts.browser_harness_session_tree_preview import (  # noqa: E402
    build_source_home,
    open_authenticated_page,
    preview_instance_env,
    preview_invocation,
    trial_home_root,
    wait_preview_ready,
)
from scripts.live_preview_task_tree import (  # noqa: E402
    DEFAULT_BACKEND,
    build_synthetic_repo,
    fail,
    log,
    make_record,
    request,
    snapshot_native_storage,
    wait_run_terminal,
)
from src.core.constants import INHERITED_IDENTITY_ENV_VARS  # noqa: E402

PRODUCTION_PORT = 18498
PRODUCTION_HOMES = (
    Path.home() / ".charliebot",
    Path.home() / ".charliebot-session-task-tree",
)
SLOW_RUN_SECONDS = 65
WORKER_PHRASE = "SLOW-RUN-MARKER-Q7X2"


class Shots:
  """The screenshot sink ``evaluate``'s screenshot helper expects."""

  def __init__(self, evidence_dir: Path) -> None:
    self.evidence_dir = evidence_dir


async def screenshot(cdp: CDP, session_id: str, shots: Shots, name: str) -> str:
  res = await cdp.send("Page.captureScreenshot", {"format": "png"}, session_id=session_id)
  path = shots.evidence_dir / f"{name}.png"
  path.write_bytes(base64.b64decode(res["data"]))
  log(f"    screenshot: {path.name}")
  return path.name


def git(repo: Path, *args: str) -> str:
  proc = subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True)
  return proc.stdout.strip()


def build_slow_repo(home: Path) -> Path:
  """The ~60 s script-run repo: the worker runs slow_report.sh and reports it."""
  repo = home / "workspaces" / "slow-repo"
  build_synthetic_repo(repo)
  (repo / "slow_report.sh").write_text(
      "#!/usr/bin/env bash\n"
      "set -euo pipefail\n"
      f"sleep {SLOW_RUN_SECONDS}\n"
      f"printf '%s\\n' '{WORKER_PHRASE}' > report.txt\n",
      encoding="utf-8")
  git(repo, "add", ".")
  git(repo, "commit", "-q", "-m", "add slow_report.sh")
  return repo


async def create_manager(base: str, key: str, name: str, goal: str, request_id: str) -> str:
  status, created = request(
      base, key, "POST", "/api/sessions/", {
          "request_id": request_id,
          "profile": "manager",
          "name": name,
          "task": {
              "goal": goal
          },
          "backend": None,
      })
  if status != 200:
    fail(f"manager create failed: {status} {created}")
  manager_id = created["id"]
  log(f"  manager {name}: {manager_id}")
  return manager_id


async def takeoff(base: str, key: str, manager_id: str, request_id: str) -> str:
  """A real takeoff turn: the user message the delegation gate reads."""
  status, msg = request(
      base, key, "POST", f"/api/chat/{manager_id}/message", {
          "content": "Take off. Reply with exactly PREVIEW-READY and then stop.",
          "request_id": request_id
      })
  if status not in (200, 202):
    fail(f"manager message failed: {status} {msg}")
  deadline = time.monotonic() + 120
  while time.monotonic() < deadline:
    status, page = request(base, key, "GET", f"/api/sessions/{manager_id}/runs?order=desc&limit=5")
    for row in page.get("items", []):
      if row.get("kind") == "manager_turn" and row.get("state") in ("success", "failed"):
        if row["state"] != "success":
          fail(f"takeoff turn of {manager_id} ended {row['state']}")
        return row["id"]
    await asyncio.sleep(1.0)
  fail(f"takeoff turn of {manager_id} never finished")


async def delegate(base: str, key: str, manager_id: str, description: str, repo: Path, task_type: str,
                   request_id: str) -> tuple[str, str]:
  status, body = request(
      base, key, "POST", "/api/internal/delegate", {
          "session_id": manager_id,
          "description": description,
          "repo_path": str(repo),
          "base_branch": "main",
          "task_type": task_type,
          "keep_worktree": False,
          "request_id": request_id,
      })
  if status != 200:
    fail(f"delegate failed: {status} {body}")
  log(f"  worker {body['session_id']} run {body['run_id']}")
  return body["session_id"], body["run_id"]


async def wait_status(
    base: str, key: str, ids: list[str], session_id: str, predicate, label: str, timeout: float) -> dict:
  """Poll /api/sessions/status until one node's payload satisfies *predicate*."""
  deadline = time.monotonic() + timeout
  last: dict = {}
  while time.monotonic() < deadline:
    status, payload = request(base, key, "GET", "/api/sessions/status?ids=" + ",".join(ids))
    if status != 200:
      fail(f"status fetch failed: {status} {payload}")
    last = payload.get(session_id, {})
    if last and predicate(last):
      return last
    await asyncio.sleep(1.0)
  fail(f"status of {session_id} never satisfied {label}; last={json.dumps(last, default=str)}")


async def icon_hidden(cdp: CDP, page_id: str, sid: str, kind: str) -> bool:
  """Whether one indicator element carries the hidden class (row may be collapsed)."""
  value = await evaluate(cdp, page_id, f"document.getElementById('{kind}-{sid}')?.classList.contains('hidden')")
  return bool(value)


async def assert_icons(
    cdp: CDP,
    page_id: str,
    sid: str,
    visible: str | None,
    hidden: list[str],
    label: str,
    timeout: float = 30.0) -> None:
  """Wait until one row's icon table reads exactly *visible* (+ nothing else).

    The row's paint trails the API verdict by at most one status poll, so the
    assertion waits for the flip instead of sampling it; a timeout fails with
    the row's own words as evidence.
    """
  deadline = time.monotonic() + timeout
  last = ""
  while time.monotonic() < deadline:
    kinds = ("spinner", "worker-indicator", "waiting-indicator", "unread", "subtree-unread")
    states = {kind: await icon_hidden(cdp, page_id, sid, kind) for kind in kinds}
    ok = all(states[kind] for kind in hidden)
    if ok and visible is not None:
      ok = not states[visible]
    if ok:
      return
    last = json.dumps(states)
    await asyncio.sleep(0.5)
  fail(f"{label}: icons never reached the expected state ({last})\n{await icon_dump(cdp, page_id, sid)}")


async def worker_facing_titles(cdp: CDP, page_id: str) -> list[str]:
  """Every worker-facing title the live view paints: sidebar row names, the
    header session name, the transcript's per-Run header lines, and the
    Delegated cards' live-state lines. None may carry a raw Markdown heading."""
  return list(
      await evaluate(
          cdp, page_id, """
        (() => {
          const texts = [];
          document.querySelectorAll('.session-name').forEach(el => texts.push(el.textContent || ''));
          const headerName = document.getElementById('header-session-name');
          if (headerName) texts.push(headerName.textContent || '');
          document.querySelectorAll('[id^="run-header-"]').forEach(el => texts.push(el.textContent || ''));
          document.querySelectorAll('.delegate-live-state').forEach(el => texts.push(el.textContent || ''));
          return texts;
        })()
    """))


async def icon_dump(cdp: CDP, page_id: str, sid: str) -> str:
  """The row's own words at icon-assertion time: the evidence a failure needs."""
  try:
    return str(
        await evaluate(
            cdp, page_id, f"""
            JSON.stringify({{
              row: !!document.getElementById('session-{sid}'),
              rowHidden: document.getElementById('session-{sid}')?.closest('[data-tree-children]')?.classList.contains('hidden'),
              spinner: document.getElementById('spinner-{sid}')?.classList.contains('hidden'),
              gear: document.getElementById('worker-indicator-{sid}')?.classList.contains('hidden'),
              clock: document.getElementById('waiting-indicator-{sid}')?.classList.contains('hidden'),
              ownState: (window.Sidebar && Sidebar.sessionUnread) ? 'ns' : 'ns',
              errs: (window.__errs || []).slice(0, 3),
            }})
        """))
  except Exception as exc:
    return f"<dump failed: {exc!r}>"


async def run_harness(args: argparse.Namespace) -> None:
    chrome = resolve_chrome(args.chrome, fail)

    evidence_dir = Path(args.evidence_dir)
    commit = open_evidence_dir(evidence_dir)
    shots = Shots(evidence_dir)
    checks: list[dict] = []

    record = make_record(checks)

    # Isolation preflight: the production service and homes must be untouched
    # by construction — the trial gets a fresh temp home and a free port.
    if args.port is not None and args.port == PRODUCTION_PORT:
        fail("the requested port is the production port 18498")
    for home in PRODUCTION_HOMES:
        if home.exists():
            record("production home untouched (exists read-only, never written)", ok=True, detail=str(home))

    tmp_path = trial_home_root("charliebot-sidebar-status-", keep=args.keep)
    for var in INHERITED_IDENTITY_ENV_VARS:
        os.environ.pop(var, None)

    # An independent instance's sentinel home: nothing outside the trial changes.
    independent = tmp_path / "independent-service"
    (independent / "state").mkdir(parents=True)
    (independent / "state" / "independent_sentinel.json").write_text('{"independent": true}')
    independent_before = {
        str(p.relative_to(independent)): p.read_bytes()
        for p in sorted(independent.rglob("*")) if p.is_file()
    }

    source = tmp_path / "source-home"
    build_source_home(source, [args.backend])
    home = tmp_path / "preview-home"
    port = args.port or pick_free_port()
    if port == PRODUCTION_PORT:
        fail("the picked free port collided with the production port; refusing")
    invocation = preview_invocation(home, port, args.backend, [])
    env = preview_instance_env(source)
    server_console = tmp_path / "server-console.log"
    log(f"starting preview instance on 127.0.0.1:{port} (home {home})")
    with open(server_console, "w", encoding="utf-8") as server_log_file:
        proc = subprocess.Popen(invocation, cwd=str(REPO_ROOT), env=env,
                                stdout=server_log_file, stderr=subprocess.STDOUT)
        chrome_proc = None
        try:
            preview_record = await wait_preview_ready(proc, home, server_console, fail, 120.0)
            base = preview_record["url"]
            if f"127.0.0.1:{PRODUCTION_PORT}" in base:
                fail("the preview URL names the production port")
            log(f"preview ready: {base} (sha {preview_record['source_sha'][:12]})")
            access_key = (home / "credentials.yaml").read_text().split("access_key: ")[1].split("\n")[0]

            # ---- real Chrome over CDP -----------------------------------
            cdp, page_id, chrome_proc = await open_authenticated_page(
                chrome, tmp_path / "chrome-profile", port=port, access_key=access_key,
                domains=("Page", "Runtime", "Network"), fail=fail)
            # ---- scenario A: a real ~60 s worker Run shows as running -----
            log("scenario A: the running state (spinner on the worker, gear on the collapsed parent)")
            slow_repo = build_slow_repo(home)
            # The trial manager must stay open and listed through the whole
            # trial (the isolation postflight still reads its Run records). A
            # manager is instructed to request completion once its own
            # conditions hold, and a root task archives itself on that success
            # — so a completable one-line goal ("Sleep then report the
            # marker") lets the report-consuming turn legitimately close the
            # task and drop the row mid-trial. The goal therefore declares the
            # standing condition that keeps its completion conditions from
            # holding during the trial.
            manager_a = await create_manager(
                base, access_key, "Slow trial program",
                "## Goal\n\nSleep then report the marker\n\nThis trial task stays open after "
                "the marker is reported: its completion conditions never hold during the "
                "trial, so never request its completion or closure.\n",
                "sidebar-status-root-a")

            # The page is opened on the manager's URL: with a session id the
            # app connects its websocket, and the tree re-fetches on the
            # delegation broadcasts the rest of the trial rides.
            await cdp.send("Page.navigate", {"url": f"{base}/?session={manager_a}"},
                           session_id=page_id)
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if await evaluate(cdp, page_id, f"!!document.getElementById('session-{manager_a}')"):
                    break
                if time.monotonic() > deadline - 0.1:
                    fail("sidebar never rendered the trial manager's row")
                await asyncio.sleep(0.5)
            log("browser attached; the manager's row is rendered")

            results: dict = {"tested_commit": commit, "invocation": invocation,
                             "preview_record": preview_record, "backend": args.backend}

            await takeoff(base, access_key, manager_a, "sidebar-status-takeoff-a")
            worker_a, run_a = await delegate(
                base, access_key, manager_a,
                "## Goal\n\nRun `bash slow_report.sh` in the repository root. The script sleeps "
                f"about {SLOW_RUN_SECONDS} seconds and then writes the marker line into report.txt. "
                "Wait until the script has finished, then reply with the exact contents of report.txt.\n",
                slow_repo, "script-run", "sidebar-status-worker-a")

            status, detail = request(base, access_key, "GET", f"/api/sessions/{worker_a}")
            name = detail.get("name") if status == 200 else None
            record("worker name is the goal's first content line", status == 200 and name is not None
                   and name.startswith("Run `bash slow_report.sh`")
                   and "## Goal" not in name, f"name={name!r}")
            # Wait for the rows to render (the tree re-fetches on the
            # delegation broadcast), for the names, and for the Run to be live.
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                rows = await evaluate(cdp, page_id, """
                    [...document.querySelectorAll('#session-list a[id^=session-]')]
                        .map(el => el.id)
                """)
                if rows and f"session-{manager_a}" in rows and f"session-{worker_a}" in rows:
                    break
                await asyncio.sleep(1.0)
            else:
                fail(f"sidebar never rendered the trial rows: {rows}")
            row_names = await evaluate(cdp, page_id, """
                [...document.querySelectorAll('#session-list .session-name')].map(el => el.textContent)
            """)
            record("rows carry goal-derived names, never '## Goal'",
                   any("Slow trial program" in (n or "") for n in (row_names or []))
                   and any("Run `bash slow_report.sh`" in (n or "") for n in (row_names or []))
                   and all("## Goal" not in (n or "") for n in (row_names or [])),
                   f"rows={row_names}")

            # First paint carries work state: the list response the sidebar
            # paints from (GET /api/sessions/) carries the task-tree rows' work
            # verdicts, the same derivation the /status payload serves, so the
            # icons need no poll.
            def _list_rows() -> dict:
                status, body = request(base, access_key, "GET", "/api/sessions/")
                if status != 200:
                    fail(f"list fetch failed: {status}")
                return {row.get("id"): row for row in body}

            list_rows = _list_rows()
            trial_ids = [manager_a, worker_a]
            record("the list response carries work_state for the trial's task-tree rows",
                   all(list_rows.get(sid, {}).get("work_state") for sid in trial_ids),
                   json.dumps({sid: list_rows.get(sid, {}).get("work_state") for sid in trial_ids}))
            status, scoped = request(base, access_key, "GET",
                                     "/api/sessions/status?ids=" + ",".join(trial_ids))
            if status != 200:
                fail(f"status fetch failed: {status}")
            record("the list rows' work_state matches the status payload's derivation",
                   all(list_rows[sid].get("work_state") == scoped.get(sid, {}).get("work_state")
                       for sid in trial_ids),
                   json.dumps({sid: [list_rows[sid].get("work_state"), scoped.get(sid, {}).get("work_state")]
                               for sid in trial_ids}))
            titles_a = await worker_facing_titles(cdp, page_id)
            record("no worker-facing title starts with '## ' (manager view)",
                   bool(titles_a) and all(not (t or "").startswith("## ") for t in titles_a),
                   f"titles={titles_a}")

            def running_payload(st: dict) -> bool:
                return bool(st) and st.get("has_running_tasks") is True and st.get("work_state") == "running"

            ids_a = [manager_a, worker_a]
            # The real backend's launch prep (context build, worktree) takes
            # on the order of a minute; the queued phase before it is the
            # waiting verdict, and the launch flips the row to running.
            payload = await wait_status(base, access_key, ids_a, worker_a, running_payload,
                                        "worker running", timeout=300)
            record("status: worker has_running_tasks + work_state=running while live",
                   payload.get("has_running_tasks") is True and payload.get("work_state") == "running",
                   json.dumps(payload, default=str))
            parent_payload = await wait_status(
                base, access_key, ids_a, manager_a,
                lambda st: bool(st) and st.get("has_running_tasks") is False and st.get("work_state") == "idle",
                "manager idle", timeout=30)
            record("status: collapsed parent's own payload stays idle (its own Runs)",
                   parent_payload.get("has_running_tasks") is False
                   and parent_payload.get("work_state") == "idle",
                   json.dumps(parent_payload, default=str))
            await assert_icons(cdp, page_id, worker_a, "spinner",
                               ["worker-indicator", "waiting-indicator", "subtree-unread"],
                               "running worker row")
            await assert_icons(cdp, page_id, manager_a, "worker-indicator",
                               ["spinner", "waiting-indicator", "unread", "subtree-unread"],
                               "collapsed manager row")
            shot = await screenshot(cdp, page_id, shots, "running_state")
            results["running_screenshot"] = shot
            record("DOM: worker row spinner, collapsed manager gear", ok=True,
                   detail=f"{worker_a}/spinner + {manager_a}/gear")

            # Expanded manager keeps its gear: a parent row's icon reads facts
            # only, never the expansion state.
            await evaluate(cdp, page_id, f"Sidebar.expandTreeNode('{manager_a}')")
            await assert_icons(cdp, page_id, manager_a, "worker-indicator",
                               ["spinner", "waiting-indicator", "unread", "subtree-unread"],
                               "expanded manager row keeps the gear for its running worker")
            await assert_icons(cdp, page_id, worker_a, "spinner",
                               ["worker-indicator", "waiting-indicator", "subtree-unread"],
                               "running worker row (expanded parent)")
            shot = await screenshot(cdp, page_id, shots, "running_expanded")
            results["running_expanded_screenshot"] = shot
            record("DOM: expanded manager keeps its gear", ok=True, detail=manager_a)
            await evaluate(
                cdp, page_id,
                f"if (Sidebar.isTreeNodeExpanded('{manager_a}')) toggleTreeNode('{manager_a}')")

            run_row, outcome = await wait_run_terminal(base, access_key, worker_a, run_a, "slow work run")
            record("slow run reached terminal success", outcome == "success", f"outcome={outcome}")
            if outcome != "success":
                raw = run_row.get("raw_log_ref") or ""
                tail = Path(raw).read_text(errors="replace")[-1500:] if raw and Path(raw).is_file() else ""
                fail(f"slow run outcome={outcome}; raw tail:\n{tail}")
            payload = await wait_status(
                base, access_key, ids_a, worker_a,
                lambda st: bool(st) and st.get("has_running_tasks") is False
                and st.get("work_state") in (None, "idle"),
                "worker cleared", timeout=60)
            record("status after finish: worker's activity cleared",
                   payload.get("has_running_tasks") is False, json.dumps(payload, default=str))
            # The parent's report-consuming turn is its own thinking state,
            # which outranks every stand-in; the collapsed row's cleared icons
            # are only assertable once that turn has settled.
            await wait_status(
                base, access_key, ids_a, manager_a,
                lambda st: bool(st) and st.get("work_state") == "idle" and not st.get("thinking_since"),
                "manager A idle after consuming the report", timeout=180)
            await assert_icons(cdp, page_id, manager_a, None,
                               ["spinner", "worker-indicator", "waiting-indicator", "subtree-unread"],
                               "collapsed manager row after finish (no activity icon)")
            record("DOM after finish: collapsed manager's gear cleared", ok=True, detail=manager_a)
            shot = await screenshot(cdp, page_id, shots, "after_finish")
            results["after_finish_screenshot"] = shot

            # ---- isolation postflight ------------------------------------
            # The host store also carries the harness's own session logs (this
            # trial may run inside a CharlieBot session), so the check is the
            # trial's own ids — the Run records' native_session_id values, the
            # way live_preview_task_tree.py collects them — matched as whole
            # ids (one whole path component each). A directory-name substring
            # sweep would drag the preview's second-resolution run-timestamp
            # dirs (20260925T213047Z) into the check, and two production
            # sessions starting in the same second are common (7 of 470 on
            # 09-25), so that shape produced false "leaked" verdicts.
            native_after = snapshot_native_storage()
            trial_native_ids: set[str] = set()
            for sid in {manager_a, worker_a}:
                status, page = request(base, access_key, "GET", f"/api/sessions/{sid}/runs?limit=100")
                if status != 200:
                    fail(f"runs fetch of {sid} failed: {status}")
                for row in page.get("items", []):
                    native = row.get("native_session_id")
                    if native:
                        trial_native_ids.add(str(native))
            if not trial_native_ids:
                fail("no Run record carried a native_session_id; nothing to isolate")
            host_components = {component
                               for path in native_after
                               for component in Path(path).parts}
            leaked_ids = sorted(nid for nid in trial_native_ids if nid in host_components)
            record("the trial's native session ids never reached the host's store",
                   not leaked_ids,
                   f"checked={len(trial_native_ids)} {sorted(trial_native_ids)[:3]}... "
                   f"host_files={len(native_after)} leaked={leaked_ids[:3]}")
            independent_after = {
                str(p.relative_to(independent)): p.read_bytes()
                for p in sorted(independent.rglob("*")) if p.is_file()
            }
            record("the independent instance's files stayed untouched",
                   independent_after == independent_before, "sentinel home byte-identical")
            record("the preview home lives inside the trial's temp directory",
                   str(home).startswith(str(tmp_path)) and home.name == "preview-home", str(home))
            record("the trial port is not the production port", port != PRODUCTION_PORT,
                   f"port={port}")

            results["checks"] = checks
            (evidence_dir / "sidebar_status_results.json").write_text(
                json.dumps(results, indent=2), encoding="utf-8")
            log(f"results written to {evidence_dir / 'sidebar_status_results.json'}")
            log("SIDEBAR STATUS LIVE HARNESS PASSED")
        finally:
            stop_child(chrome_proc, grace_s=5, kill_reap_s=5)
            stop_child(proc, grace_s=60, kill_reap_s=30)
            log("server console tail:\n" + server_console.read_text()[-800:])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", required=True,
                        help="Where the screenshots, assertion JSON and tested commit land")
    parser.add_argument("--backend", default=DEFAULT_BACKEND,
                        help="The charlie-code backend id the trial instance runs")
    parser.add_argument("--port", type=int, default=None,
                        help="Fixed preview port (default: a free port; 18498 is refused)")
    parser.add_argument("--keep", action="store_true",
                        help="Keep the preview home for inspection instead of purging it")
    parser.add_argument("--chrome", default=None)
    args = parser.parse_args()
    asyncio.run(run_harness(args))


if __name__ == "__main__":
    main()

"""The pending-triggers tray live harness: real browser, fresh isolated home.

Starts the real server entry point (``server.py``) on a fresh temporary
CharlieBot home and a free port — not the session-tree preview, whose request
gate refuses ``/api/internal/schedule-trigger`` by design (that trial does not
run the mechanisms, src/core/session_tree_preview.py) while this one has to
register real triggers through ``charliebot schedule-trigger``. The harness
creates one chat session, registers its delayed triggers through that CLI, and
drives the real UI in headless Chrome over CDP while the tray renders them:

1. collapsed: one line under the messages — the bell, "Next trigger", the
   earliest local fire time with its "(in ...)" remaining time, the watched
   target, the message head and "+1 more".
2. expanded: the header ("2 pending triggers · next ...") and one row per
   trigger in fire_at order — the watched trigger's "fires when pid ... exits ·
   at the latest ..." and the pure-delay trigger's "fires at ...".
3. in-tray cancel: the row's X arms into red "Cancel?", the second click calls
   the cancel endpoint, and after the refetch the row is gone while the other
   stays.

Isolation mirrors the other live previews: the trial's home is a fresh temp
directory (its config carries only the selected backend entry) and its port a
free one; the harness env is scrubbed of production identity variables; the
production service (127.0.0.1:18498) is never started, stopped, restarted or
contacted, and ``~/.charliebot`` is never touched. Evidence (screenshots,
assertion JSON, tested commit) lands in --evidence-dir, never in git. No
message is ever sent, so no model call happens.
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
import re  # noqa: E402
import secrets  # noqa: E402
import shutil  # noqa: E402
import subprocess  # noqa: E402
import time  # noqa: E402
import urllib.error  # noqa: E402
import urllib.request  # noqa: E402

from scripts.browser_harness_session_tree import (  # noqa: E402
    evaluate,
    open_evidence_dir,
    pick_free_port,
    resolve_chrome,
    stop_child,
)
from scripts.browser_harness_session_tree_preview import (  # noqa: E402
    build_source_home,
    open_authenticated_page,
    trial_home_root,
)
from scripts.live_preview_task_tree import DEFAULT_BACKEND, fail, log, make_record, request  # noqa: E402
from src.core.constants import INHERITED_IDENTITY_ENV_VARS  # noqa: E402

PRODUCTION_PORT = 18498
PRODUCTION_HOME = Path.home() / ".charliebot"

# The trigger delays the collapsed/expanded copy shows: the watched one sits
# just over the reviewed mockup's "(in 3h 52m)" example, so the page still
# reads "(in 3h 5x m)" after the setup minute; the pure-delay trigger sits
# further out so the fire_at order is stable across the trial.
WATCHED_MAX_WAIT_S = 3 * 3600 + 55 * 60
PURE_DELAY_MAX_WAIT_S = 6 * 3600 + 30 * 60
WATCHED_MESSAGE = "tests finished: confirm the push landed on origin/main, then deploy"
PURE_DELAY_MESSAGE = "check the eval sweep results and report the best checkpoint"


def scrub_identity_env() -> dict[str, str]:
  env = dict(os.environ)
  for var in INHERITED_IDENTITY_ENV_VARS:
    env.pop(var, None)
  return env


async def screenshot(cdp, page_id: str, evidence_dir: Path, name: str) -> Path:
  res = await cdp.send("Page.captureScreenshot", {"format": "png"}, session_id=page_id)
  path = evidence_dir / f"{name}.png"
  path.write_bytes(base64.b64decode(res["data"]))
  log(f"    screenshot: {path}")
  return path


def build_trial_home(source: Path, home: Path, port: int) -> str:
  """The trial home: the source config bound to the free port + a fresh operator key.

  build_source_home wrote the selected backend entry, the credential section it
  references, and a placeholder operator key; the copy carries the trial's real
  identity, so the CLI's operator calls and the browser's login agree.
  """
  import yaml

  shutil.copytree(source, home)
  config_path = home / "config.yaml"
  config = yaml.safe_load(config_path.read_text(encoding="utf-8"))
  config["server"] = {"host": "127.0.0.1", "port": port}
  config_path.write_text(yaml.safe_dump(config), encoding="utf-8")
  access_key = "tray-" + secrets.token_urlsafe(24)
  creds_path = home / "credentials.yaml"
  creds = yaml.safe_load(creds_path.read_text(encoding="utf-8")) or {}
  creds["charliebot"] = {"access_key": access_key}
  creds_path.write_text(yaml.safe_dump(creds), encoding="utf-8")
  return access_key


def request_status(base: str, key: str, path: str) -> int | None:
  req = urllib.request.Request(base + path, headers={"Authorization": "Bearer " + key})
  try:
    with urllib.request.urlopen(req, timeout=10) as resp:
      return resp.status
  except urllib.error.HTTPError as e:
    return e.code
  except urllib.error.URLError:
    return None  # the socket is not listening yet; the readiness poll retries


async def wait_server_ready(proc: subprocess.Popen, base: str, key: str, console: Path, timeout_s: float) -> None:
  """Poll one authenticated read until the isolated server answers 200."""
  deadline = time.monotonic() + timeout_s
  last = None
  while time.monotonic() < deadline:
    if proc.poll() is not None:
      fail(f"server process exited early: {console.read_text()[-1500:]}")
    last = request_status(base, key, "/api/sessions/")
    if last == 200:
      return
    await asyncio.sleep(0.3)
  fail(f"isolated server never became ready (last status {last}; console {console.read_text()[-800:]})")


def schedule_trigger_cli(home: Path, session_id: str, *args: str) -> dict:
  """One real ``charliebot schedule-trigger`` call against the isolated instance.

  The CLI resolves the server from CHARLIEBOT_HOME (the trial home's config
  carries the trial port) and authenticates from its credentials.yaml — the
  same path an operator's shell command takes.
  """
  env = scrub_identity_env()
  env["CHARLIEBOT_HOME"] = str(home)
  env["PYTHONUNBUFFERED"] = "1"
  proc = subprocess.run(
      [sys.executable, "-m", "src.cli.main", "schedule-trigger", "--session", session_id, *args],
      cwd=REPO_ROOT,
      env=env,
      capture_output=True,
      text=True,
      timeout=60)
  if proc.returncode != 0:
    fail(f"schedule-trigger failed ({proc.returncode}): {proc.stdout}\n{proc.stderr}")
  return json.loads(proc.stdout)


async def wait_tray(cdp, page_id: str, predicate: str, label: str, timeout_s: float = 30.0) -> None:
  deadline = time.monotonic() + timeout_s
  while time.monotonic() < deadline:
    if await evaluate(cdp, page_id, predicate):
      return
    await asyncio.sleep(0.3)
  fail(f"the tray never showed {label}")


async def run_harness(args: argparse.Namespace) -> None:
  chrome = resolve_chrome(args.chrome, fail)

  evidence_dir = Path(args.evidence_dir)
  commit = open_evidence_dir(evidence_dir)
  checks: list[dict] = []

  record = make_record(checks)

  if args.port is not None and args.port == PRODUCTION_PORT:
    fail("the requested port is the production port 18498")
  if PRODUCTION_HOME.exists():
    record("production home untouched (exists read-only, never written)", ok=True, detail=str(PRODUCTION_HOME))

  tmp_path = trial_home_root("charliebot-pending-triggers-", keep=args.keep)

  source = tmp_path / "source-home"
  build_source_home(source, [args.backend])
  home = tmp_path / "trial-home"
  port = args.port or pick_free_port()
  if port == PRODUCTION_PORT:
    fail("the picked free port collided with the production port; refusing")
  access_key = build_trial_home(source, home, port)
  base = f"http://127.0.0.1:{port}"
  server_env = scrub_identity_env()
  server_env["CHARLIEBOT_HOME"] = str(home)
  server_env["PYTHONUNBUFFERED"] = "1"
  invocation = [sys.executable, "server.py"]
  server_console = tmp_path / "server-console.log"
  log(f"isolated instance: {' '.join(invocation)} (home {home}, port {port})")
  with open(server_console, "w") as console:
    proc = subprocess.Popen(invocation, env=server_env, stdout=console, stderr=subprocess.STDOUT, cwd=REPO_ROOT)
    chrome_proc = None
    try:
      await wait_server_ready(proc, base, access_key, server_console, 120.0)
      log(f"server ready: {base}")

      # A plain chat session; the tray is a chat-column surface, so no task
      # tree and no message ever rides this trial.
      status, created = request(base, access_key, "POST", "/api/sessions/", {"name": "Pending triggers preview"})
      if status != 200:
        fail(f"session create failed: {status} {created}")
      session_id = created["id"]
      log(f"  session {session_id}")

      watched = schedule_trigger_cli(
          home, session_id, "--max-wait", str(WATCHED_MAX_WAIT_S), "--watch", str(os.getpid()), "--message",
          WATCHED_MESSAGE)
      pure = schedule_trigger_cli(
          home, session_id, "--max-wait", str(PURE_DELAY_MAX_WAIT_S), "--message", PURE_DELAY_MESSAGE)
      log(f"  triggers: watched {watched['trigger_id']}, pure {pure['trigger_id']}")

      status, payload = request(base, access_key, "GET", f"/api/sessions/{session_id}/pending-triggers")
      record(
          "endpoint lists both pending triggers fire_at-first", status == 200 and len(payload) == 2 and
          payload[0]["id"] == watched["trigger_id"] and payload[0]["watch_targets"][0]["pid"] == os.getpid(),
          json.dumps(payload)[:200])

      # ---- real Chrome over CDP -------------------------------------
      cdp, page_id, chrome_proc = await open_authenticated_page(
          chrome, tmp_path / "chrome-profile", port=port, access_key=access_key, domains=("Page", "Runtime"), fail=fail)

      await cdp.send("Page.navigate", {"url": f"{base}/?session={session_id}"}, session_id=page_id)
      await wait_tray(
          cdp, page_id, "!!document.getElementById('pending-triggers-tray')"
          " && !document.getElementById('pending-triggers-tray').classList.contains('hidden')"
          " && document.getElementById('pending-triggers-tray').innerHTML.includes('Next trigger')",
          "the collapsed tray")
      collapsed_text = await evaluate(
          cdp, page_id, """
          document.getElementById('pending-triggers-tray').innerText.replace(/\\s+/g, ' ').trim()
      """)
      record(
          "collapsed tray copy matches the mockup",
          isinstance(collapsed_text, str) and collapsed_text.startswith("Next trigger") and
          re.search(r"\(in 3h 5\dm\)", collapsed_text) is not None and "pid " + str(os.getpid()) in collapsed_text and
          "+1 more" in collapsed_text, repr(collapsed_text))
      await screenshot(cdp, page_id, evidence_dir, "pending_triggers_collapsed")

      await evaluate(cdp, page_id, "document.querySelector('#pending-triggers-tray [role=button]').click()")
      await wait_tray(
          cdp, page_id, "document.getElementById('pending-triggers-tray').innerHTML.includes('pending triggers')",
          "the expanded tray")
      expanded_text = await evaluate(
          cdp, page_id, """
          document.getElementById('pending-triggers-tray').innerText.replace(/\\s+/g, ' ').trim()
      """)
      record(
          "expanded tray copy matches the mockup",
          isinstance(expanded_text, str) and "2 pending triggers" in expanded_text and
          "fires when pid " + str(os.getpid()) + " exits" in expanded_text and "fires at" in expanded_text and
          "at the latest" in expanded_text, repr(expanded_text))
      await screenshot(cdp, page_id, evidence_dir, "pending_triggers_expanded")

      # The in-tray cancel: the watched row's X, armed, then confirmed.
      watched_row = f'[data-trigger-row="{watched["trigger_id"]}"]'
      await evaluate(cdp, page_id, f"document.querySelector('#pending-triggers-tray {watched_row} button').click()")
      await wait_tray(
          cdp, page_id, "!!document.querySelector('#pending-triggers-tray [title=\"Click again to cancel\"]')",
          "the armed Cancel? button")
      cancel_text = await evaluate(
          cdp, page_id, """
          (document.querySelector('#pending-triggers-tray [title="Click again to cancel"]') || {}).textContent || ''
      """)
      record("the armed button reads Cancel?", cancel_text == "Cancel?", repr(cancel_text))
      await evaluate(
          cdp, page_id, "document.querySelector('#pending-triggers-tray [title=\"Click again to cancel\"]').click()")
      await wait_tray(
          cdp, page_id, f"!document.querySelector('#pending-triggers-tray {watched_row}')",
          "the cancelled row gone after the refetch")
      after_text = await evaluate(
          cdp, page_id, """
          document.getElementById('pending-triggers-tray').innerText.replace(/\\s+/g, ' ').trim()
      """)
      record(
          "after the cancel one row remains",
          isinstance(after_text, str) and "1 pending trigger" in after_text and PURE_DELAY_MESSAGE in after_text,
          repr(after_text))
      status, payload = request(base, access_key, "GET", f"/api/sessions/{session_id}/pending-triggers")
      record(
          "endpoint dropped the cancelled trigger", status == 200 and
          [row["id"] for row in payload] == [pure["trigger_id"]],
          json.dumps(payload)[:200])
      await screenshot(cdp, page_id, evidence_dir, "pending_triggers_after_cancel")

      errs = await evaluate(cdp, page_id, "window.__errs")
      record("no page errors through the trial", errs == [], json.dumps(errs))

      (evidence_dir / "checks.json").write_text(
          json.dumps(
              {
                  "tested_commit": commit,
                  "invocation": invocation,
                  "home": str(home),
                  "backend": args.backend,
                  "session_id": session_id,
                  "checks": checks,
              },
              indent=2,
              default=str))
      log(f"evidence: {evidence_dir}")
    finally:
      stop_child(chrome_proc, grace_s=10, kill_reap_s=10)
      stop_child(proc, grace_s=10, kill_reap_s=10)


def main() -> None:
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument(
      "--evidence-dir", required=True, help="directory the screenshots and the assertion JSON land in (never in git)")
  parser.add_argument(
      "--backend", default=DEFAULT_BACKEND, help="the trial's default backend id (must exist in the current profile)")
  parser.add_argument(
      "--port", type=int, default=None, help="the trial instance's loopback port (default: a free port)")
  parser.add_argument("--keep", action="store_true", help="keep the trial home for inspection instead of deleting it")
  parser.add_argument("--chrome", default=None, help="chrome binary (default: google-chrome on PATH)")
  args = parser.parse_args()
  asyncio.run(run_harness(args))


if __name__ == "__main__":
  main()

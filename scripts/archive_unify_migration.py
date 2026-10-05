#!/usr/bin/env python3
"""One-shot migration that makes "archived" the single end state of a task node.

Three subcommands around the server restart (the repo's own entry point for the
plan's rollout steps; every server touch rides the operator API — the script
never writes a session file):

  snapshot --out FILE   While the OLD server still runs: fold every task node's
                        raw event history read-only and list the nodes the
                        migration must archive (state open, and the retired
                        presentation is "hidden" or the stored status is
                        "archived"), plus every task node's parent for the
                        apply-time parent check.
  apply --in FILE       After the restart: DELETE /api/sessions/{id} for each
                        listed node, then read every listed node and every
                        recorded parent back and compare. Any mismatch prints
                        and exits non-zero.
  rollback              While the NEW server still runs, before a code revert:
                        POST /api/sessions/{id}/unarchive for every task node
                        whose last close outcome is archived.

Each subcommand preflights the same two facts: the configured server answers,
and credentials.yaml carries charliebot.access_key. The snapshot parses
metadata.json and data/chat_events.jsonl as raw JSON — it must never import the
model module that drops the retired presentation field (asserted below).
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

SNAPSHOT_KIND = "archive_unify_migration/snapshot/v1"


def _base_url() -> str:
  """The configured server's base URL (the CLI's own resolution)."""
  from src.cli.common import _internal_base_url
  return _internal_base_url()


def _access_key() -> str:
  """The operator key every write rides; empty means the preflight fails."""
  from src.core.credentials import configured_access_key
  return configured_access_key()


def _headers(key: str) -> dict[str, str]:
  return {"Authorization": f"Bearer {key}", "Content-Type": "application/json"}


def _request(base: str, key: str, method: str, path: str, payload: dict | None = None) -> tuple[int, dict | list]:
  data = json.dumps(payload).encode() if payload is not None else None
  req = urllib.request.Request(base + path, data=data, headers=_headers(key), method=method)
  try:
    with urllib.request.urlopen(req, timeout=30) as resp:
      body = resp.read().decode()
      return resp.status, (json.loads(body) if body else {})
  except urllib.error.HTTPError as e:
    try:
      return e.code, json.loads(e.read().decode() or "{}")
    except ValueError:
      return e.code, {}
  except urllib.error.URLError as e:
    return 0, {"error": str(e)}  # an unreachable server is a refusal, not a crash


def preflight(*, require_server: bool) -> str:
  """The two facts every subcommand needs: a reachable server and an operator key."""
  key = _access_key()
  if not key:
    sys.exit("preflight failed: credentials.yaml has no charliebot.access_key")
  base = _base_url()
  if require_server:
    status, _body = _request(base, key, "GET", "/api/internal/version")
    if status != 200:
      sys.exit(f"preflight failed: the server at {base} answered {status} for /api/internal/version")
  return base


# ---------------------------------------------------------------------------
# snapshot: raw-JSON fold over the sessions directory (old server still up)
# ---------------------------------------------------------------------------


def _read_jsonl(path: Path) -> list[dict]:
  events: list[dict] = []
  try:
    lines = path.read_text(encoding="utf-8").splitlines()
  except OSError:
    return events
  for line in lines:
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict):
      events.append(event)
  return events


def _fold_task_state(events: list[dict]) -> str:
  """The task state the raw event history folds to: the last lifecycle fact wins."""
  state = "open"
  for event in events:
    etype = event.get("type")
    if etype == "task_closed":
      state = str(event.get("outcome") or "completed")
    elif etype == "task_reopened":
      state = "open"
  return state


def cmd_snapshot(out: Path) -> None:
  base = preflight(require_server=True)  # the old server must be the one running
  sessions_dir = _sessions_dir()
  migrate: list[str] = []
  task_parents: dict[str, str | None] = {}
  for session_dir in sorted(sessions_dir.iterdir()):
    meta_path = session_dir / "metadata.json"
    if not session_dir.is_dir() or not meta_path.exists():
      continue
    try:
      meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
      print(f"snapshot: skipping unreadable {meta_path}: {exc}", file=sys.stderr)
      continue
    if not isinstance(meta, dict) or not meta.get("profile"):
      continue  # a legacy session is not a task node
    session_id = str(meta.get("id") or session_dir.name)
    task_parents[session_id] = meta.get("task_parent_id")
    events = _read_jsonl(session_dir / "data" / "chat_events.jsonl")
    archives = session_dir / "data" / "archives"
    if archives.is_dir():
      for segment in sorted(archives.glob("*.jsonl")):
        events.extend(_read_jsonl(segment))
    state = _fold_task_state(events)
    hidden = meta.get("presentation") == "hidden"
    legacy_archived = meta.get("status") == "archived"
    if state == "open" and (hidden or legacy_archived):
      migrate.append(session_id)
  doc = {
      "kind": SNAPSHOT_KIND,
      "sessions_dir": str(sessions_dir),
      "base_url": base,
      "migrate": migrate,
      "task_parents": task_parents,
  }
  out.write_text(json.dumps(doc, indent=2) + "\n", encoding="utf-8")
  print(f"snapshot: {len(migrate)} task node(s) to archive, {len(task_parents)} task node(s) recorded -> {out}")


def _sessions_dir() -> Path:
  from src.core.home import charliebot_home_dir
  path = charliebot_home_dir() / "sessions"
  if not path.is_dir():
    sys.exit(f"snapshot source missing: no sessions directory at {path}")
  return path


# ---------------------------------------------------------------------------
# apply: after the restart, through the new archive route
# ---------------------------------------------------------------------------


def cmd_apply(source: Path) -> None:
  base = preflight(require_server=True)
  doc = json.loads(source.read_text(encoding="utf-8"))
  if doc.get("kind") != SNAPSHOT_KIND:
    sys.exit(f"apply: {source} is not a {SNAPSHOT_KIND} snapshot")
  key = _access_key()
  mismatches: list[str] = []
  for session_id in doc.get("migrate", []):
    status, body = _request(base, key, "DELETE", f"/api/sessions/{session_id}")
    if status != 200:
      mismatches.append(f"{session_id}: archive call answered {status}: {body}")
      continue
    status, detail = _request(base, key, "GET", f"/api/sessions/{session_id}")
    if status != 200:
      mismatches.append(f"{session_id}: read-back answered {status}")
      continue
    state = detail.get("task_state")
    if state != "archived":
      mismatches.append(f"{session_id}: read-back task_state is {state!r}, expected 'archived'")
  for session_id, recorded_parent in doc.get("task_parents", {}).items():
    status, detail = _request(base, key, "GET", f"/api/sessions/{session_id}")
    if status != 200:
      mismatches.append(f"{session_id}: parent read-back answered {status}")
      continue
    actual_parent = detail.get("task_parent_id")
    if actual_parent != recorded_parent:
      mismatches.append(f"{session_id}: parent moved: recorded {recorded_parent!r}, read back {actual_parent!r}")
  if mismatches:
    for line in mismatches:
      print(f"MISMATCH: {line}")
    sys.exit(f"apply: {len(mismatches)} mismatch(es); the migration set needs attention")
  print(
      f"apply: {len(doc.get('migrate', []))} node(s) archived, "
      f"{len(doc.get('task_parents', {}))} parent(s) verified, no mismatch")


# ---------------------------------------------------------------------------
# rollback: before the code revert, through the new unarchive route
# ---------------------------------------------------------------------------


def _walk_task_nodes(base: str, key: str) -> list[dict]:
  """Every task-tree row (the keyset-paginated tree, roots then subtrees)."""
  rows: list[dict] = []

  def fetch(parent_id: str | None) -> list[dict]:
    out: list[dict] = []
    cursor: str | None = None
    while True:
      query = "include_archived=true&limit=500"
      if parent_id is not None:
        query += f"&parent_id={parent_id}"
      if cursor is not None:
        query += f"&cursor={urllib.request.quote(cursor, safe='')}"
      status, page = _request(base, key, "GET", f"/api/sessions/tree?{query}")
      if status != 200:
        sys.exit(f"rollback: tree page answered {status}: {page}")
      out.extend(page.get("items", []))
      cursor = page.get("next_cursor")
      if cursor is None:
        return out

  seen: set[str] = set()
  queue = [row["id"] for row in fetch(None)]
  while queue:
    node_id = queue.pop(0)
    if node_id in seen:
      continue
    seen.add(node_id)
    children = fetch(node_id)
    rows.extend(children)
    queue.extend(row["id"] for row in children)
  return rows


def cmd_rollback() -> None:
  base = preflight(require_server=True)
  key = _access_key()
  rows = _walk_task_nodes(base, key)
  targets = [row["id"] for row in rows if row.get("task_state") == "archived"]
  restored: list[str] = []
  for session_id in targets:
    status, body = _request(base, key, "POST", f"/api/sessions/{session_id}/unarchive")
    if status != 200:
      print(f"rollback: unarchive of {session_id} answered {status}: {body}", file=sys.stderr)
      sys.exit(1)
    restored.extend(body.get("restored", []) if isinstance(body, dict) else [])
  print(f"rollback: {len(targets)} archived node(s) unarchived; {len(set(restored))} node(s) restored in total")


def main() -> None:
  parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
  sub = parser.add_subparsers(dest="command", required=True)

  snap = sub.add_parser("snapshot", help="List the migration set while the old server runs (read-only)")
  snap.add_argument("--out", required=True, type=Path, help="Where the snapshot JSON is written")

  apply_ = sub.add_parser("apply", help="Archive the snapshot's nodes after the restart, then verify")
  apply_.add_argument("--in", dest="source", required=True, type=Path, help="The snapshot JSON to apply")

  sub.add_parser("rollback", help="Unarchive every task node whose last close outcome is archived")

  args = parser.parse_args()
  if args.command == "snapshot":
    cmd_snapshot(args.out)
  elif args.command == "apply":
    cmd_apply(args.source)
  elif args.command == "rollback":
    cmd_rollback()


if __name__ == "__main__":
  main()

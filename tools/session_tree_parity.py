#!/usr/bin/env python3
"""Compare the session tree's rows and work states against origin/main on an isolated session copy.

The copy hard-links up to 128 live sessions with their ancestors and fences them against writes. Two children
run in separate systemd scopes under charliebot-pressure-test.slice: origin/main exported with git archive, then
this worktree. Each child reads every tree page (the roots and every node's children, with and without archived
rows) and every node's session detail, then writes the answers to JSON. The parent compares the answers field by
field.

The live server appends to hard-linked histories in place, so a node's files can move while the comparison runs.
A node is left out when its files, or those of a node in its subtree, differ between any two snapshots: one
before the first side and one after each side. Rows and details under a left-out node are not compared.
"""

from __future__ import annotations

import argparse
import asyncio
import datetime
import fcntl
import io
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile
import uuid
from typing import Any

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
  sys.path.insert(0, str(REPO_ROOT))

from tools import server_pressure_probe as probe  # noqa: E402

DEFAULT_SESSION_COUNT = 64
DEFAULT_SEED = 1792795452
SIDE_MEMORY_MAX = "3G"
SIDE_SWAP_MAX = "512M"
RESULTS_NAME = "session_tree_parity"
LOCK_PATH = probe.DEFAULT_RESULTS_DIR / ".probe.lock"
DETAIL_FIELDS = ("work_state", "task_state", "archived")
BUILT_EXTENSION_SOURCES = ("src/runtime/agent_process/_vfkspawn.c",)
ROOT_KEY = "<root>"
MISSING = object()


def _page_key(parent: str | None, include_archived: bool) -> str:
  return f"{parent or ROOT_KEY}|include_archived={include_archived}"


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
  parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
  parser.add_argument("--origin-ref", default="origin/main", help="revision the worktree is compared against")
  parser.add_argument("--session-count", type=int, default=DEFAULT_SESSION_COUNT)
  parser.add_argument("--seed", type=int, default=DEFAULT_SEED, help="stable selector seed")
  parser.add_argument("--reset-selection", action="store_true", help="select a new deterministic session subset")
  parser.add_argument("--results-dir", type=pathlib.Path, default=probe.DEFAULT_RESULTS_DIR / RESULTS_NAME)
  args = parser.parse_args(argv)
  if not 1 <= args.session_count <= probe.MAX_COPIED_SESSIONS:
    raise ValueError(f"--session-count must be between 1 and {probe.MAX_COPIED_SESSIONS}")
  return args


def _export_ref(ref: str, dest: pathlib.Path) -> pathlib.Path:
  """Extract the tree of *ref* into *dest* so a child imports that revision's src package.

  git archive leaves out built extensions. Each one listed in BUILT_EXTENSION_SOURCES is copied from this
  checkout's build, and only when its C source matches *ref*, so the binary is the one that ref's source builds.
  """
  dest.mkdir(parents=True, exist_ok=False)
  archive = subprocess.run(
      ["git", "archive", "--format=tar", ref], cwd=REPO_ROOT, check=True, capture_output=True).stdout
  with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
    tar.extractall(dest, filter="data")
  for source in BUILT_EXTENSION_SOURCES:
    unchanged = subprocess.run(["git", "diff", "--quiet", ref, "--", source], cwd=REPO_ROOT, check=False)
    if unchanged.returncode != 0:
      raise RuntimeError(f"{source} differs from {ref}; its built extension would not match that revision")
    source_path = REPO_ROOT / source
    built = list(source_path.parent.glob(f"{source_path.stem}.*.so"))
    if len(built) != 1:
      raise RuntimeError(f"expected one built extension for {source}, found {len(built)}")
    shutil.copy2(built[0], dest / source_path.parent.relative_to(REPO_ROOT) / built[0].name)
  return dest


def _worktree_dirty() -> bool:
  result = subprocess.run(
      ["git", "status", "--porcelain", "--untracked-files=no"], cwd=REPO_ROOT, check=True, capture_output=True)
  return bool(result.stdout.strip())


def _revision(ref: str) -> str:
  result = subprocess.run(["git", "rev-parse", "--verify", ref], cwd=REPO_ROOT, check=True, capture_output=True)
  return result.stdout.decode().strip()


def _session_signature(session_dir: pathlib.Path) -> tuple[tuple[str, int, int, int], ...]:
  """Inode, size and mtime of every file under one session: a change to any of them moves the signature."""
  entries = []
  for root, _dirs, files in os.walk(session_dir):
    for name in files:
      path = pathlib.Path(root) / name
      st = path.stat()
      entries.append((str(path.relative_to(session_dir)), st.st_ino, st.st_size, st.st_mtime_ns))
  return tuple(sorted(entries))


def _snapshot(sessions_dir: pathlib.Path, node_ids: list[str]) -> dict[str, tuple]:
  return {node: _session_signature(sessions_dir / node) for node in node_ids}


def _subtree(node: str, children: dict[str, list[str]]) -> set[str]:
  out: set[str] = set()
  stack = [node]
  while stack:
    current = stack.pop()
    if current in out:
      continue
    out.add(current)
    stack.extend(children.get(current, []))
  return out


def _side_command(side: str, code_root: pathlib.Path, copy: dict[str, Any], out_dir: pathlib.Path) -> list[str]:
  home = copy["home"]
  manifest = copy["copy_root"] / "copy_manifest.json"
  return [
      shutil.which("systemd-run") or "systemd-run",
      "--user",
      "--scope",
      "--collect",
      "--quiet",
      f"--unit=session-tree-parity-{side}-{uuid.uuid4().hex[:8]}",
      f"--slice={probe.PRESSURE_SLICE}",
      f"--property=MemoryMax={SIDE_MEMORY_MAX}",
      f"--property=MemorySwapMax={SIDE_SWAP_MAX}",
      f"--working-directory={code_root}",
      f"--setenv=PATH={probe.SERVER_PATH}",
      "--setenv=PYTHONDONTWRITEBYTECODE=1",
      f"--setenv=CHARLIEBOT_HOME={home}",
      "--",
      sys.executable,
      str(REPO_ROOT / "tools" / "session_tree_parity.py"),
      "_side",
      "--code-root",
      str(code_root),
      "--home",
      str(home),
      "--linked-manifest",
      str(manifest),
      "--out",
      str(out_dir / f"{side}.json"),
  ]


def _run_side(side: str, code_root: pathlib.Path, copy: dict[str, Any], out_dir: pathlib.Path) -> dict[str, Any]:
  out_dir.mkdir(parents=True, exist_ok=True)
  command = _side_command(side, code_root, copy, out_dir)
  result = subprocess.run(command, cwd=code_root, capture_output=True, text=True, check=False)
  if result.returncode != 0:
    raise RuntimeError(f"{side} side failed with exit {result.returncode}:\n{result.stderr[-4000:]}")
  return json.loads((out_dir / f"{side}.json").read_text(encoding="utf-8"))


def _compare_fields(origin: dict[str, Any], worktree: dict[str, Any], fields: tuple[str, ...] | None) -> list[dict]:
  differences = []
  names = fields if fields is not None else tuple(sorted(set(origin) | set(worktree)))
  for name in names:
    left = origin.get(name, MISSING)
    right = worktree.get(name, MISSING)
    if left != right:
      differences.append({"field": name, "origin": repr(left), "worktree": repr(right)})
  return differences


def _compare(
    origin: dict[str, Any],
    worktree: dict[str, Any],
    left_out: set[str],
) -> dict[str, Any]:
  nodes = origin["nodes"]
  children = {}
  for page in origin["pages"].values():
    if page["include_archived"]:
      children[page["parent"] or ROOT_KEY] = [row["id"] for row in page["items"]]
  pages_compared = pages_left_out = rows_compared = rows_left_out = membership_differences = 0
  row_differences: list[dict] = []
  for key in sorted(origin["pages"]):
    left_page = origin["pages"][key]
    right_page = worktree["pages"][key]
    parent = left_page["parent"]
    scope = set(nodes) if parent is None else _subtree(parent, children)
    if scope & left_out:
      pages_left_out += 1
      rows_left_out += len(left_page["items"])
      continue
    pages_compared += 1
    left_ids = [row["id"] for row in left_page["items"]]
    right_ids = [row["id"] for row in right_page["items"]]
    if left_ids != right_ids or left_page["next_cursor"] != right_page["next_cursor"]:
      membership_differences += 1
      row_differences.append({"page": key, "origin_ids": left_ids, "worktree_ids": right_ids})
      continue
    for left_row, right_row in zip(left_page["items"], right_page["items"], strict=True):
      rows_compared += 1
      row_differences.extend(
          {
              "page": key,
              "id": left_row["id"],
              **difference
          } for difference in _compare_fields(left_row, right_row, None))
  details_compared = details_left_out = detail_differences = 0
  for node in sorted(nodes):
    if _subtree(node, children) & left_out:
      details_left_out += 1
      continue
    details_compared += 1
    for difference in _compare_fields(origin["details"][node], worktree["details"][node], DETAIL_FIELDS):
      row_differences.append({"detail": node, **difference})
      detail_differences += 1
  differing_fields = len(row_differences)
  compared_nodes = [node for node in nodes if node not in left_out]
  return {
      "nodes":
          {
              "total": len(nodes),
              "compared": len(compared_nodes),
              "left_out": len(left_out),
              "left_out_ids": sorted(left_out),
          },
      "pages": {
          "compared": pages_compared,
          "left_out": pages_left_out
      },
      "rows": {
          "compared": rows_compared,
          "left_out": rows_left_out,
          "membership_differences": membership_differences,
      },
      "details": {
          "compared": details_compared,
          "left_out": details_left_out,
          "differing_fields": detail_differences,
      },
      "differing_fields": differing_fields,
      "differences": row_differences[:50],
  }


def _run(args: argparse.Namespace) -> dict[str, Any]:
  selected = probe._select_ids(args.results_dir, args.session_count, args.seed, args.reset_selection)
  run_id = f"{datetime.datetime.now(datetime.UTC).strftime('%Y%m%dT%H%M%SZ')}_{uuid.uuid4().hex[:8]}"
  scratch = args.results_dir / ".scratch" / run_id
  scratch.mkdir(parents=True)
  try:
    copy_root = scratch / "copy"
    copy = probe.build_session_copy(
        copy_root=copy_root,
        source_sessions=probe.LIVE_SESSIONS,
        selected_ids=selected,
        event_byte_limit=int(probe.MAX_LIVE_EVENT_GIB * probe.GIB),
    )
    copy["copy_root"] = copy_root
    probe.write_minimal_config(copy["home"], port=0)
    sessions_dir = copy["home"] / "sessions"
    node_ids = list(copy["session_ids"])
    snapshots = [_snapshot(sessions_dir, node_ids)]
    origin_code = _export_ref(args.origin_ref, scratch / "origin")
    outputs = {}
    for side, code_root in (("origin", origin_code), ("worktree", REPO_ROOT)):
      outputs[side] = _run_side(side, code_root, copy, scratch / "out")
      snapshots.append(_snapshot(sessions_dir, node_ids))
    changed = {node for node in node_ids if len({snap[node] for snap in snapshots}) > 1}
    report = _compare(outputs["origin"], outputs["worktree"], changed)
    report.update(
        {
            "created_at": datetime.datetime.now(datetime.UTC).isoformat(),
            "origin_ref": args.origin_ref,
            "origin_revision": _revision(args.origin_ref),
            "worktree_revision": _revision("HEAD"),
            "worktree_uncommitted": _worktree_dirty(),
            "copy":
                {
                    "session_count": len(node_ids),
                    "linked_file_count": copy["linked_file_count"],
                    "live_event_bytes": copy["live_event_bytes"],
                },
            "sides": {
                side: out["process"] for side, out in outputs.items()
            },
        })
    return report
  finally:
    shutil.rmtree(scratch)


def main(argv: list[str] | None = None) -> None:
  if argv is None:
    argv = sys.argv[1:]
  if argv[:1] == ["_side"]:
    _side_main(argv[1:])
    return
  args = _parse_args(argv)
  args.results_dir.mkdir(parents=True, exist_ok=True)
  with LOCK_PATH.open("w", encoding="utf-8") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    probe._assert_pressure_preflight()
    report = _run(args)
  report_name = f"session_tree_parity_{args.origin_ref.replace('/', '_')}.json"
  probe.flush_json(args.results_dir / report_name, report)
  print(json.dumps({key: report[key] for key in ("nodes", "pages", "rows", "details", "differing_fields")}, indent=2))
  if report["differing_fields"]:
    raise SystemExit(1)


def _side_main(argv: list[str]) -> None:
  """One side's child process: read the tree through this side's code and write the answers to JSON."""
  parser = argparse.ArgumentParser()
  parser.add_argument("--code-root", type=pathlib.Path, required=True)
  parser.add_argument("--home", type=pathlib.Path, required=True)
  parser.add_argument("--linked-manifest", type=pathlib.Path, required=True)
  parser.add_argument("--out", type=pathlib.Path, required=True)
  args = parser.parse_args(argv)
  if pathlib.Path(os.environ["CHARLIEBOT_HOME"]).resolve() != args.home.resolve():
    raise RuntimeError("CHARLIEBOT_HOME does not match the selected copy")
  probe._assert_agent_clis_off_path()
  linked = [
      pathlib.Path(value) for value in json.loads(args.linked_manifest.read_text(encoding="utf-8"))["linked_files"]
  ]
  landlock = probe.apply_landlock_write_allowlist([args.home / "state", args.out.parent])
  probe.assert_linked_files_unwritable(linked)
  blocked_child_attempts = probe._install_child_process_block()
  sys.path.insert(0, str(args.code_root))
  from src.app import registrations
  from src.runtime import task_execution

  if not pathlib.Path(task_execution.__file__).resolve().is_relative_to(args.code_root.resolve()):
    raise RuntimeError("src was imported from outside the side's code root")
  registrations.register_all()
  tree = task_execution.task_manager()
  pages, details, nodes = asyncio.run(_collect(tree))
  attempts = blocked_child_attempts()
  if attempts:
    raise RuntimeError(f"tree read attempted child processes: {attempts}")
  payload = {
      "nodes": nodes,
      "pages": pages,
      "details": details,
      "process": {
          "landlock": landlock,
          "blocked_child_attempts": attempts,
          "code_root": str(args.code_root),
      },
  }
  args.out.write_text(json.dumps(payload, default=str, sort_keys=True), encoding="utf-8")


async def _collect(tree: Any) -> tuple[dict[str, dict], dict[str, dict], list[str]]:
  """Every page of the tree and every node's detail, discovered from the roots down."""
  pages: dict[str, dict] = {}
  nodes: list[str] = []
  seen: set[str] = set()
  frontier: list[str | None] = [None]
  while frontier:
    parent = frontier.pop()
    for include_archived in (False, True):
      page = await tree.tree_page(
          parent_id=parent, include_archived=include_archived, limit=probe.MAX_COPIED_SESSIONS, cursor=None)
      pages[_page_key(parent, include_archived)] = {
          "parent": parent,
          "include_archived": include_archived,
          "items": page["items"],
          "next_cursor": page["next_cursor"],
      }
    for row in pages[_page_key(parent, include_archived=True)]["items"]:
      if row["id"] not in seen:
        seen.add(row["id"])
        nodes.append(row["id"])
        frontier.append(row["id"])
  details: dict[str, dict] = {}
  for node in nodes:
    detail = await tree.session_detail(node)
    details[node] = {field: detail[field] for field in DETAIL_FIELDS}
  return pages, details, nodes


if __name__ == "__main__":
  main()

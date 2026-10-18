"""Python reference for the Claude Code usage source (tests/claude_usage_cross_impl.rs).

One JSON document arrives on stdin and one leaves on stdout. Each mode runs one step of the Python
usage source on synthetic inputs: ``read`` a list of transcripts, ``transform`` usage-panel responses,
or ``sweep`` a synthetic tree in a dry run and then a real run. The process points HOME,
CLAUDE_CONFIG_DIR and CHARLIEBOT_HOME at the synthetic root before importing src, so no real login
directory is read.
"""

import datetime
import json
import os
import pathlib
import sys

DATA = json.load(sys.stdin)
ROOT = pathlib.Path(DATA["root"])
os.environ["HOME"] = str(ROOT)
os.environ["CLAUDE_CONFIG_DIR"] = str(ROOT / "claude-home")
os.environ["CHARLIEBOT_HOME"] = str(ROOT / "home")
PROTOCOL_STDOUT = sys.stdout
sys.stdout = sys.stderr

from src.backends import claude_code
from src.backends.claude_code import usage_logs, usage_quota, usage_sweep
from src.infra import config
from src.runtime.hooks import usage_sources

claude_code.register()


def record_json(record: usage_sources.UsageRecord) -> dict:
  return {
      "record_id": record.record_id,
      "kind": record.kind.value,
      "source": record.source,
      "model": record.model,
      "account": record.account,
      "ts": record.ts,
      "in_fresh": record.in_fresh,
      "cache_write": record.cache_write,
      "cache_read": record.cache_read,
      "output": record.output,
      "in_unsplit": record.in_unsplit,
      "sessions": list(record.sessions),
  }


def read_mode(data: dict) -> dict:
  cases = []
  for case in data["cases"]:
    signature, records = usage_logs.read(pathlib.Path(case["path"]), case["account"], case["previous"])
    cases.append({"signature": signature, "records": [record_json(record) for record in records]})
  return {"cases": cases}


def transform_mode(data: dict) -> dict:
  return {"cases": [usage_quota._transform_response(case["raw"], account=case["account"]) for case in data["cases"]]}


def tree_bytes(root: pathlib.Path) -> dict[str, bytes]:
  return {str(path.relative_to(root)): path.read_bytes() for path in sorted(root.rglob("*")) if path.is_file()}


def build_sweep_tree(spec: dict) -> dict[str, pathlib.Path]:
  """One transcript directory per transcript entry; returns each directory by entry name."""
  now = datetime.datetime.fromisoformat(spec["now"])
  for sid in spec["session_dirs"]:
    (ROOT / "home" / "sessions" / sid).mkdir(parents=True, exist_ok=True)
  for name in spec["worktrees"]:
    (ROOT / "worktrees" / name).mkdir(parents=True, exist_ok=True)
  for name in spec["trashed"]:
    (ROOT / "worktrees" / ".trash" / name).mkdir(parents=True, exist_ok=True)
  trees = {"default": ROOT / ".claude" / "projects", "config": ROOT / "claude-home" / "projects"}
  directories = {}
  for entry in spec["transcripts"]:
    encoded = usage_sweep.claude_project_dir_name(ROOT / entry["cwd"])
    directory = trees[entry["tree"]] / encoded
    files = {"transcript.jsonl": b"claude-transcript", "subagents/agent.jsonl": b"sub!"}
    stamp = (now - datetime.timedelta(days=entry["age_days"])).timestamp()
    for relative, body in files.items():
      path = directory / relative
      path.parent.mkdir(parents=True, exist_ok=True)
      path.write_bytes(body)
      os.utime(path, (stamp, stamp))
    directories[entry["name"]] = directory
  return directories


def sweep_result(result: usage_sources.SourceSweep, directories: dict[str, pathlib.Path]) -> dict:
  return {
      "categories":
          [
              {
                  "name": category.name,
                  "unit": category.unit,
                  "count": category.count,
                  "bytes": category.bytes
              } for category in result.categories
          ],
      "exists": {
          name: directory.is_dir() for name, directory in directories.items()
      },
  }


def sweep_mode(data: dict) -> dict:
  spec = data["spec"]
  directories = build_sweep_tree(spec)
  cfg = config.CharlieBotConfig.model_validate(
      {
          "charliebot_home": str(ROOT / "home"),
          "paths": {
              "worktree_dir": str(ROOT / "worktrees")
          },
          "backends": {
              "options": [{
                  "id": "fable",
                  "label": "Fable",
                  "type": "cc-claude",
                  "model": "claude-fable-5-1"
              }]
          },
      })
  facts = {
      session["id"]: usage_sources.SessionFacts(id=session["id"], cold=session["cold"], cc_session_id=None)
      for session in spec["sessions"]
  }
  now = datetime.datetime.fromisoformat(spec["now"])

  def scope(dry_run: bool) -> usage_sources.SweepScope:
    return usage_sources.SweepScope(
        cfg=cfg,
        now=now,
        dry_run=dry_run,
        session_id=None,
        facts=facts,
        references={},
        orphan_idle_days=spec["orphan_idle_days"],
        vacuum=False,
        force=False,
    )

  before = tree_bytes(ROOT)
  dry = usage_sweep.sweep(scope(True))
  dry_summary = sweep_result(dry, directories)
  dry_untouched = tree_bytes(ROOT) == before
  real = usage_sweep.sweep(scope(False))
  return {"dry_untouched": dry_untouched, "dry": dry_summary, "real": sweep_result(real, directories)}


MODES = {"read": read_mode, "transform": transform_mode, "sweep": sweep_mode}

PROTOCOL_STDOUT.write(json.dumps(MODES[sys.argv[1]](DATA), ensure_ascii=False))

"""Measure opencode turns on the shared 2.0 server: cold start, warm new directory, warm same directory.

usage: python scripts/opencode_turn_bench.py --runs <n> [--model <provider/model>] [--gap-seconds <s>]
                                              [--repo <git repo dir>]

Runs against the profile named by CHARLIEBOT_HOME (it refuses to run unset or on the default profile).
Each run is one cold turn (the server stopped first), one warm turn in a new directory, and one warm
turn in that same directory again. Latency is the time from the turn's start to the model request
(the session.step.started event's data.started), in milliseconds. Real-model turns are spaced by
--gap-seconds (default 60) because the free model refuses rapid requests. server_rss_mb.started is
read from a server started alone before any turn. With --repo, 16 new subdirectories of that git
repo each run one turn after the measurement, and rss_after_new_dirs_mb holds the server RSS 5 s
after each of those turns. Prints a summary table to stderr and the JSON result to stdout.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import pathlib
import re
import sys
import tempfile
import time

from src.backends.opencode import opencode, server
from src.infra import constants, home

DEFAULT_MODEL = "opencode/deepseek-v4.1-flash-free"
PROMPT = "Reply with the single word OK."
NEW_DIR_COUNT = 16
# Seconds between a turn's end and the RSS reading after it: the release has run by then.
RSS_SETTLE_SECONDS = 5


def _profile_guard() -> pathlib.Path:
  """The profile home this run measures; exits when CHARLIEBOT_HOME is unset or names the default profile."""
  if not os.environ.get(home.CHARLIEBOT_HOME_ENV, "").strip():
    sys.exit(f"opencode_turn_bench: {home.CHARLIEBOT_HOME_ENV} must name a non-default profile")
  if home.charliebot_home_dir() == home.default_charliebot_home():
    sys.exit("opencode_turn_bench: CHARLIEBOT_HOME resolves to the default profile; use an isolated profile")
  return home.charliebot_home_dir()


def _tree_rss_mb(pid: int) -> float:
  """Resident memory of *pid* and its descendants, in MiB, read from /proc."""
  pids, todo = [], [pid]
  while todo:
    current = todo.pop()
    pids.append(current)
    for task in os.listdir(f"/proc/{current}/task"):
      with open(f"/proc/{current}/task/{task}/children") as f:
        todo += [int(child) for child in f.read().split()]
  total_kb = 0
  for current in pids:
    with open(f"/proc/{current}/status") as f:
      total_kb += int(re.search(r"VmRSS:\s+(\d+)", f.read()).group(1))
  return round(total_kb / 1024, 1)


def _turn_env() -> dict[str, str]:
  """The environment a turn gets, minus any run identity this shell happens to carry."""
  env = {
      key: value
      for key, value in os.environ.items()
      if key not in (constants.RUN_TOKEN_ENV, constants.SESSION_ID_ENV_VAR)
  }
  env.setdefault("PATH", "/usr/bin:/bin")
  return env


async def _turn(model: str, directory: pathlib.Path) -> tuple[dict, str]:
  """One turn: its model-request latency in ms, and its outcome."""
  backend = opencode.OpenCodeBackend(model=model)
  started_ms = time.time() * 1000
  events = [event async for event in backend.run(PROMPT, str(directory), _turn_env())]
  latency = None if backend.model_request_started_ms is None else round(backend.model_request_started_ms - started_ms)
  if events and events[-1]["type"] == "result":
    outcome = "succeeded"
  elif events and events[-1]["type"] == "error" and "interrupted" in events[-1]["message"]:
    outcome = "interrupted"
  else:
    outcome = "failed"
  return {
      "latency_ms": latency,
      "outcome": outcome,
      "error": events[-1].get("message") if outcome != "succeeded" else None
  }, outcome


async def _measure(runs: int, model: str, gap_seconds: float, work: pathlib.Path, repo: pathlib.Path | None) -> dict:
  shared = server.server_for(None)
  result: dict = {
      "opencode_version": None,
      "model": model,
      "cold_ms": [],
      "warm_new_dir_ms": [],
      "warm_same_dir_ms": [],
      "server_rss_mb": {
          "started": None,
          "after_first_turn": None,
          "after_all_turns": None
      },
      "rss_after_new_dirs_mb": None,
      "turn_outcomes": []
  }
  outcomes: list[str] = []
  first_turn = True
  pending_gap = False

  async def paced_turn(directory: pathlib.Path) -> dict:
    nonlocal first_turn, pending_gap
    if pending_gap:
      await asyncio.sleep(gap_seconds)
    turn, outcome = await _turn(model, directory)
    outcomes.append(outcome)
    pending_gap = True
    if first_turn:
      result["server_rss_mb"]["after_first_turn"] = _tree_rss_mb(shared.pid)
      first_turn = False
    if turn["error"]:
      print(f"turn failed: {turn['error']}", file=sys.stderr)
    return turn

  await shared.ensure_running()
  result["server_rss_mb"]["started"] = _tree_rss_mb(shared.pid)
  result["opencode_version"] = shared.version
  await shared.stop()

  for run in range(runs):
    await shared.stop()
    cold_dir = work / f"cold{run}"
    cold_dir.mkdir(parents=True)
    cold = await paced_turn(cold_dir)
    result["cold_ms"].append(cold["latency_ms"])
    new_dir = work / f"warm{run}"
    new_dir.mkdir(parents=True)
    warm_new = await paced_turn(new_dir)
    result["warm_new_dir_ms"].append(warm_new["latency_ms"])
    warm_same = await paced_turn(new_dir)
    result["warm_same_dir_ms"].append(warm_same["latency_ms"])
  result["server_rss_mb"]["after_all_turns"] = _tree_rss_mb(shared.pid)
  result["turn_outcomes"] = outcomes
  if repo is not None:
    readings: list[float] = []
    # The directories live inside the repo, so each is a new location of a real git repository.
    with tempfile.TemporaryDirectory(prefix="opencode-turn-bench-dirs-", dir=repo) as new_root:
      for index in range(NEW_DIR_COUNT):
        directory = pathlib.Path(new_root) / f"d{index:02d}"
        directory.mkdir()
        await paced_turn(directory)
        await asyncio.sleep(RSS_SETTLE_SECONDS)
        readings.append(_tree_rss_mb(shared.pid))
    result["rss_after_new_dirs_mb"] = readings
  await shared.stop()
  return result


def _summary(result: dict) -> str:
  rows = [
      ("cold start", result["cold_ms"]), ("warm, new dir", result["warm_new_dir_ms"]),
      ("warm, same dir", result["warm_same_dir_ms"])
  ]
  lines = [f"{'turn':<16}{'model request, ms per run'}"]
  lines += [f"{name:<16}{values}" for name, values in rows]
  rss = result["server_rss_mb"]
  lines.append(
      f"server RSS MiB: started {rss['started']}, after first turn {rss['after_first_turn']}, "
      f"after all turns {rss['after_all_turns']}")
  if result["rss_after_new_dirs_mb"] is not None:
    lines.append(f"server RSS MiB after each new dir: {result['rss_after_new_dirs_mb']}")
  lines.append(f"outcomes: {result['turn_outcomes']}")
  return "\n".join(lines)


def main() -> None:
  parser = argparse.ArgumentParser(description="Measure opencode turns on the shared 2.0 server.")
  parser.add_argument("--runs", type=int, required=True, help="runs; each run is one cold and two warm turns")
  parser.add_argument("--model", default=DEFAULT_MODEL, help="provider/model used by every turn")
  parser.add_argument("--gap-seconds", type=float, default=60.0, help="pause between real-model turns")
  parser.add_argument("--repo", type=pathlib.Path, help="git repo whose new subdirectories each run one more turn")
  args = parser.parse_args()
  if args.runs < 1:
    sys.exit("opencode_turn_bench: --runs must be at least 1")
  if args.repo is not None and not (args.repo / ".git").exists():
    sys.exit(f"opencode_turn_bench: --repo {args.repo} is not a git repository")
  _profile_guard()
  with tempfile.TemporaryDirectory(prefix="opencode-turn-bench-") as scratch:
    result = asyncio.run(_measure(args.runs, args.model, args.gap_seconds, pathlib.Path(scratch), args.repo))
  print(_summary(result), file=sys.stderr)
  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()

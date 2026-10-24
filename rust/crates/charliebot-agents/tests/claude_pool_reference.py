import datetime
import json
import os
import pathlib
import sys
import types

DATA = json.load(sys.stdin)
os.environ["HOME"] = DATA["home"]
os.environ["CHARLIEBOT_HOME"] = DATA["home"]
PROTOCOL_STDOUT = sys.stdout
sys.stdout = sys.stderr

from src.backends import claude_code

claude_code.register()

from src.backends.claude_code import claude_accounts, claude_metadata, claude_relay, master_cc_relay
from src.infra import config, models
from src.runtime import master_cc_state


def reset_claude_account_readings():
  """Drop the process-local Claude account readings, as tests/conftest.py does at d1f7e3ba6."""
  claude_accounts._event_readings.clear()
  claude_accounts._panel_readings.clear()
  claude_accounts._auth_failures.clear()
  claude_accounts._login_notified.clear()


def make_config(data):
  return config.CharlieBotConfig.model_validate({
      "charliebot_home": data["home"],
      "backends": {
          "options": [{
              "id": "fable",
              "label": "Fable",
              "type": "cc-claude",
              "model": "claude-fable-5-1",
              "account_pool": "alpha",
          }]
      },
      "accounts": {
          "claude": data["accounts"],
          "claude_pools": data["pools"],
      },
  })


def moment(value):
  return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))


def apply_readings(case, now):
  for reading in case.get("readings", []):
    claude_accounts.observe_rate_limit(reading["label"], reading["info"], now)
  for label in case.get("auth_failures", []):
    claude_accounts.record_auth_failure(label, now)


def selection(data):
  cfg = make_config(data)
  output = []
  for case in data["cases"]:
    reset_claude_account_readings()
    master_cc_state._current_items.clear()
    now = moment(case["now"])
    apply_readings(case, now)
    meta = models.SessionMetadata(profile="manager", id="observer", name="Observer")
    claude_metadata.set_account(meta, case.get("current"))
    for index, label in enumerate(case.get("busy", [])):
      other = models.SessionMetadata(profile="manager", id=f"busy-{index}", name="Busy")
      claude_metadata.set_account(other, label)
      master_cc_state._current_items[other.id] = types.SimpleNamespace(session_meta=other)
    last_request = moment(case["last_request"]) if case.get("last_request") else None
    chosen, cold = master_cc_relay.choose_turn_account(
        cfg, meta, "claude-fable-5-1", last_request, now, case.get("pool"))
    output.append({
        "chosen": chosen.label if chosen else None,
        "cold": cold,
        "unavailable": claude_accounts.unavailable_detail(cfg, now, case.get("pool")),
    })
  return output


def snapshot(root):
  files = {}
  for path in sorted(root.rglob("*")):
    if path.is_file():
      files[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
  return files


def transcript_case(root, case):
  src_root = root / case["src"]
  dst_root = root / case["dst"]
  src = src_root / "projects" / "work" / f"{case['id']}.jsonl"
  dst = dst_root / "projects" / "work" / f"{case['id']}.jsonl"
  src.parent.mkdir(parents=True, exist_ok=True)
  if case.get("src_content") is not None:
    src.write_text(case["src_content"], encoding="utf-8")
    if case.get("src_mtime") is not None:
      os.utime(src, ns=(case["src_mtime"], case["src_mtime"]))
    if case.get("src_sidecar"):
      sidecar = src.with_suffix("") / "tool-results"
      sidecar.mkdir(parents=True, exist_ok=True)
      (sidecar / "result.txt").write_text(case["src_sidecar"], encoding="utf-8")
  if case.get("dst_content") is not None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.write_text(case["dst_content"], encoding="utf-8")
    if case.get("dst_mtime") is not None:
      os.utime(dst, ns=(case["dst_mtime"], case["dst_mtime"]))
    if case.get("dst_only"):
      sidecar = dst.with_suffix("") / "tool-results"
      sidecar.mkdir(parents=True, exist_ok=True)
      (sidecar / "retained.txt").write_text(case["dst_only"], encoding="utf-8")
  try:
    claude_accounts.move_transcript(case["id"], src_root, dst_root)
    error = None
  except claude_accounts.TranscriptMoveError as exc:
    error = str(exc).replace(str(root), "<root>")
  return {"error": error, "files": snapshot(root)}


def moves(data):
  root = pathlib.Path(data["root"])
  root.mkdir(parents=True, exist_ok=True)
  return [transcript_case(root / case["name"], case) for case in data["cases"]]


def relay(data):
  cfg = make_config(data)
  output = []
  root = pathlib.Path(data["root"])
  for index, case in enumerate(data["cases"]):
    reset_claude_account_readings()
    now = moment(case["now"])
    apply_readings(case, now)
    watch = claude_relay.RelayWatch("a", "claude-fable-5-1", cfg, "alpha", now)
    stop = [watch.observe(event) for event in case["events"]]
    wants_next = watch.wants_next(case["exit_code"], case.get("stderr", ""))
    moved = None
    if wants_next:
      current = next(account for account in claude_accounts.pool(cfg) if account.label == "a")
      session_id = case.get("session_id")
      if session_id:
        transcript = pathlib.Path(current.config_dir) / "projects" / "work" / f"{session_id}.jsonl"
        transcript.parent.mkdir(parents=True, exist_ok=True)
        transcript.write_text("synthetic transcript\n", encoding="utf-8")
      result = claude_relay.relay_move(
          cfg, "claude-fable-5-1", current, session_id, now, "alpha")
      moved = {
          "account": result.account.label if result.account else None,
          "error": result.error,
          "refused_holder": result.refused_holder.label if result.refused_holder else None,
          "pool_exhausted": result.pool_exhausted,
      }
    output.append({"stop": stop, "decision": watch.decided, "move": moved})
  return output


def main():
  data = DATA
  mode = sys.argv[1]
  if mode == "selection":
    result = selection(data)
  elif mode == "moves":
    result = moves(data)
  elif mode == "relay":
    result = relay(data)
  elif mode == "renderer":
    from src.backends.claude_code import turn_contribution

    result = turn_contribution.CONTRIBUTION.event_renderers()[data["event"]["type"]](data["event"])
  else:
    raise ValueError(f"unknown mode {mode}")
  json.dump(result, PROTOCOL_STDOUT, sort_keys=True)


if __name__ == "__main__":
  main()

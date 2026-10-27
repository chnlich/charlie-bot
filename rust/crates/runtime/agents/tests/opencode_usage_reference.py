"""Python reference for the opencode usage source (tests/opencode_usage_cross_impl.rs).

One JSON document arrives on stdin and one leaves on stdout. Each step runs ``usage_logs.read`` on one synthetic
store with the previous signature the step names. The process points HOME and CHARLIEBOT_HOME at the synthetic root
before importing src, so no real opencode store is read.
"""

import json
import os
import pathlib
import sys

DATA = json.load(sys.stdin)
ROOT = pathlib.Path(DATA["root"])
os.environ["HOME"] = str(ROOT)
os.environ["CHARLIEBOT_HOME"] = str(ROOT / "home")
PROTOCOL_STDOUT = sys.stdout
sys.stdout = sys.stderr

from src.backends.opencode import usage_logs


def record_json(record) -> dict:
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


def steps_result(data: dict) -> dict:
  steps = []
  for step in data["steps"]:
    signature, records = usage_logs.read(pathlib.Path(step["path"]), step["account"], step["previous"])
    steps.append({"signature": signature, "records": [record_json(record) for record in records]})
  return {"steps": steps}


PROTOCOL_STDOUT.write(json.dumps(steps_result(DATA), ensure_ascii=False))

"""Record fake-service traffic so parity steps can compare the requests each server made."""

import base64
import copy
import json
import pathlib
import threading
from typing import Any


def _json_safe(value: Any) -> Any:
  if isinstance(value, bytes):
    return {"$bytes_base64": base64.b64encode(value).decode("ascii")}
  if isinstance(value, list):
    return [_json_safe(item) for item in value]
  if isinstance(value, dict):
    return {key: _json_safe(item) for key, item in value.items()}
  return value


class Recorder:
  """Append structured request and WebSocket records to one side's kept log."""

  def __init__(self, path: pathlib.Path) -> None:
    self.path = path
    self._records: list[dict] = []
    self._lock = threading.Lock()
    path.write_bytes(b"")

  def record(self, record: dict) -> dict:
    with self._lock:
      stored = {"_index": len(self._records), **record}
      self._records.append(stored)
      with self.path.open("a", encoding="utf-8") as output:
        output.write(json.dumps(_json_safe(stored), ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n")
      return copy.deepcopy(stored)

  def mark(self) -> int:
    with self._lock:
      return len(self._records)

  def snapshot(self) -> list[dict]:
    with self._lock:
      return copy.deepcopy(self._records)

  def since(self, mark: int) -> list[dict]:
    with self._lock:
      return copy.deepcopy(self._records[mark:])

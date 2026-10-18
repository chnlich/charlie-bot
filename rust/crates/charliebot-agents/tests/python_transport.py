"""Python half of the shared process-transport wire-format test."""

import asyncio
import json
import pathlib
import sys

from src.runtime import runs
from src.runtime.agent_process import base


async def _follow(raw_path: pathlib.Path, cursor_path: pathlib.Path, start_offset: int, stop_after: int) -> None:
  events: list[dict] = []

  async for event in base.tail_follow_events(
      raw_path,
      translate=lambda raw: [raw],
      is_alive=lambda: stop_after > 0 and len(events) < stop_after,
      cursor=cursor_path,
      start_offset=start_offset,
      post_result_timeout=10.0,
      buffer_limit=base.DEFAULT_BUFFER_LIMIT,
      poll_interval=0.001,
  ):
    events.append(event)
  print(json.dumps(events, separators=(",", ":")))


def _main() -> None:
  command, *args = sys.argv[1:]
  if command == "cursor_write":
    path, offset = args
    writer = runs.RawCursorWriter(pathlib.Path(path))
    writer.write(int(offset))
    writer.close()
  elif command == "cursor_read":
    (path,) = args
    print(runs.read_raw_cursor(pathlib.Path(path)))
  elif command == "follow":
    path, cursor, offset, stop_after = args
    cursor_path = pathlib.Path(cursor)
    start_offset = runs.read_raw_cursor(cursor_path) if offset == "cursor" else int(offset)
    asyncio.run(_follow(pathlib.Path(path), cursor_path, start_offset, int(stop_after)))
  elif command == "pid_start":
    (pid,) = args
    stat = runs.read_pid_stat(int(pid))
    if stat is None:
      raise RuntimeError(f"process {pid} has no stat")
    print(stat[0])
  else:
    raise ValueError(f"unknown command {command}")


if __name__ == "__main__":
  _main()

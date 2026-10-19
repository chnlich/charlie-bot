"""Write and read synthetic trigger files through Python's model and atomic writer."""

from __future__ import annotations

import asyncio
import datetime
import json
import pathlib
import sys

from src.infra import json_utils, models


def fixed_time() -> datetime.datetime:
  return datetime.datetime(2026, 1, 2, 3, 4, 5, 123456, tzinfo=datetime.UTC)


def records() -> list[models.PendingTrigger]:
  stamp = fixed_time()
  return [
      models.PendingTrigger(id="synthetic-delay-pending", session_id="synthetic-session", fire_at=stamp, created_at=stamp,
                            message="delay", status=models.TriggerStatus.PENDING),
      models.PendingTrigger(id="synthetic-local-fired", session_id="synthetic-session", fire_at=stamp, created_at=stamp,
                            message="local", status=models.TriggerStatus.FIRED, fired_at=stamp, fire_reason="completed",
                            watch_targets=[models.LocalPid(pid=123, exit_file="/synthetic/exit", log="/synthetic/log")]),
      models.PendingTrigger(id="synthetic-remote-cancelled", session_id="synthetic-session", fire_at=stamp, created_at=stamp,
                            message="remote", status=models.TriggerStatus.CANCELLED, fire_reason="target task is archived",
                            watch_targets=[models.RemotePid(host="synthetic-host", pid=456)]),
      models.PendingTrigger(id="synthetic-local-pending", session_id="synthetic-session", fire_at=stamp, created_at=stamp,
                            message="local pending", watch_targets=[models.LocalPid(pid=124)]),
      models.PendingTrigger(id="synthetic-remote-pending", session_id="synthetic-session", fire_at=stamp, created_at=stamp,
                            message="remote pending", watch_targets=[models.RemotePid(host="synthetic-host", pid=457)]),
      models.PendingTrigger(id="synthetic-slurm-pending", session_id="synthetic-session", fire_at=stamp, created_at=stamp,
                            message="slurm pending", watch_targets=[models.SlurmJob(host="synthetic-host", job_id=789)]),
      models.PendingTrigger(id="synthetic-local-slurm-pending", session_id="synthetic-session", fire_at=stamp, created_at=stamp,
                            message="local slurm pending", watch_targets=[models.SlurmJob(job_id=790)]),
  ]


async def write_records(directory: pathlib.Path) -> None:
  directory.mkdir(parents=True, exist_ok=True)
  for trigger in records():
    await json_utils.write_model_json_atomically(directory / f"{trigger.id}.json", trigger)


def read_records(directory: pathlib.Path) -> list[dict]:
  loaded = [models.PendingTrigger.model_validate_json(path.read_text(encoding="utf-8"))
            for path in sorted(directory.glob("*.json"))]
  return [trigger.model_dump(mode="json") for trigger in loaded]


def main() -> None:
  action = sys.argv[1]
  directory = pathlib.Path(sys.argv[2])
  if action == "write":
    asyncio.run(write_records(directory))
    output = read_records(directory)
  elif action == "read":
    output = read_records(directory)
  else:
    raise ValueError(f"unknown action: {action}")
  print(json.dumps(output, ensure_ascii=False, separators=(",", ":")))


if __name__ == "__main__":
  main()

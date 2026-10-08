"""The trigger file walk: the stat listing of one session's trigger files.

The trigger manager (``src/runtime/triggers.py``) and the sidebar probe (``src/runtime/sessions.py``)
read the same files, so both import the one walk from here.
"""

import os
from pathlib import Path


def iter_trigger_file_stats(triggers_dir: str | Path) -> list[tuple[str, os.stat_result]]:
  """(path, stat) pairs for the regular ``*.json`` trigger files under *triggers_dir*.

  The one scandir+stat walk every trigger read shares: the list memo of ``TriggerManager``
  and the sidebar probe's verdict scan (src.runtime.sessions). Raises OSError when
  *triggers_dir* itself cannot be scanned — that verdict belongs to the caller.
  A file that vanishes between scandir and stat is skipped, the same "nothing
  to read" verdict every stat failure earns. Paths are scandir's plain strings,
  and the stat rides ``DirEntry.stat`` — this scan runs per poll.
  """
  pairs: list[tuple[str, os.stat_result]] = []
  with os.scandir(triggers_dir) as entries:
    for entry in entries:
      if not entry.name.endswith(".json") or not entry.is_file():
        continue
      try:
        st = entry.stat()
      except OSError:
        continue  # vanished between scandir and stat — nothing to read
      pairs.append((entry.path, st))
  return pairs

"""free_disk_gib reads the real filesystem; the suite's autouse stand-in answers every other test."""
import shutil

import pytest

from src.runtime import task_execution

# Collection imports this module before any fixture runs, so this binding is the real reader.
REAL_FREE_DISK_GIB = task_execution.free_disk_gib


def test_stand_in_answers_and_real_reader_reads_the_nearest_existing_ancestor(tmp_path):
  missing = tmp_path / "not-yet" / "created"
  assert task_execution.free_disk_gib(missing) == 1024.0
  assert REAL_FREE_DISK_GIB is not task_execution.free_disk_gib
  assert REAL_FREE_DISK_GIB(missing) == pytest.approx(shutil.disk_usage(tmp_path).free / 1024**3, abs=0.5)

"""Deterministic tests for atomic metadata.json writes.

``save_metadata`` must make each update to a session's ``metadata.json``
atomically visible to concurrent readers: a reader always observes one complete
document, the old one or the new one, never an empty or partial file. Each write
uses an exclusive temp file in the target directory followed by ``os.replace``.

These tests lock behaviour, not implementation shape: the discriminating
assertions kill wrong implementations (in-place truncation, or a shared temp
name) rather than asserting on the temp file's name. A test that passes on both
the broken and fixed write side is not evidence, so each assertion targets a
specific wrong implementation.
"""

import asyncio
import threading
from pathlib import Path
from unittest.mock import patch

import pytest
from conftest import (
    JSON_UTILS_OS_REPLACE_PATCH_TARGET,
    OPUS_BACKEND_ID,
    REAL_OS_REPLACE,
    make_os_replace_spy,
    make_read_at_os_replace,
)
from conftest import make_session_mgr as _make_session_mgr

from src.core.models import SessionMetadata

_REAL_REPLACE = REAL_OS_REPLACE


@pytest.mark.asyncio
async def test_atomic_write_swaps_target_via_os_replace(tmp_path: Path) -> None:
  """The swap goes through os.replace on the target metadata.json path.

  A write side that never calls os.replace (an in-place truncating write-through)
  fails here because the hook never fires against the target -- killing the
  vacuous pass where the hook is never reached.
  """
  mgr = _make_session_mgr(tmp_path)
  meta = SessionMetadata(name="seed", backend=OPUS_BACKEND_ID)
  await mgr.save_metadata(meta)
  target = mgr._metadata_path(meta.id)

  replaced_targets: list[str] = []

  with patch(JSON_UTILS_OS_REPLACE_PATCH_TARGET, side_effect=make_os_replace_spy(replaced_targets)):
    updated = meta.model_copy()
    updated.name = "changed"
    await mgr.save_metadata(updated)

  assert str(target) in replaced_targets


@pytest.mark.asyncio
async def test_atomic_read_observes_previous_document_at_swap(tmp_path: Path) -> None:
  """At the instant of the swap the target still holds the previous document.

  Reading the target at that moment must yield the old complete document -- not
  empty and not a parse failure -- proving the previous inode stays intact right
  up to the rename. An in-place truncating write side would read empty here.
  """
  mgr = _make_session_mgr(tmp_path)
  meta = SessionMetadata(name="before", backend=OPUS_BACKEND_ID)
  await mgr.save_metadata(meta)
  target = mgr._metadata_path(meta.id)

  read_at_swap: list[str] = []

  with patch(JSON_UTILS_OS_REPLACE_PATCH_TARGET, side_effect=make_read_at_os_replace(read_at_swap, target)):
    updated = meta.model_copy()
    updated.name = "after"
    await mgr.save_metadata(updated)

  assert len(read_at_swap) == 1
  raw = read_at_swap[0]
  assert raw.strip() != ""
  assert raw.strip().startswith("{")
  assert '"before"' in raw


class _InterleaveController:
  """Coordinate two concurrent save_metadata writes into the defect window.

  The first write's replace is held until the second write has fully completed,
  forcing the second write to run entirely between the first write's temp-file
  write and its replace -- precisely the order that breaks a shared temp name.
  """

  def __init__(self) -> None:
    self._lock = threading.Lock()
    self._call_count = 0
    self._second_done = threading.Event()

  def replace(self, src: str, dst: str) -> None:
    with self._lock:
      self._call_count += 1
      call_n = self._call_count
    if call_n == 1:
      assert self._second_done.wait(timeout=5), "second write never completed"
      return _REAL_REPLACE(src, dst)
    result = _REAL_REPLACE(src, dst)
    self._second_done.set()
    return result


@pytest.mark.asyncio
async def test_two_concurrent_writes_both_return_and_target_stays_complete(tmp_path: Path) -> None:
  """Two writes to the same session interleave; both must return normally.

  With a shared temp name the delayed first replacer would raise
  FileNotFoundError because its source was moved away by the second write, so
  ``both return normally`` is the discriminating assertion -- a mere "target is
  complete" check would not catch it.
  """
  mgr = _make_session_mgr(tmp_path)
  meta = SessionMetadata(name="seed", backend=OPUS_BACKEND_ID)
  await mgr.save_metadata(meta)
  target = mgr._metadata_path(meta.id)

  first = meta.model_copy()
  first.name = "first"
  second = meta.model_copy()
  second.name = "second"

  harness = _InterleaveController()

  def _coordinated_replace(src: str, dst: str) -> None:
    return harness.replace(src, dst)

  with patch(JSON_UTILS_OS_REPLACE_PATCH_TARGET, side_effect=_coordinated_replace):
    results = await asyncio.gather(
        mgr.save_metadata(first),
        mgr.save_metadata(second),
        return_exceptions=True,
    )

  assert all(res is None for res in results), f"writes should both succeed, got {results!r}"
  raw = target.read_text(encoding="utf-8")
  assert raw.strip() != ""
  assert raw.strip().startswith("{")

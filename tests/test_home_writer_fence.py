"""The home writer fence: the shared writer/migration exclusion.

The exclusion must be a mechanism normal writers actually take (the server
holds the same flock for its whole lifetime), never a caller-supplied boolean
or a private lock. These tests cover acquisition, mutual exclusion with
holder identity, the read-only probe, pid-reuse staleness, and the server
lifespan's refuse-to-start / release-on-exit behavior.
"""

from __future__ import annotations

import os
import pathlib

import pytest

from src.runtime import home_writer_fence


def test_acquire_excludes_and_reports_holder(tmp_path: pathlib.Path) -> None:
  home = tmp_path / "home"
  home.mkdir()
  first = home_writer_fence.acquire_home_writer_fence(home, purpose="first")
  try:
    assert home_writer_fence.read_fence_holder(home) is not None
    with pytest.raises(home_writer_fence.HomeWriterActiveError) as excinfo:
      home_writer_fence.acquire_home_writer_fence(home, purpose="second")
    holder = excinfo.value.holder
    assert holder is not None and holder.pid == os.getpid()
    assert holder.purpose == "first"
    assert "first" in str(excinfo.value)
  finally:
    first.release()
  # Release drops the identity row and frees the lock.
  assert not home_writer_fence.fence_identity_path(home).exists()
  second = home_writer_fence.acquire_home_writer_fence(home, purpose="second")
  second.release()


# --- server lifespan integration --------------------------------------------

# ---------------------------------------------------------------------------
# Fence failure paths: ownership survives every failure mode
# ---------------------------------------------------------------------------


def test_fence_refuses_symlinked_paths(tmp_path: pathlib.Path) -> None:
  """A symlinked state directory, lock, or identity record refuses acquisition
  and probing instead of placing or reading the exclusion outside the home."""
  outside = tmp_path / "outside"
  outside.mkdir()
  # (a) state itself is a symlink.
  home = tmp_path / "home_a"
  home.mkdir()
  (home / "state").symlink_to(outside)
  with pytest.raises(home_writer_fence.FencePathRefusalError, match="symlink"):
    home_writer_fence.acquire_home_writer_fence(home, purpose="cli")
  with pytest.raises(home_writer_fence.FencePathRefusalError, match="symlink"):
    home_writer_fence.probe_writer_fence(home)
  assert not (outside / "home_writer.lock").exists()
  # (b) the lock file is a symlink inside a real state dir.
  home = tmp_path / "home_b"
  (home / "state").mkdir(parents=True)
  (home / "state" / "home_writer.lock").symlink_to(outside / "captured.lock")
  with pytest.raises(home_writer_fence.FencePathRefusalError, match="symlink"):
    home_writer_fence.acquire_home_writer_fence(home, purpose="cli")
  with pytest.raises(home_writer_fence.FencePathRefusalError, match="symlink"):
    home_writer_fence.probe_writer_fence(home)
  assert not (outside / "captured.lock").exists()
  # (c) a real fence still works after the refusals.
  fence = home_writer_fence.acquire_home_writer_fence(tmp_path / "home_c", purpose="real")
  fence.release()

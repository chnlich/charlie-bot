"""Tests for the shared gc-off boundary.

The boundary is what keeps a bulk build from leaving collection off for the
server's remaining lifetime: whatever the enclosed span does — return, raise,
cancel — the exit path re-enables GC, and whether it also collects is decided
by the caller's `collect` flag.
"""

import gc
from collections.abc import Iterator
from contextlib import contextmanager
from typing import Any
from unittest.mock import patch

import pytest

from src.core.gc_control import gc_off


@contextmanager
def _counting_collect() -> Iterator[list[int]]:
  """Swap gc.collect for a counting delegate; yields the one-slot counter list."""
  counter = [0]
  real_collect = gc.collect

  def counting_collect(*args: Any, **kwargs: Any) -> int:
    counter[0] += 1
    return real_collect(*args, **kwargs)

  with patch.object(gc, "collect", counting_collect):
    yield counter


def test_gc_off_reenables_and_collects_when_asked() -> None:
  with _counting_collect() as collects:
    with gc_off(collect=True):
      assert not gc.isenabled()
    assert gc.isenabled()
  assert collects == [1]


def test_gc_off_skips_collect_when_flag_is_false() -> None:
  with _counting_collect() as collects:
    with gc_off(collect=False):
      assert not gc.isenabled()
    assert gc.isenabled()
  assert collects == [0]


def test_gc_off_reenables_when_the_span_raises() -> None:
  with pytest.raises(RuntimeError, match="boom"), gc_off(collect=False):
    assert not gc.isenabled()
    raise RuntimeError("boom")
  assert gc.isenabled()


def test_counting_collect_helper_sees_real_calls() -> None:
  with _counting_collect() as collects:
    gc.collect()
  assert collects == [1]

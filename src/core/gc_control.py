"""The gc-off span shared by the server's bulk builds.

A span that disables GC runs it process-wide (GC is global, not per-thread),
so every caller runs on a bounded span and the re-enable path fires on every
exit. Whether that path also collects is a per-caller fact: a build whose
churn is garbage afterwards collects to reclaim the cyclic leftovers now;
a span whose allocations stay referenced lets the next natural cycle take
them and passes collect=False.
"""

from __future__ import annotations

import gc
from collections.abc import Iterator
from contextlib import contextmanager


@contextmanager
def gc_off(collect: bool) -> Iterator[None]:
  """Run the enclosed span with GC disabled, re-enable on every exit path."""
  gc.disable()
  try:
    yield
  finally:
    gc.enable()
    if collect:
      gc.collect()

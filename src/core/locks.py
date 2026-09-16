"""The one get-or-create definition for the per-key asyncio locks.

The get and the create share one event-loop tick: with no await between
them, two coroutines asking for the same key cannot each construct a lock
and then serialize on different ones.
"""

import asyncio
from collections.abc import Hashable
from typing import TypeVar

K = TypeVar("K", bound=Hashable)


def lock_for(locks: dict[K, asyncio.Lock], key: K) -> asyncio.Lock:
  """Return the lock registered under *key*, registering a new one on first use."""
  lock = locks.get(key)
  if lock is None:
    lock = asyncio.Lock()
    locks[key] = lock
  return lock

"""Bounded LRU memo shared by the request-path caches.

The one definition of the sibling-memo rule the per-route caches cite
(git diff, annotate): callers reach the memo from executor threads, so
get+touch and store+evict each hold the lock — a concurrent insert's cap
eviction must not pop a key between a hit's dict lookup and its
move_to_end.
"""

import threading
from collections import OrderedDict
from typing import Generic, Hashable, TypeVar

K = TypeVar("K", bound=Hashable)
V = TypeVar("V")


class BoundedMemo(Generic[K, V]):
  """A locked, LRU-capped key -> value map.

  ``get`` returns the value under *key*, or None on a miss, and refreshes
  the key's recency on a hit; ``store`` inserts, refreshes recency, and
  evicts the least-recently-used entries past the limit. A stored value
  must not be None — get cannot distinguish it from a miss. Values are
  handed back uncopied and unsynchronized: the caller owns the contract
  that a served value is immutable, or treated read-only.
  """

  def __init__(self, limit: int) -> None:
    self._limit = limit
    self._entries: OrderedDict[K, V] = OrderedDict()
    self._lock = threading.Lock()

  def get(self, key: K) -> V | None:
    """Return the value under *key*, or None on a miss; a hit refreshes recency."""
    with self._lock:
      entry = self._entries.get(key)
      if entry is not None:
        self._entries.move_to_end(key)
      return entry

  def store(self, key: K, value: V) -> None:
    """Store *value* under *key* and evict past the limit."""
    with self._lock:
      self._entries[key] = value
      self._entries.move_to_end(key)
      while len(self._entries) > self._limit:
        self._entries.popitem(last=False)

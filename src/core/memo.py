"""Bounded LRU memo shared by the request-path caches.

The one definition of the sibling-memo rule the per-route caches cite
(git diff, annotate): callers reach the memo from executor threads, so
get+touch and store+evict each hold the lock — a concurrent insert's cap
eviction must not pop a key between a hit's dict lookup and its
move_to_end.
"""

import threading
from collections import OrderedDict
from collections.abc import Callable, Iterator
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

  def drop_where(self, predicate: Callable[[K], bool]) -> None:
    """Evict every entry whose key satisfies *predicate*, in one lock hold.

    The scan and the deletions share one hold, so a concurrent store cannot
    re-add a matching key between the snapshot and the drops.
    """
    with self._lock:
      for key in [key for key in self._entries if predicate(key)]:
        del self._entries[key]

  def drop(self, key: K) -> None:
    """Remove *key*'s entry when present; a no-op otherwise."""
    with self._lock:
      self._entries.pop(key, None)

  def clear(self) -> None:
    """Remove every entry (the tests' cross-test pollution reset)."""
    with self._lock:
      self._entries.clear()

  def __contains__(self, key: object) -> bool:
    """Return whether *key* has a resident entry (the tests' structural assertions)."""
    with self._lock:
      return key in self._entries

  def __iter__(self) -> Iterator[K]:
    """Yield the keys, least-recently-used first, from one consistent snapshot."""
    with self._lock:
      return iter(list(self._entries))

  def __len__(self) -> int:
    """Return the number of resident entries."""
    with self._lock:
      return len(self._entries)

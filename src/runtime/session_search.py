"""Session search: the sidebar's name and chat-content search, and the memos that keep its repeats cheap.

``SessionSearch.search_sessions_readonly`` matches names over every status and scans the active sessions' chat files
for the query. A chat file's proven absence or presence rides a per-file memo, and a query's match result rides a
per-query memo, so a growing query string re-reads a file only after the file moves. Each row's sidebar state comes
from the sidebar block. The process builds one block (``search()``); tests build their own and install it with
``set_search()``.
"""

import asyncio
import io
from pathlib import Path

from src.infra.config import CharlieBotConfig, get_config
from src.infra.log_once import LazyStructlogLogger, WarnOnceRegistry
from src.infra.memo import BoundedMemo
from src.infra.models import SessionMetadata, SessionStatus
from src.runtime import session_events, session_sidebar, session_store

log = LazyStructlogLogger()

_SEARCH_RESULT_LIMIT = 200  # newest rows a name/content search returns; keeps the render bounded
# LRU cap on the content-search miss memo: chat-file path -> {proven-absent
# lowercase needle -> the (mtime_ns, size, ino) the absence was proven at}.
# Absence of N proves every superstring of N absent while the file keeps that
# signature, so the debounced sidebar's growing query string re-reads a file
# only after the file moves. One slot per file is not enough: the shortest
# needle ever searched would occupy it forever, and every query family outside
# its superstrings would re-scan the whole active corpus on every request (the
# whole corpus, ~155 MB on the seed host, per search). Chat files mutate only
# by append between atomic archive rewrites (inode swap), so a same-inode file
# that grew kept its old bytes: a query some stored needle prefixes re-proves
# absence by scanning the appended tail alone. The cap must cover the active
# chat-file population the derive walks, not a sample of it: a derive under
# churn re-reads every candidate without a current root from byte 0 (the
# appended tail alone otherwise), so a cap under the population turns every
# metadata write or append between requests into a near-full corpus scan.
# ~1090 active chat files at the 2026-10-04 re-pricing; the entry is one path
# string plus at most _SEARCH_MISS_ROOTS_PER_FILE needle->signature records,
# so the cap bounds memo memory at a few MB.
_SEARCH_MISS_MEMO_LIMIT = 4096
# Roots kept per file, LRU: needle families beyond the cap degrade to a scan
# for the evicted family only, the same cost the one-slot form paid for every
# non-dominant family.
_SEARCH_MISS_ROOTS_PER_FILE = 8
# The hit side's bounds mirror the absence side's: one proven-present root per
# query family per file, the file map LRU-bounded. A stored needle's hit
# answers its substrings without a read (the miss side answers superstrings),
# because the hit's bytes sit in the prefix the scan read and same-inode
# growth only appends past it. The file cap prices the same population the
# miss side's cap prices: an evicted hit root re-reads the file from byte 0
# on the next derive.
_SEARCH_HIT_MEMO_LIMIT = 4096
_SEARCH_HIT_ROOTS_PER_FILE = 8
# LRU cap on the per-query match-result memo: lowered query -> the derived rows
# plus the freshness ground it was derived from. A correction keystroke re-fires
# a small set of queries (the prefix family it edits plus the alternations it
# switches between), so the cap covers one typing burst; an evicted query
# re-derives at the pre-memo cost, the same degradation the miss/hit memo
# bounds set.
_SEARCH_MATCH_MEMO_LIMIT = 8
# str.lower()/bytes.translate and substring search hold the GIL for the whole
# input, so the sidebar content search reads chat files in windows of this many
# characters (the decoded path) or bytes (the raw path): each window's GIL hold
# stays bounded instead of scaling with the chat file's size, which would stall
# the event loop for every other request.
_SEARCH_CHUNK_SIZE = 1 << 18
# Chat files whose classification demanded bytes ride one thread-pool task per
# this many files: the hand-off (context copy, queue round-trip) is per-task
# work the pool's threads serialize on, and a batch keeps the one-read-per-
# worker overlap the per-file shape already had.
_SEARCH_SCAN_BATCH_FILES = 8
# The raw-byte scan's case fold: A-Z to a-z, every other byte identity. UTF-8
# never encodes a non-ASCII codepoint below 0x80, so for an ASCII needle this
# fold sees exactly the ASCII letters the decoded text's str.lower() sees,
# except U+212A and U+0130, whose str.lower() contains an ASCII letter — those
# two stay on the decoded path's side of the boundary the scan docstring states.
_ASCII_LOWER = bytes.maketrans(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ", b"abcdefghijklmnopqrstuvwxyz")

# Search scans take the failed-read path for every active session whose live
# chat file cannot be read (a fresh session's data/ stays empty until its
# first event), and each scan reports the same failure again — one line per
# search request per stuck file — while only the first sighting of a reported
# (session, error) pair carries information.
_SEARCH_READ_FAILURES_SEEN = WarnOnceRegistry()


def _log_search_read_failed_once(session_id: str, error: OSError) -> None:
  """Log one search_read_failed per (session, error) per process.

  A caller relies on at most one line per (session_id, error): a search round
  that sees the same session's same failure re-fires a fired alarm, and the
  key is exactly the fields the line logs, so a swapped failure (path or
  errno changed) earns one new line and nothing outside the log statement
  drifts the key away from what was reported.
  """
  _SEARCH_READ_FAILURES_SEEN.log(
      log.debug, "search_read_failed", (session_id, str(error)), session_id=session_id, error=str(error))


def _absence_rescan_start(
    roots: tuple[tuple[str, tuple[int, int, int]], ...],
    sig: tuple[int, int, int],
    query_lower: str,
) -> int | None:
  """Start offset for the absence re-scan of one chat file, or None when no read is needed.

  None when a stored root's signature equals the file's current stat: the
  root's absence proof covers every query it extends, so the file answers
  without a read and without a fresh memo entry. Otherwise the largest
  same-inode growth base among the roots the query extends gives the smallest
  re-proof window (0 = full scan; a shrunken or inode-swapped file has no
  base). A query occurrence crossing the old size starts at most 4 bytes per
  char earlier, hence the seek window; +8 covers decode resync at the offset.
  """
  for _needle, root_sig in roots:
    if query_lower.startswith(_needle) and root_sig == sig:
      return None  # proven-absent memo verdict: no file read
  best_size = -1
  for needle, root_sig in roots:
    if not query_lower.startswith(needle):
      continue
    old_size, old_ino = root_sig[1], root_sig[2]
    if old_ino == sig[2] and sig[1] > old_size > best_size:
      best_size = old_size
  if best_size < 0:
    return 0
  return max(0, best_size - (4 * len(query_lower) + 8))


def _hit_root_covers(
    roots: tuple[tuple[str, tuple[int, int, int]], ...],
    sig: tuple[int, int, int],
    query_lower: str,
) -> bool:
  """True when a stored hit root answers the query without a file read.

  A stored needle's hit covers the needle's substrings: the hit's bytes sit in
  the prefix the scan read, and same-inode growth only appends past it, so the
  hit persists while the inode holds and the file has not shrunk below the
  scanned size — the same signature convention the absence roots'
  append-window re-proof runs on. A shrink or an inode swap re-scans.
  """
  for needle, root_sig in roots:
    if query_lower in needle and root_sig[2] == sig[2] and sig[1] >= root_sig[1]:
      return True
  return False


def _search_content_sigs_hold(
    paths: list[Path],
    stored: tuple[tuple[int, int, int] | None, ...],
) -> bool:
  """True when every candidate chat file still carries the derived-from signature.

  A None entry prices a path whose stat failed at derivation time: the file
  must still be missing for the stored rows to stand. Any other outcome — an
  append, a rewrite, a rotation's inode swap, a file that appeared where the
  derivation saw none — moves the key, because a content verdict is proven
  against bytes, not against metadata the listings memo watches.
  """
  for path, sig in zip(paths, stored, strict=True):
    try:
      stat = path.stat()
    except OSError:
      if sig is not None:
        return False
      continue
    if sig is None or (stat.st_mtime_ns, stat.st_size, stat.st_ino) != sig:
      return False
  return True


def _scan_content_for_hit(path: Path, session_id: str, query_lower: str, start: int) -> bool | None:
  """Window scan of a chat-events file for *query_lower* (thread-pool work).

  *start* is a byte offset the scan begins at; the caller uses it to re-scan
  only the bytes appended after a proven-absent prefix. An ASCII query rides
  raw bytes: UTF-8 never encodes a non-ASCII codepoint below 0x80, so the
  ``_ASCII_LOWER`` fold matches exactly what the decoded text's lower() sees,
  except U+212A and U+0130, whose str.lower() contains an ASCII letter — those
  two need the decoded path. A non-ASCII query decodes as before: *start* can
  split a UTF-8 sequence, so a nonzero start decodes with ``errors="replace"``
  and the verdict equals a full strict scan's for every file a full scan can
  decode; a zero start keeps strict decoding, where a corrupt file fails loud
  as before. Returns the verdict, or None when the file could not be read: an
  errored scan proves no absence, so the caller must not memoize it as a miss.
  """
  overlap = len(query_lower) - 1
  # Every window carries the last *overlap* units of the previous one, so a
  # hit straddling a read boundary lies whole inside exactly one window.
  try:
    with path.open("rb") as raw:
      if start:
        raw.seek(start)
      if query_lower.isascii():
        needle = query_lower.encode("ascii")
        tail = b""
        while True:
          chunk = raw.read(_SEARCH_CHUNK_SIZE)
          if not chunk:
            return False
          window = tail + chunk.translate(_ASCII_LOWER)
          if needle in window:
            return True
          tail = window[-overlap:] if overlap else b""
      with io.TextIOWrapper(raw, encoding="utf-8", errors="replace" if start else "strict") as stream:
        tail = ""
        while True:
          chunk = stream.read(_SEARCH_CHUNK_SIZE)
          if not chunk:
            return False
          window = tail + chunk.lower()
          if query_lower in window:
            return True
          tail = window[-overlap:] if overlap else ""
  except OSError as e:
    _log_search_read_failed_once(session_id, e)
    return None


class SessionSearch:
  """Session search over the session metadata store, the events block and the sidebar block."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      store: session_store.SessionStore,
      events: session_events.SessionEvents,
      sidebar: session_sidebar.SessionSidebar,
  ) -> None:
    self._cfg = cfg
    self._store = store
    self._events = events
    self._sidebar = sidebar
    self._search_miss_memo: BoundedMemo[str, BoundedMemo[str, tuple[int, int,
                                                                    int]]] = BoundedMemo(_SEARCH_MISS_MEMO_LIMIT)
    self._search_hit_memo: BoundedMemo[str, BoundedMemo[str, tuple[int, int,
                                                                   int]]] = BoundedMemo(_SEARCH_HIT_MEMO_LIMIT)
    self._search_match_memo: BoundedMemo[str, tuple[list[SessionMetadata], list[Path],
                                                    tuple[tuple[int, int, int] | None, ...],
                                                    list[SessionMetadata]]] = BoundedMemo(_SEARCH_MATCH_MEMO_LIMIT)

  async def search_sessions(
      self,
      query: str,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> list[SessionMetadata]:
    """Search sessions by name (every status) and chat event content (active only), case-insensitive.

    Returns at most ``_SEARCH_RESULT_LIMIT`` rows, newest first: the cap keeps
    the render bounded when a short query matches thousands of archived names.
    The rows are owned copies (thinking-stamped, sidebar-state applied) for
    callers that mutate or hand them on; read-only consumers call
    :meth:`search_sessions_readonly`.
    """
    rows, derived = await self.search_sessions_readonly(
        query,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )
    sessions = [session_store.stamp_thinking_since(row.model_copy()) for row in rows]
    session_sidebar.apply_sidebar_state(sessions, derived, include_running_status, include_pending_trigger_status)
    return sessions

  async def search_sessions_readonly(
      self,
      query: str,
      include_running_status: bool,
      include_pending_trigger_status: bool,
  ) -> tuple[list[SessionMetadata], dict[str, dict]]:
    """Search sessions by name (every status) and chat event content (active only), case-insensitive.

    Returns ``(rows, derived)`` for consumers that only read: rows are the
    shared cached metadata references, newest first, at most
    ``_SEARCH_RESULT_LIMIT`` of them (the caller must not mutate them), and
    derived maps each row's id to the sidebar-state fields for the requested
    include flags. The cap applies to the sorted name matches before any row
    work: 200 newer-or-equal matches always outrank a name match below the
    cap line, and a content hit can only displace rows at the line from
    above, so the dropped matches can never reach the returned rows.

    The match result (which rows, before any sidebar-state overlay) memoizes
    per lowered query on the ground the result derives from: the listings
    memo's list identity for the names, and each content candidate's chat-file
    signature for the scans — a chat file moves without touching any
    metadata.json, so the listing identity alone cannot vouch for it. An
    errored scan stores nothing, keeping the retry-per-request rule
    ``_scan_content_for_hit`` documents.
    """
    query_lower = query.lower()
    all_meta = await self._store.load_session_metas()
    cached = self._search_match_memo.get(query_lower)
    if (cached is not None and cached[0] is all_meta and _search_content_sigs_hold(cached[1], cached[2])):
      derived = await self._sidebar.resolve_sidebar_state(
          cached[3],
          include_running_status=include_running_status,
          include_pending_trigger_status=include_pending_trigger_status,
      )
      return cached[3], derived
    # One pass lowers every name once: the name-match test and the content-scan
    # candidate split read the same lowered string, and a name hit is by
    # definition no content-scan candidate.
    matches: list[SessionMetadata] = []
    content_candidates: list[tuple[SessionMetadata, Path]] = []
    for meta in all_meta:
      if query_lower in meta.name.lower():
        matches.append(meta)
      elif meta.status == SessionStatus.ACTIVE:
        content_candidates.append((meta, self._events.get_chat_events_path(meta.id)))
    matches.sort(key=lambda meta: meta.updated_at, reverse=True)

    # Classification runs on the event loop: one hot stat per candidate plus a
    # memo lookup measures ~0.1 ms for the whole set, while the same checks as
    # per-file executor round-trips measured ~2.4 ms each under the server's
    # pool churn (the default executor's ~cpu+4 workers are shared with every
    # poll read, append, and probe, so each acquisition queues). Only reads
    # that must move corpus bytes go to the pool. Every candidate contributes
    # its signature (None when the stat failed) — the stored rows' freshness
    # ground must cover the paths that failed too, or a reappearing file could
    # not re-key the derivation.
    proven_hits: list[SessionMetadata] = []
    read_jobs: list[tuple[SessionMetadata, Path, tuple[int, int, int], int]] = []
    content_sigs: list[tuple[int, int, int] | None] = []
    for meta, path in content_candidates:
      key = str(path)
      roots = self._search_miss_memo.peek(key)
      memo_roots = tuple(roots.items()) if roots is not None else ()
      try:
        stat = path.stat()
      except OSError as e:
        _log_search_read_failed_once(meta.id, e)
        content_sigs.append(None)
        continue
      sig = (stat.st_mtime_ns, stat.st_size, stat.st_ino)
      content_sigs.append(sig)
      hit_roots = self._search_hit_memo.peek(key)
      if hit_roots is not None and _hit_root_covers(tuple(hit_roots.items()), sig, query_lower):
        proven_hits.append(meta)  # the stored hit answers without a read
        continue
      start = _absence_rescan_start(memo_roots, sig, query_lower)
      if start is not None:
        read_jobs.append((meta, path, sig, start))

    scan_failures = 0

    async def _check_content_batch(
        batch: list[tuple[SessionMetadata, Path, tuple[int, int, int], int]]) -> list[SessionMetadata | None]:
      """Read a batch of chat files whose classification demanded bytes, one
      thread-pool task for the batch.

      One task per file paid the hand-off 755 times over the active corpus
      (manager-level pooled scan 0.53 s against 0.43 s single-thread); one
      task per batch keeps the pool's one-read-per-worker overlap while paying
      the hand-off once per batch (measured 0.48-0.49 s at batches of 8-16,
      interleaved against the per-file shape). Verdicts stay per-file, so the
      memoization below is unchanged.
      """
      nonlocal scan_failures
      verdicts = await asyncio.to_thread(
          lambda: [_scan_content_for_hit(path, meta.id, query_lower, start) for meta, path, _, start in batch])
      out: list[SessionMetadata | None] = []
      for (meta, path, sig, _), verdict in zip(batch, verdicts, strict=True):
        if verdict is None:
          scan_failures += 1
          out.append(None)  # errored scan proves no absence, so nothing is memoized
        elif verdict:
          self._memoize_search_hit(str(path), sig, query_lower)
          out.append(meta)
        else:
          self._memoize_search_miss(str(path), sig, query_lower)
          out.append(None)
      return out

    scanned_hits = await asyncio.gather(
        *(
            _check_content_batch(read_jobs[at:at + _SEARCH_SCAN_BATCH_FILES])
            for at in range(0, len(read_jobs), _SEARCH_SCAN_BATCH_FILES)))
    content_hits = proven_hits + [meta for batch in scanned_hits for meta in batch if meta is not None]
    rows = matches[:_SEARCH_RESULT_LIMIT] + content_hits
    rows.sort(key=lambda meta: meta.updated_at, reverse=True)
    rows = rows[:_SEARCH_RESULT_LIMIT]
    if scan_failures == 0:
      self._search_match_memo.store(
          query_lower, (all_meta, [path for _meta, path in content_candidates], tuple(content_sigs), rows))
    derived = await self._sidebar.resolve_sidebar_state(
        rows,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )
    return rows, derived

  def _memoize_search_miss(self, memo_key: str, sig: tuple[int, int, int], needle: str) -> None:
    """Record a clean-scan miss as one more proven-absent root for the file.

    Each root covers its own superstrings while its signature holds, so
    unrelated query families keep serving from the memo beside each other;
    one needle per file would let the shortest needle ever searched occupy
    the proof and send every other family back to a full corpus scan. The
    per-file root LRU (``_SEARCH_MISS_ROOTS_PER_FILE``) and the file LRU
    (``_SEARCH_MISS_MEMO_LIMIT``) bound both maps.
    """
    roots = self._search_miss_memo.get(memo_key)
    if roots is None:
      roots = BoundedMemo(_SEARCH_MISS_ROOTS_PER_FILE)
    roots.store(needle, sig)
    self._search_miss_memo.store(memo_key, roots)

  def _memoize_search_hit(self, memo_key: str, sig: tuple[int, int, int], needle: str) -> None:
    """Record a clean-scan hit as one more proven-present root for the file.

    The root covers the needle's substrings while the inode holds and the file
    has not shrunk below the scanned size; the per-file root LRU
    (``_SEARCH_HIT_ROOTS_PER_FILE``) and the file LRU (``_SEARCH_HIT_MEMO_LIMIT``)
    bound both maps, the same shape the absence side's bounds set.
    """
    roots = self._search_hit_memo.get(memo_key)
    if roots is None:
      roots = BoundedMemo(_SEARCH_HIT_ROOTS_PER_FILE)
    roots.store(needle, sig)
    self._search_hit_memo.store(memo_key, roots)


# The process owner of the search block; built on the first ``search()`` call.
_search: SessionSearch | None = None


def search() -> SessionSearch:
  """The process-wide session search block."""
  global _search
  if _search is None:
    _search = SessionSearch(get_config(), session_store.store(), session_events.events(), session_sidebar.sidebar())
  return _search


def set_search(replacement: SessionSearch | None) -> None:
  """Replace the process search singleton (tests); None restores lazy construction."""
  global _search
  _search = replacement

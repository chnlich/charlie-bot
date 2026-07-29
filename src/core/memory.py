"""Labeled-entry memory store: parse, load, lint, assemble, and record usage.

The store is a local git repo at ``cfg.memory_dir`` (``~/.charliebot/memory/``):

  entries/<topic>/<slug>.md   # canonical entries, one fact/rule set per file
  topics                      # controlled vocabulary, one topic per line
  staging/                    # candidate files (.gitignore'd)
  usage.jsonl                 # query/injection hit log, O_APPEND (.gitignore'd)

Entry grammar: line 1 is exactly ``---``; header lines each match
``^([a-z_]+): ([A-Za-z0-9._-]+)$`` until the next line that is exactly ``---``;
everything after is an opaque markdown body whose first line is ``# <title>``.
Only the first header block is parsed, so the body may contain ``---`` lines.

All logic lives here; the CLI (``src/cli/memory.py``) is a thin wrapper, and the
spawn paths (``master_cc``, ``spawner``) call the assemble functions directly.
"""

import json
import re
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import structlog

log = structlog.get_logger()

_TOPICS_FILENAME = "topics"
_ENTRIES_DIRNAME = "entries"
_STAGING_DIRNAME = "staging"
_USAGE_FILENAME = "usage.jsonl"

# Header line: ``field: value`` where field is lower_snake and value is slug-charset.
_HEADER_RE = re.compile(r"^([a-z_]+): ([A-Za-z0-9._-]+)$")
# Topic vocabulary line: ``name`` or ``name resident``.
_TOPIC_LINE_RE = re.compile(r"^([a-z0-9][a-z0-9-]*)( resident)?$")
# Topic directory name (no resident suffix).
_TOPIC_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")
# Slug charset (entry filename stem / header value charset).
_SLUG_RE = re.compile(r"^[A-Za-z0-9._-]+$")
# Created date: YYYY-MM-DD.
_CREATED_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")

_KNOWN_FIELDS = frozenset({"scope", "topic", "audience", "created", "source", "revises"})
_SCOPES = frozenset({"user", "host"})
_AUDIENCES = frozenset({"master", "worker", "both"})
_MASTER_AUDIENCES = frozenset({"master", "both"})
_WORKER_AUDIENCES = frozenset({"worker", "both"})

CALLER_MASTER_SPAWN = "master-spawn"
CALLER_WORKER_SPAWN = "worker-spawn"
CALLER_QUERY = "query"
_CALLERS = frozenset({CALLER_MASTER_SPAWN, CALLER_WORKER_SPAWN, CALLER_QUERY})


class MemoryFormatError(Exception):
  """Raised when a memory store file violates the entry/topic grammar.

  The message names the offending file and (when applicable) the line number.
  """


@dataclass
class Topic:
  name: str
  resident: bool


@dataclass
class Entry:
  path: Path
  topic: Optional[str]
  slug: str
  scope: Optional[str]
  audience: Optional[str]
  created: Optional[str]
  source: Optional[str]
  revises: Optional[str]
  title: str
  body: str

  @property
  def id(self) -> str:
    return f"{self.topic}/{self.slug}"


@dataclass
class Store:
  memory_dir: Path
  topics: dict[str, Topic]
  entries: list[Entry]


@dataclass
class UsageStat:
  entry_id: str
  hits: int
  last_seen: Optional[str]
  idle_days: Optional[int]
  over_threshold: bool


def parse_entry(path: Path) -> Entry:
  """Parse one entry file into an :class:`Entry`, or raise :class:`MemoryFormatError`.

  Structural validation only: the ``---`` framing, header line format, known
  field names, and the ``# <title>`` body opener. Semantic checks (required
  fields, topic vocabulary membership, value domains) are done by
  :func:`load_store` and :func:`lint`. The body may contain ``---`` lines; only
  the first header block is parsed.
  """
  text = path.read_text(encoding="utf-8")
  lines = text.split("\n")
  if not lines or lines[0] != "---":
    raise MemoryFormatError(f"{path}: line 1: expected '---' front matter opener")
  header: dict[str, str] = {}
  i = 1
  while i < len(lines) and lines[i] != "---":
    m = _HEADER_RE.match(lines[i])
    if m is None:
      raise MemoryFormatError(f"{path}: line {i + 1}: malformed header line: {lines[i]!r}")
    key, value = m.group(1), m.group(2)
    if key not in _KNOWN_FIELDS:
      raise MemoryFormatError(f"{path}: line {i + 1}: unknown header field {key!r}")
    if key in header:
      raise MemoryFormatError(f"{path}: line {i + 1}: duplicate header field {key!r}")
    header[key] = value
    i += 1
  if i >= len(lines):
    raise MemoryFormatError(f"{path}: missing closing '---' after header")
  # lines[i] == "---" is the closer; the body is everything after it.
  body_lines = lines[i + 1:]
  if not body_lines or (len(body_lines) == 1 and body_lines[0] == ""):
    raise MemoryFormatError(f"{path}: line {i + 2}: empty body; expected '# <title>' first line")
  first = body_lines[0]
  if not first.startswith("# "):
    raise MemoryFormatError(f"{path}: line {i + 2}: body must start with '# <title>'")
  title = first[2:].strip()
  if not title:
    raise MemoryFormatError(f"{path}: line {i + 2}: empty title after '# '")
  body = "\n".join(body_lines)
  return Entry(
      path=path,
      topic=header.get("topic"),
      slug=path.stem,
      scope=header.get("scope"),
      audience=header.get("audience"),
      created=header.get("created"),
      source=header.get("source"),
      revises=header.get("revises"),
      title=title,
      body=body,
  )


def _load_topics(memory_dir: Path) -> dict[str, Topic]:
  """Read the topics vocabulary; raise :class:`MemoryFormatError` on a bad line."""
  topics_path = memory_dir / _TOPICS_FILENAME
  if not topics_path.is_file():
    raise MemoryFormatError(f"{topics_path}: topics vocabulary file not found")
  topics: dict[str, Topic] = {}
  for lineno, raw in enumerate(topics_path.read_text(encoding="utf-8").split("\n"), start=1):
    if raw == "":
      continue
    m = _TOPIC_LINE_RE.match(raw)
    if m is None:
      raise MemoryFormatError(f"{topics_path}: line {lineno}: malformed topic line: {raw!r}")
    name = m.group(1)
    resident = m.group(2) is not None
    if name in topics:
      raise MemoryFormatError(f"{topics_path}: line {lineno}: duplicate topic {name!r}")
    topics[name] = Topic(name=name, resident=resident)
  return topics


def _validate_entry(entry: Entry, topics: dict[str, Topic], *, relaxed: bool) -> list[str]:
  """Return a list of semantic violations for *entry* (empty = valid).

  ``relaxed`` matches the staging/ rules: header fields are optional except
  ``topic`` (which need not be in the vocabulary), and ``revises`` is allowed.
  Strict (entries/) requires all fields, topic vocabulary membership, a
  matching directory name, and forbids ``revises``.
  """
  where = _STAGING_DIRNAME if relaxed else _ENTRIES_DIRNAME
  topic_label = entry.topic or "?"

  def v(msg: str) -> str:
    return f"{where}/{topic_label}/{entry.slug}.md: {msg}"

  violations: list[str] = []
  if not _SLUG_RE.match(entry.slug):
    violations.append(v(f"filename slug {entry.slug!r} does not match slug charset [A-Za-z0-9._-]"))
  if not entry.topic:
    violations.append(v("missing required header field 'topic'"))
  elif not _TOPIC_NAME_RE.match(entry.topic):
    violations.append(v(f"topic {entry.topic!r} is not a valid topic name"))
  if relaxed:
    if entry.scope is not None and entry.scope not in _SCOPES:
      violations.append(v(f"scope {entry.scope!r} not in {{user, host}}"))
    if entry.audience is not None and entry.audience not in _AUDIENCES:
      violations.append(v(f"audience {entry.audience!r} not in {{master, worker, both}}"))
    if entry.created is not None and not _CREATED_RE.match(entry.created):
      violations.append(v(f"created {entry.created!r} not YYYY-MM-DD"))
    if entry.revises is not None and not _SLUG_RE.match(entry.revises):
      violations.append(v(f"revises {entry.revises!r} does not match slug charset"))
  else:
    if entry.topic and entry.topic not in topics:
      violations.append(v(f"topic {entry.topic!r} not in topics vocabulary"))
    parent_name = entry.path.parent.name
    if entry.topic and parent_name != entry.topic:
      violations.append(v(f"directory name {parent_name!r} != topic {entry.topic!r}"))
    for field in ("scope", "audience", "created", "source"):
      if getattr(entry, field) is None:
        violations.append(v(f"missing required header field {field!r}"))
    if entry.scope is not None and entry.scope not in _SCOPES:
      violations.append(v(f"scope {entry.scope!r} not in {{user, host}}"))
    if entry.audience is not None and entry.audience not in _AUDIENCES:
      violations.append(v(f"audience {entry.audience!r} not in {{master, worker, both}}"))
    if entry.created is not None and not _CREATED_RE.match(entry.created):
      violations.append(v(f"created {entry.created!r} not YYYY-MM-DD"))
    if entry.source is not None and not _SLUG_RE.match(entry.source):
      violations.append(v(f"source {entry.source!r} does not match slug charset"))
    if entry.revises is not None:
      violations.append(v("'revises' is forbidden in entries/ (only staging candidates may carry it)"))
  return violations


def _iter_entry_files(memory_dir: Path) -> list[Path]:
  """Return sorted entry .md files under entries/<topic>/."""
  entries_dir = memory_dir / _ENTRIES_DIRNAME
  if not entries_dir.is_dir():
    return []
  files: list[Path] = []
  for topic_dir in sorted(entries_dir.iterdir()):
    if not topic_dir.is_dir():
      continue
    files.extend(sorted(topic_dir.glob("*.md")))
  return files


def load_store(memory_dir: Path) -> Store:
  """Read the topics vocabulary and all entries; raise on any violation.

  Fail-loud: an unknown topic, a directory/topic mismatch, a bad filename
  charset, a missing ``# `` title, or ``revises`` in entries/ all raise
  :class:`MemoryFormatError`. A missing ``entries/`` directory yields an empty
  (but valid) store; a missing ``topics`` file raises.
  """
  topics = _load_topics(memory_dir)
  entries: list[Entry] = []
  for md_file in _iter_entry_files(memory_dir):
    entry = parse_entry(md_file)
    violations = _validate_entry(entry, topics, relaxed=False)
    if violations:
      raise MemoryFormatError(violations[0])
    entries.append(entry)
  return Store(memory_dir=memory_dir, topics=topics, entries=entries)


def lint(memory_dir: Path) -> list[str]:
  """Return all store violations (empty = clean).

  Validates entries/ strictly and staging/ candidates with relaxed rules
  (header fields optional except ``topic``; ``revises`` allowed; topic need
  not be in the vocabulary). A malformed topics file or entry body is reported
  as a violation rather than raised, so the full list surfaces at once.
  """
  violations: list[str] = []
  topics_path = memory_dir / _TOPICS_FILENAME
  if not topics_path.is_file():
    violations.append(f"{topics_path}: topics vocabulary file not found")
    topics = {}
  else:
    try:
      topics = _load_topics(memory_dir)
    except MemoryFormatError as e:
      violations.append(str(e))
      topics = {}
  for md_file in _iter_entry_files(memory_dir):
    try:
      entry = parse_entry(md_file)
    except MemoryFormatError as e:
      violations.append(str(e))
      continue
    violations.extend(_validate_entry(entry, topics, relaxed=False))
  staging_dir = memory_dir / _STAGING_DIRNAME
  if staging_dir.is_dir():
    for md_file in sorted(staging_dir.glob("*.md")):
      try:
        entry = parse_entry(md_file)
      except MemoryFormatError as e:
        violations.append(str(e))
        continue
      violations.extend(_validate_entry(entry, topics, relaxed=True))
  return violations


def _format_block(full_body_entries: list[Entry], index_entries: list[Entry]) -> str:
  """Join full bodies (sorted) then index lines (sorted) into one text block."""
  chunks: list[str] = []
  if full_body_entries:
    chunks.append("\n\n".join(e.body.rstrip("\n") for e in full_body_entries))
  if index_entries:
    chunks.append("\n".join(f"{e.topic}/{e.slug} \u00b7 {e.title}" for e in index_entries))
  return "\n\n".join(chunks)


def assemble_master(memory_dir: Path) -> Optional[str]:
  """Assemble the master spawn memory block.

  Full bodies of entries in resident topics with audience ``master``/``both``,
  then index lines (``<topic>/<slug> \u00b7 <title>``) for all other
  master/both entries, each group stably sorted by ``(topic, slug)``. Records
  one usage line (caller ``master-spawn``) for the full-body injections only.

  Returns None when the memory dir is missing (logged) or when the store has
  no master/both entries to inject. A malformed store propagates
  :class:`MemoryFormatError` (fail-loud); only a missing dir is tolerated.
  """
  if not memory_dir.is_dir():
    log.error("memory_dir_missing", path=str(memory_dir))
    return None
  store = load_store(memory_dir)
  resident_names = {t.name for t in store.topics.values() if t.resident}
  full_body_entries: list[Entry] = []
  index_entries: list[Entry] = []
  for e in store.entries:
    if e.audience not in _MASTER_AUDIENCES:
      continue
    if e.topic in resident_names:
      full_body_entries.append(e)
    else:
      index_entries.append(e)
  if not full_body_entries and not index_entries:
    return None
  full_body_entries.sort(key=lambda e: (e.topic, e.slug))
  index_entries.sort(key=lambda e: (e.topic, e.slug))
  block = _format_block(full_body_entries, index_entries)
  if full_body_entries:
    record_usage(memory_dir, CALLER_MASTER_SPAWN, [e.id for e in full_body_entries])
  return block


def assemble_worker(memory_dir: Path, repo_basename: str) -> Optional[str]:
  """Assemble the worker spawn memory block for *repo_basename*.

  Full bodies of entries whose topic equals *repo_basename* with audience
  ``worker``/``both``, then index lines for all other worker/both entries,
  then one usage line explaining ``charliebot memory query`` and
  ``charliebot memory add`` (workers may stage candidates). Records one usage
  line (caller ``worker-spawn``) for the full-body injections only.

  Returns None when the memory dir is missing (logged). When the store exists
  the usage line is always present, so the result is non-None. A malformed
  store propagates :class:`MemoryFormatError` (fail-loud).
  """
  if not memory_dir.is_dir():
    log.error("memory_dir_missing", path=str(memory_dir))
    return None
  store = load_store(memory_dir)
  full_body_entries: list[Entry] = []
  index_entries: list[Entry] = []
  for e in store.entries:
    if e.audience not in _WORKER_AUDIENCES:
      continue
    if e.topic == repo_basename:
      full_body_entries.append(e)
    else:
      index_entries.append(e)
  full_body_entries.sort(key=lambda e: (e.topic, e.slug))
  index_entries.sort(key=lambda e: (e.topic, e.slug))
  usage_line = (
      "On-demand knowledge: `charliebot memory query --topic <topic>` (full text) or `--index` "
      "for the index only. Stage a candidate with `charliebot memory add --topic <topic> "
      "--scope <user|host> --audience <master|worker|both> [--revises <slug>]` (writes staging/, "
      "never entries/).")
  chunks: list[str] = []
  if full_body_entries:
    chunks.append("\n\n".join(e.body.rstrip("\n") for e in full_body_entries))
  if index_entries:
    chunks.append("\n".join(f"{e.topic}/{e.slug} \u00b7 {e.title}" for e in index_entries))
  chunks.append(usage_line)
  if full_body_entries:
    record_usage(memory_dir, CALLER_WORKER_SPAWN, [e.id for e in full_body_entries])
  return "\n\n".join(chunks)


def record_usage(memory_dir: Path, caller: str, entry_ids: list[str]) -> None:
  """Append one JSON line ``{ts, caller, entries}`` to usage.jsonl via O_APPEND.

  ``caller`` must be one of ``master-spawn``, ``worker-spawn``, ``query``. A
  single ``write`` call appends the whole line. No-op when *entry_ids* is
  empty (nothing was injected). The caller is responsible for ensuring
  *memory_dir* exists.
  """
  if caller not in _CALLERS:
    raise ValueError(f"invalid caller {caller!r}; expected one of {sorted(_CALLERS)}")
  if not entry_ids:
    return
  usage_path = memory_dir / _USAGE_FILENAME
  line = json.dumps({
      "ts": datetime.now(timezone.utc).isoformat(),
      "caller": caller,
      "entries": list(entry_ids),
  })
  with open(usage_path, "a", encoding="utf-8") as f:
    f.write(line + "\n")


def usage_stats(memory_dir: Path, idle_days: int = 60) -> list[UsageStat]:
  """Per-entry usage stats: hits, last_seen, idle_days, over_threshold.

  Reads usage.jsonl (skipping corrupt lines with a warning) and tallies per
  entry id. An entry never seen has ``hits=0``, ``last_seen=None``,
  ``idle_days=None``, ``over_threshold=True`` (maximally idle; the curator
  applies the created-date grace separately). Seen entries compute
  ``idle_days`` from the most recent record; ``over_threshold`` is True when
  ``idle_days >= idle_days``. Results are sorted by entry id.
  """
  store = load_store(memory_dir)
  hits: dict[str, int] = {e.id: 0 for e in store.entries}
  last_seen: dict[str, datetime] = {}
  usage_path = memory_dir / _USAGE_FILENAME
  if usage_path.is_file():
    for raw in usage_path.read_text(encoding="utf-8").splitlines():
      if not raw.strip():
        continue
      try:
        rec = json.loads(raw)
        ts = rec["ts"]
        ids = rec["entries"]
        if not isinstance(ts, str) or not isinstance(ids, list):
          raise ValueError("bad record shape")
        seen_dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        if seen_dt.tzinfo is None:
          seen_dt = seen_dt.replace(tzinfo=timezone.utc)
        for eid in ids:
          if not isinstance(eid, str):
            raise ValueError("bad entry id")
          if eid in hits:
            hits[eid] += 1
            prev = last_seen.get(eid)
            if prev is None or seen_dt > prev:
              last_seen[eid] = seen_dt
      except (ValueError, KeyError, TypeError) as e:
        log.warning("memory_usage_corrupt_line_skipped", line=raw, error=str(e))
        continue
  now = datetime.now(timezone.utc)
  stats: list[UsageStat] = []
  for e in store.entries:
    ls = last_seen.get(e.id)
    if ls is not None:
      idle = (now - ls).days
      stats.append(
          UsageStat(
              entry_id=e.id,
              hits=hits.get(e.id, 0),
              last_seen=ls.isoformat(),
              idle_days=idle,
              over_threshold=idle >= idle_days))
    else:
      stats.append(UsageStat(entry_id=e.id, hits=0, last_seen=None, idle_days=None, over_threshold=True))
  stats.sort(key=lambda s: s.entry_id)
  return stats

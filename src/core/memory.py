"""Labeled-entry memory store: parse, load, lint, and assemble.

The store is a local git repo at ``cfg.memory_dir`` (``~/.charliebot/memory/``):

  entries/<topic>/<slug>.md   # canonical entries, one fact/rule set per file
  topics                      # controlled vocabulary, one topic per line
  staging/                    # candidate files (.gitignore'd)

Entry grammar (format v2): line 1 is exactly ``---``; header lines each match
``^([a-z_]+): <value>$`` until the next line that is exactly ``---``; everything
after is an opaque pure-markdown body with no first-line requirement. The v2
header carries ``scope``, ``topic``, ``audience`` (comma list of ``master`` /
``worker``), and ``title``; ``revises`` is staging-only. Only the first header
block is parsed, so the body may contain ``---`` lines.

Parsing is dual-read so the legacy (v1) store keeps working until it is
migrated: a missing frontmatter ``title`` falls back to a body first line of
``# <title>``, legacy ``audience: both`` reads as ``master, worker``, and
``created``/``source`` remain parseable (lint rejects them in entries/ only).

All logic lives here; the CLI (``src/cli/memory.py``) is a thin wrapper, and the
spawn paths (``master_cc``, ``spawner``) call the assemble functions directly.
"""

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

import structlog

log = structlog.get_logger()

_TOPICS_FILENAME = "topics"
_ENTRIES_DIRNAME = "entries"
_STAGING_DIRNAME = "staging"

# Header line: ``field: value`` where field is lower_snake. Value charset is
# validated per field below (slug-charset for most, free text for ``title``).
_HEADER_RE = re.compile(r"^([a-z_]+): (.+)$")
# Topic vocabulary line: ``name`` or ``name resident``.
_TOPIC_LINE_RE = re.compile(r"^([a-z0-9][a-z0-9-]*)( resident)?$")
# Topic directory name (no resident suffix).
_TOPIC_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")
# Slug charset (entry filename stem / header value charset).
_SLUG_RE = re.compile(r"^[A-Za-z0-9._-]+$")
# Created date (legacy field): YYYY-MM-DD.
_CREATED_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
# Audience value: comma list of slug-charset elements.
_AUDIENCE_VALUE_RE = re.compile(r"^[A-Za-z0-9._-]+( *, *[A-Za-z0-9._-]+)*$")

# ``created``/``source`` stay parseable for dual-read; lint rejects them in
# entries/ only.
_KNOWN_FIELDS = frozenset({"scope", "topic", "audience", "title", "created", "source", "revises"})
_SCOPES = frozenset({"user", "host"})
_AUDIENCE_ELEMENTS = frozenset({"master", "worker"})

# Header line prepended to the index lines by both spawn assemblers.
INDEX_HEADER = "# Memory index — full text via `charliebot memory query --topic <topic>`"


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
  audience: Optional[list[str]]  # comma list parsed out; legacy ``both`` -> ["master", "worker"]
  audience_raw: Optional[str]  # raw frontmatter value, kept for lint diagnostics (literal ``both``)
  created: Optional[str]
  source: Optional[str]
  revises: Optional[str]
  title: str  # frontmatter ``title`` preferred; legacy fallback is the body ``# <title>`` first line
  title_in_header: bool  # True when the title came from frontmatter (v2) rather than the body (legacy)
  body: str

  @property
  def id(self) -> str:
    return f"{self.topic}/{self.slug}"


@dataclass
class Store:
  memory_dir: Path
  topics: dict[str, Topic]
  entries: list[Entry]


def _parse_audience(raw: str) -> list[str]:
  """Split a comma-list audience value; legacy ``both`` reads as ``["master", "worker"]``."""
  if raw == "both":
    return ["master", "worker"]
  return [part.strip() for part in raw.split(",")]


def parse_entry(path: Path) -> Entry:
  """Parse one entry file into an :class:`Entry`, or raise :class:`MemoryFormatError`.

  Structural validation only: the ``---`` framing, header line format, known
  field names, and per-field value charsets. Semantic checks (required fields,
  topic vocabulary membership, value domains) are done by :func:`load_store`
  and :func:`lint`. The body may contain ``---`` lines; only the first header
  block is parsed.

  Dual-read: the title comes from frontmatter ``title`` when present, else
  falls back to a legacy body first line of ``# <title>``; when neither exists
  the entry is malformed (fail-loud).
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
    if key == "title":
      value = value.strip()
      if not value:
        raise MemoryFormatError(f"{path}: line {i + 1}: empty 'title' header value")
    elif key == "audience":
      if not _AUDIENCE_VALUE_RE.match(value):
        raise MemoryFormatError(f"{path}: line {i + 1}: malformed header line: {lines[i]!r}")
    elif not _SLUG_RE.match(value):
      raise MemoryFormatError(f"{path}: line {i + 1}: malformed header line: {lines[i]!r}")
    header[key] = value
    i += 1
  if i >= len(lines):
    raise MemoryFormatError(f"{path}: missing closing '---' after header")
  # lines[i] == "---" is the closer; the body is everything after it.
  body_lines = lines[i + 1:]
  if not body_lines or (len(body_lines) == 1 and body_lines[0] == ""):
    raise MemoryFormatError(f"{path}: line {i + 2}: empty body")
  body = "\n".join(body_lines)
  if "title" in header:
    title = header["title"]
    title_in_header = True
  else:
    # Legacy fallback: the body's first line carries `# <title>`.
    first = body_lines[0]
    if not first.startswith("# "):
      raise MemoryFormatError(f"{path}: line {i + 2}: no frontmatter 'title' and body must start with '# <title>'")
    title = first[2:].strip()
    title_in_header = False
    if not title:
      raise MemoryFormatError(f"{path}: line {i + 2}: empty title after '# '")
  audience_raw = header.get("audience")
  return Entry(
      path=path,
      topic=header.get("topic"),
      slug=path.stem,
      scope=header.get("scope"),
      audience=_parse_audience(audience_raw) if audience_raw is not None else None,
      audience_raw=audience_raw,
      created=header.get("created"),
      source=header.get("source"),
      revises=header.get("revises"),
      title=title,
      title_in_header=title_in_header,
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


def _audience_violations(entry: Entry, v: Callable[[str], str]) -> list[str]:
  """Element-domain violations for the parsed audience list (empty = valid)."""
  if entry.audience is None:
    return []
  return [
      v(f"audience element {el!r} not in {{master, worker}}") for el in entry.audience if el not in _AUDIENCE_ELEMENTS
  ]


def _validate_entry(entry: Entry, topics: dict[str, Topic], *, relaxed: bool, strict_v2: bool = False) -> list[str]:
  """Return a list of semantic violations for *entry* (empty = valid).

  ``relaxed`` matches the staging/ rules: header fields are optional except
  ``topic`` (which need not be in the vocabulary), ``revises`` is allowed, and
  ``created``/``source`` are not violations (existing staged candidates stay
  lint-clean). Strict (entries/) requires ``scope``/``audience``, topic
  vocabulary membership, and a matching directory name, and forbids
  ``revises``. The base strict rules stay dual-read so :func:`load_store`
  keeps loading the legacy store; ``strict_v2`` (lint only) adds the v2
  requirements: frontmatter ``title``, no literal ``both``, and no
  ``created``/``source``.
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
    violations.extend(_audience_violations(entry, v))
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
    for field in ("scope", "audience"):
      if getattr(entry, field) is None:
        violations.append(v(f"missing required header field {field!r}"))
    if entry.scope is not None and entry.scope not in _SCOPES:
      violations.append(v(f"scope {entry.scope!r} not in {{user, host}}"))
    violations.extend(_audience_violations(entry, v))
    if entry.created is not None and not _CREATED_RE.match(entry.created):
      violations.append(v(f"created {entry.created!r} not YYYY-MM-DD"))
    if entry.source is not None and not _SLUG_RE.match(entry.source):
      violations.append(v(f"source {entry.source!r} does not match slug charset"))
    if entry.revises is not None:
      violations.append(v("'revises' is forbidden in entries/ (only staging candidates may carry it)"))
    if strict_v2:
      if not entry.title_in_header:
        violations.append(v("missing required header field 'title'"))
      if entry.audience_raw == "both":
        violations.append(v("literal audience 'both' is forbidden in entries/; write 'master, worker'"))
      if entry.created is not None:
        violations.append(v("'created' is forbidden in entries/ (dropped in entry format v2)"))
      if entry.source is not None:
        violations.append(v("'source' is forbidden in entries/ (dropped in entry format v2)"))
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
  charset, an unresolvable title (no frontmatter ``title`` and no ``# ``
  body opener), or ``revises`` in entries/ all raise
  :class:`MemoryFormatError`. Validation stays dual-read: legacy
  ``created``/``source``/``both``/body-title entries still load (only lint is
  v2-strict). A missing ``entries/`` directory yields an empty (but valid)
  store; a missing ``topics`` file raises.
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

  Validates entries/ with the strict v2 rules (frontmatter ``title`` required;
  literal ``both`` and ``created``/``source`` are violations) and staging/
  candidates with relaxed rules (header fields optional except ``topic``;
  ``revises`` allowed; topic need not be in the vocabulary; comma-list
  audience and legacy ``both`` accepted; ``created``/``source`` not flagged).
  A malformed topics file or entry body is reported as a violation rather than
  raised, so the full list surfaces at once.
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
    violations.extend(_validate_entry(entry, topics, relaxed=False, strict_v2=True))
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


def full_text(entry: Entry) -> str:
  """The entry's presentable full text: ``# {title}`` + blank line + body.

  A legacy body that already opens with ``# `` is returned as-is so its own
  heading is not duplicated. Trailing newlines are stripped.
  """
  body = entry.body.rstrip("\n")
  if body.startswith("# "):
    return body
  return f"# {entry.title}\n\n{body}"


def _index_lines(index_entries: list[Entry]) -> str:
  """The INDEX_HEADER line followed by sorted ``<topic>/<slug> · <title>`` lines."""
  return "\n".join([INDEX_HEADER] + [f"{e.topic}/{e.slug} · {e.title}" for e in index_entries])


def _format_block(full_body_entries: list[Entry], index_entries: list[Entry]) -> str:
  """Join full bodies (sorted) then index lines (sorted) into one text block."""
  chunks: list[str] = []
  if full_body_entries:
    chunks.append("\n\n".join(full_text(e) for e in full_body_entries))
  if index_entries:
    chunks.append(_index_lines(index_entries))
  return "\n\n".join(chunks)


def assemble_master(memory_dir: Path) -> Optional[str]:
  """Assemble the master spawn memory block.

  Full bodies of entries in resident topics whose audience contains
  ``master``, then the INDEX_HEADER line and index lines
  (``<topic>/<slug> · <title>``) for all other master-audience entries, each
  group stably sorted by ``(topic, slug)``.

  Returns None when the memory dir is missing (logged) or when the store has
  no master-audience entries to inject. A malformed store propagates
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
    if e.audience is None or "master" not in e.audience:
      continue
    if e.topic in resident_names:
      full_body_entries.append(e)
    else:
      index_entries.append(e)
  if not full_body_entries and not index_entries:
    return None
  full_body_entries.sort(key=lambda e: (e.topic, e.slug))
  index_entries.sort(key=lambda e: (e.topic, e.slug))
  return _format_block(full_body_entries, index_entries)


def assemble_worker(memory_dir: Path, repo_basename: str) -> Optional[str]:
  """Assemble the worker spawn memory block for *repo_basename*.

  Full bodies of entries whose topic equals *repo_basename* and whose audience
  contains ``worker``, then the INDEX_HEADER line and index lines for all
  other worker-audience entries, then one usage line explaining
  ``charliebot memory query`` and ``charliebot memory add`` (workers may stage
  candidates).

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
    if e.audience is None or "worker" not in e.audience:
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
      "--scope <user|host> --audience <master|worker|master,worker> [--revises <slug>]` "
      "(writes staging/, never entries/).")
  chunks: list[str] = []
  if full_body_entries:
    chunks.append("\n\n".join(full_text(e) for e in full_body_entries))
  if index_entries:
    chunks.append(_index_lines(index_entries))
  chunks.append(usage_line)
  return "\n\n".join(chunks)

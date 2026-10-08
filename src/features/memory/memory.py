"""Labeled-entry memory store: parse, load, lint, and assemble.

The store is a local git repo at the store root (``store_root.memory_dir``, ``~/.charliebot/memory/``):

  entries/<topic>/<slug>.md   # canonical entries, one fact/rule set per file
  topics                      # controlled vocabulary, one topic per line
  staging/                    # free-form capture files, labels assigned at curation (.gitignore'd)

Agent-facing read contract: the topic is the sole read unit. ``memory query
--topic <topic>`` returns every entry of that topic admitted for the caller's
audience, as one whole. The entry layer belongs to the store's internals: it
separates request kinds — audience (master vs worker), scope, revision
tracking — while agents see topics only. Whole-topic reads keep that
separation sound, so every read surface keeps the topic as its unit.

Entry grammar (format v2): line 1 is exactly ``---``; header lines each match
``^([a-z_]+): <value>$`` until the next line that is exactly ``---``; everything
after is an opaque pure-markdown body with no first-line requirement. The v2
header carries ``scope``, ``topic``, ``audience`` (comma list of ``master`` /
``worker``), and ``title``; ``revises`` is staging-only. Only the first header
block is parsed, so the body may contain ``---`` lines.

Parsing reads both store formats: a missing frontmatter ``title`` falls back to a body first line of
``# <title>``, legacy ``audience: both`` reads as ``master, worker``, and
``created``/``source`` remain parseable (lint rejects them in entries/ only).

All logic lives here; the CLI (``src/features/memory/cli.py``) is a thin wrapper, and task
prompt snapshots use the selection functions directly.

The store creates its own scaffold: :func:`ensure_store` runs first in every entry
point that reads or writes the live store (:func:`load_store`, ``memory add``, the
``memory proposal`` verbs), so a fresh home has no store until its first use.
:func:`lint` reports the tree as it finds it and never calls it.
"""

import os
import pathlib
import re
import subprocess
from collections.abc import Callable

from src.infra import memo

_TOPICS_FILENAME = "topics"
_ENTRIES_DIRNAME = "entries"
_STAGING_DIRNAME = "staging"

DEFAULT_MEMORY_TOPICS = (
    "profile resident\n"
    "communication resident\n"
    "workflow resident\n"
    "rulings resident\n"
    "host resident\n"
    "charliebot\n")

DEFAULT_MEMORY_GITIGNORE = "staging/\n"

# Bound on _store_memo in memory dirs, not entries: a host serves one memory
# dir in steady state (tests hold several), so a small cap bounds memoized
# Store payloads. An entry holds the stat-only signature alongside the Store.
_STORE_MEMO_LIMIT = 8
_store_memo: memo.BoundedMemo[pathlib.Path, tuple[tuple[tuple[str, int, int], ...],
                                                  Store]] = memo.BoundedMemo(_STORE_MEMO_LIMIT)

# Header line: ``field: value`` where field is lower_snake. Value charset is
# validated per field below (slug-charset for most, free text for ``title``).
_HEADER_RE = re.compile(r"^([a-z_]+): (.+)$")
# Topic vocabulary line: ``name`` or ``name resident``.
_TOPIC_LINE_RE = re.compile(r"^([a-z0-9][a-z0-9-]*)( resident)?$")
# Topic name: the store's namespace charset for entry header fields.
TOPIC_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")
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
INDEX_HEADER = (
    '# Memory index — full text via `charliebot memory query --topic <topic>` (topic = segment before "/", e.g. '
    "`--topic integrations`)")


class MemoryFormatError(Exception):
  """Raised when a memory store file violates the entry/topic grammar.

  The message names the offending file and (when applicable) the line number.
  """


class Topic:
  """One vocabulary line: the topic name and whether it is resident."""

  __slots__ = ("name", "resident")

  def __init__(self, name: str, resident: bool) -> None:
    self.name = name
    self.resident = resident


class Entry:
  """One parsed entry file.

  ``audience`` is the comma list parsed out (legacy ``both`` -> ["master", "worker"]);
  ``audience_raw`` keeps the raw frontmatter value for lint diagnostics (literal
  ``both``). ``title`` is the frontmatter ``title`` when present, else the body's
  first ``# `` line (legacy).
  """

  __slots__ = (
      "audience",
      "audience_raw",
      "body",
      "created",
      "path",
      "revises",
      "scope",
      "slug",
      "source",
      "title",
      "title_in_header",
      "topic",
  )

  def __init__(
      self, path: pathlib.Path, topic: str | None, slug: str, scope: str | None, audience: list[str] | None,
      audience_raw: str | None, created: str | None, source: str | None, revises: str | None, title: str,
      title_in_header: bool, body: str) -> None:
    self.path = path
    self.topic = topic
    self.slug = slug
    self.scope = scope
    self.audience = audience
    self.audience_raw = audience_raw
    self.created = created
    self.source = source
    self.revises = revises
    self.title = title
    self.title_in_header = title_in_header
    self.body = body

  @property
  def id(self) -> str:
    return f"{self.topic}/{self.slug}"


class Store:
  """One loaded store: the memory dir, the topics vocabulary, the parsed entries."""

  __slots__ = ("entries", "memory_dir", "topics")

  def __init__(self, memory_dir: pathlib.Path, topics: dict[str, Topic], entries: list[Entry]) -> None:
    self.memory_dir = memory_dir
    self.topics = topics
    self.entries = entries


def _parse_audience(raw: str) -> list[str]:
  """Split a comma-list audience value; legacy ``both`` reads as ``["master", "worker"]``."""
  if raw == "both":
    return ["master", "worker"]
  return [part.strip() for part in raw.split(",")]


def parse_entry(path: pathlib.Path) -> Entry:
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
  return parse_entry_text(path.read_text(encoding="utf-8"), entry_path=path)


def parse_entry_text(text: str, *, entry_path: pathlib.Path) -> Entry:
  """Parse one entry file's *text* into an :class:`Entry`, or raise :class:`MemoryFormatError`.

  Shared body of :func:`parse_entry`, callable without a file on disk so a
  proposed (not yet written) entry gets the same structural parse. *entry_path*
  only attributes errors and supplies the parsed identity: the slug is its
  filename stem and validation compares its parent directory name against the
  topic, so callers parsing proposed text pass the path the entry would take.
  """
  lines = text.split("\n")
  if not lines or lines[0] != "---":
    raise MemoryFormatError(f"{entry_path}: line 1: expected '---' front matter opener")
  header: dict[str, str] = {}
  i = 1
  while i < len(lines) and lines[i] != "---":
    m = _HEADER_RE.match(lines[i])
    if m is None:
      raise MemoryFormatError(f"{entry_path}: line {i + 1}: malformed header line: {lines[i]!r}")
    key, value = m.group(1), m.group(2)
    if key not in _KNOWN_FIELDS:
      raise MemoryFormatError(f"{entry_path}: line {i + 1}: unknown header field {key!r}")
    if key in header:
      raise MemoryFormatError(f"{entry_path}: line {i + 1}: duplicate header field {key!r}")
    if key == "title":
      value = value.strip()
      if not value:
        raise MemoryFormatError(f"{entry_path}: line {i + 1}: empty 'title' header value")
    elif key == "audience":
      if not _AUDIENCE_VALUE_RE.match(value):
        raise MemoryFormatError(f"{entry_path}: line {i + 1}: malformed header line: {lines[i]!r}")
    elif not _SLUG_RE.match(value):
      raise MemoryFormatError(f"{entry_path}: line {i + 1}: malformed header line: {lines[i]!r}")
    header[key] = value
    i += 1
  if i >= len(lines):
    raise MemoryFormatError(f"{entry_path}: missing closing '---' after header")
  # lines[i] == "---" is the closer; the body is everything after it.
  body_lines = lines[i + 1:]
  if not body_lines or (len(body_lines) == 1 and body_lines[0] == ""):
    raise MemoryFormatError(f"{entry_path}: line {i + 2}: empty body")
  body = "\n".join(body_lines)
  if "title" in header:
    title = header["title"]
    title_in_header = True
  else:
    # Legacy fallback: the body's first line carries `# <title>`.
    first = body_lines[0]
    if not first.startswith("# "):
      raise MemoryFormatError(
          f"{entry_path}: line {i + 2}: no frontmatter 'title' and body must start with '# <title>'")
    title = first[2:].strip()
    title_in_header = False
    if not title:
      raise MemoryFormatError(f"{entry_path}: line {i + 2}: empty title after '# '")
  audience_raw = header.get("audience")
  return Entry(
      path=entry_path,
      topic=header.get("topic"),
      slug=entry_path.stem,
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


def _load_topics(memory_dir: pathlib.Path) -> dict[str, Topic]:
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

  ``relaxed`` matches the staging/ rules for legacy frontmatter candidates:
  header fields are optional except ``topic`` (which need not be in the
  vocabulary), ``revises`` is allowed, and
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

  def header_field_violations() -> list[str]:
    """The scope/audience/created header checks both rule sets share."""
    out: list[str] = []
    if entry.scope is not None and entry.scope not in _SCOPES:
      out.append(v(f"scope {entry.scope!r} not in {{user, host}}"))
    out.extend(_audience_violations(entry, v))
    if entry.created is not None and not _CREATED_RE.match(entry.created):
      out.append(v(f"created {entry.created!r} not YYYY-MM-DD"))
    return out

  violations: list[str] = []
  if not _SLUG_RE.match(entry.slug):
    violations.append(v(f"filename slug {entry.slug!r} does not match slug charset [A-Za-z0-9._-]"))
  if not entry.topic:
    violations.append(v("missing required header field 'topic'"))
  elif not TOPIC_NAME_RE.match(entry.topic):
    violations.append(v(f"topic {entry.topic!r} is not a valid topic name"))
  if relaxed:
    violations.extend(header_field_violations())
    if entry.revises is not None and not _SLUG_RE.match(entry.revises):
      violations.append(v(f"revises {entry.revises!r} does not match slug charset"))
  else:
    if entry.topic and entry.topic not in topics:
      violations.append(v(f"topic {entry.topic!r} not in topics vocabulary"))
    parent_name = entry.path.parent.name
    if entry.topic and parent_name != entry.topic:
      violations.append(v(f"directory name {parent_name!r} != topic {entry.topic!r}"))
    violations.extend(
        v(f"missing required header field {field!r}")
        for field in ("scope", "audience")
        if getattr(entry, field) is None)
    violations.extend(header_field_violations())
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


def _iter_entry_files(memory_dir: pathlib.Path) -> list[pathlib.Path]:
  """Return sorted entry .md files under entries/<topic>/."""
  entries_dir = memory_dir / _ENTRIES_DIRNAME
  if not entries_dir.is_dir():
    return []
  files: list[pathlib.Path] = []
  for topic_dir in sorted(entries_dir.iterdir()):
    if not topic_dir.is_dir():
      continue
    files.extend(sorted(topic_dir.glob("*.md")))
  return files


def _store_signature(memory_dir: pathlib.Path) -> tuple[tuple[str, int, int], ...] | None:
  """Stat-only signature of every byte :func:`load_store` parses.

  One (relative path, mtime_ns, size) triple per parsed file — the topics
  vocabulary plus every entries/<topic>/*.md — so any rewrite, append, new
  entry, or deletion changes the signature. Returns None when a file cannot
  be stat'ed (missing topics file, a race with a writer): that call must not
  memoize, and the load itself surfaces or tolerates the missing file exactly
  as the uncached path does.

  Walked with os.scandir and string joins: pathlib.Path.glob/pathlib.Path.relative_to would
  rebuild a pathlib.Path per entry, and that allocation cost dominates the stats the
  signature exists to pay (the _load_session_metas preamble's lesson).
  """
  sig: list[tuple[str, int, int]] = []
  try:
    st = os.stat(os.path.join(memory_dir, _TOPICS_FILENAME))
  except OSError:
    return None
  sig.append((_TOPICS_FILENAME, st.st_mtime_ns, st.st_size))
  try:
    topic_names = sorted(e.name for e in os.scandir(os.path.join(memory_dir, _ENTRIES_DIRNAME)) if e.is_dir())
  except OSError:
    topic_names = []  # a missing entries/ loads as the valid empty store
  for topic in topic_names:
    topic_path = os.path.join(memory_dir, _ENTRIES_DIRNAME, topic)
    try:
      md_names = sorted(e.name for e in os.scandir(topic_path) if e.name.endswith(".md"))
    except OSError:
      return None
    for name in md_names:
      try:
        st = os.stat(os.path.join(topic_path, name))
      except OSError:
        return None
      sig.append((f"{topic}/{name}", st.st_mtime_ns, st.st_size))
  return tuple(sig)


def _seed_if_missing(path: pathlib.Path, content: str) -> None:
  """Write content to path only if the file does not already exist."""
  if not path.exists():
    path.write_text(content, encoding="utf-8")


def ensure_store(memory_dir: pathlib.Path) -> None:
  """Create the labeled-entry store scaffold at *memory_dir* (idempotent).

  Creates the directory, runs ``git init`` when it is not already a repo, seeds
  the topics vocabulary and .gitignore (never overwriting existing files), and
  creates the ``entries/`` and ``staging/`` directories. The canon (entries/
  and topics) is populated only by user-approved curation diffs, never here.
  """
  memory_dir.mkdir(parents=True, exist_ok=True)
  if not (memory_dir / ".git").exists():
    subprocess.run(["git", "init"], cwd=str(memory_dir), check=True, capture_output=True)
  _seed_if_missing(memory_dir / _TOPICS_FILENAME, DEFAULT_MEMORY_TOPICS)
  _seed_if_missing(memory_dir / ".gitignore", DEFAULT_MEMORY_GITIGNORE)
  (memory_dir / _ENTRIES_DIRNAME).mkdir(exist_ok=True)
  (memory_dir / _STAGING_DIRNAME).mkdir(exist_ok=True)


def load_store(memory_dir: pathlib.Path) -> Store:
  """Read the topics vocabulary and all entries; raise on any violation.

  Calls :func:`ensure_store` first, so a missing store directory or ``topics``
  file is created from the scaffold defaults and loads as the valid empty store.

  Fail-loud: an unknown topic, a directory/topic mismatch, a bad filename
  charset, an unresolvable title (no frontmatter ``title`` and no ``# ``
  body opener), or ``revises`` in entries/ all raise
  :class:`MemoryFormatError`. Validation stays dual-read: legacy
  ``created``/``source``/``both``/body-title entries still load (only lint is
  v2-strict).

  Repeat loads of an unchanged store are served from a process-wide memo
  keyed on :func:`_store_signature`'s (path, mtime_ns, size) read of every
  parsed file: the master run's per-message instruction build and the worker
  spawn path re-enter here many times a minute under the same bytes, and
  entry writes go through file rewrites that bump the signature. Only
  successful loads memoize; a malformed store keeps raising on every call.
  The memoized Store is shared with callers, whose contract is read-only.
  A rewrite to a malformed store drops the stale hit, so the failure never
  lingers as an entry the next call could confuse with the current bytes.
  """
  ensure_store(memory_dir)
  sig = _store_signature(memory_dir)
  if sig is not None:
    hit = _store_memo.get(memory_dir)
    if hit is not None and hit[0] == sig:
      return hit[1]
  try:
    store = _load_store_uncached(memory_dir)
  except BaseException:
    if sig is not None:
      _store_memo.drop(memory_dir)
    raise
  if sig is not None:
    _store_memo.store(memory_dir, (sig, store))
  return store


def _load_store_uncached(memory_dir: pathlib.Path) -> Store:
  """Parse the store from disk; the work :func:`load_store` memoizes."""
  topics = _load_topics(memory_dir)
  entries: list[Entry] = []
  for md_file in _iter_entry_files(memory_dir):
    entry = parse_entry(md_file)
    violations = _validate_entry(entry, topics, relaxed=False)
    if violations:
      raise MemoryFormatError(violations[0])
    entries.append(entry)
  return Store(memory_dir=memory_dir, topics=topics, entries=entries)


def lint(memory_dir: pathlib.Path) -> list[str]:
  """Return all store violations (empty = clean).

  Validates entries/ with the strict v2 rules (frontmatter ``title`` required;
  literal ``both`` and ``created``/``source`` are violations). staging/ files
  dispatch on the first line: a first line of exactly ``---`` parses as a
  frontmatter candidate under the relaxed rules (header fields optional except
  ``topic``; ``revises`` allowed; topic need not be in the vocabulary;
  comma-list audience and legacy ``both`` accepted; ``created``/``source``
  not flagged); any other file is a free-form capture, valid iff it is
  non-empty and its first line is a non-empty ``# <title>``.
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
      text = md_file.read_text(encoding="utf-8")
      first_line = text.split("\n", 1)[0]
      if first_line == "---":
        # Legacy frontmatter candidate: relaxed rules, unchanged.
        try:
          entry = parse_entry(md_file)
        except MemoryFormatError as e:
          violations.append(str(e))
          continue
        violations.extend(_validate_entry(entry, topics, relaxed=True))
        continue
      # Free-form capture: valid iff non-empty with a '# <title>' first line.
      if not text:
        violations.append(f"{md_file}: empty capture file")
      elif not first_line.startswith("# "):
        violations.append(f"{md_file}: line 1: capture must start with '# <title>'")
      elif not first_line[2:].strip():
        violations.append(f"{md_file}: line 1: empty title after '# '")
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


def audience_allows(entry: Entry, audience: str) -> bool:
  """The one audience predicate every read surface shares (startup, preview, query).

  An entry with no audience header admits nobody; otherwise the audience must
  be one of the entry's declared elements.
  """
  return entry.audience is not None and audience in entry.audience


class MemoryEntrySource:
  """One selected entry's provenance and contribution.

  ``source_ref`` names the store origin (``memory:<topic>/<slug>``); the slug
  identifies the entry file under ``entries/<topic>/``. ``delivery`` is the
  form actually injected: ``full`` (whole entry text) or ``index`` (its index
  line).

  A value object: treat instances as immutable. A plain class, not a frozen
  dataclass — the memory CLI verbs are fresh processes, and the
  ``dataclasses`` import pulls ``inspect`` (~9 ms of the M98 wall) for
  machinery no consumer calls.
  """

  __slots__ = ("delivery", "source_ref", "text")

  def __init__(self, source_ref: str, delivery: str, text: str) -> None:
    self.source_ref = source_ref
    self.delivery = delivery
    self.text = text

  def __eq__(self, other: object) -> bool:
    if not isinstance(other, MemoryEntrySource):
      return NotImplemented
    return (self.source_ref, self.delivery, self.text) == (other.source_ref, other.delivery, other.text)

  def __hash__(self) -> int:
    return hash((self.source_ref, self.delivery, self.text))


class MemorySelection:
  """The provenance-bearing memory assembly result one audience selection produced.

  ``text`` is the exact block selected for the audience — every caller that
  needs the string reads it here, never re-filtering. ``segments`` is the
  ordered (full-then-index) split the block is joined from, so a snapshot can
  label what the model actually received per delivery mode. ``usage_line``
  rides only on worker selections.

  A value object: treat instances as immutable; same plain-class reason as
  :class:`MemoryEntrySource`.
  """

  __slots__ = ("audience", "repo_basename", "segments", "text", "usage_line")

  def __init__(
      self, audience: str, repo_basename: str | None, segments: tuple[tuple[str, str, tuple[MemoryEntrySource, ...]],
                                                                      ...], usage_line: str | None,
      text: str | None) -> None:
    self.audience = audience
    self.repo_basename = repo_basename
    self.segments = segments  # (delivery, text, sources)
    self.usage_line = usage_line
    self.text = text

  def __eq__(self, other: object) -> bool:
    if not isinstance(other, MemorySelection):
      return NotImplemented
    return (self.audience, self.repo_basename, self.segments, self.usage_line,
            self.text) == (other.audience, other.repo_basename, other.segments, other.usage_line, other.text)

  def __hash__(self) -> int:
    return hash((self.audience, self.repo_basename, self.segments, self.usage_line, self.text))


WORKER_USAGE_LINE = (
    "On-demand knowledge: `charliebot memory query --topic <topic>` (full text) or `--index` "
    "for the index only. Stage a capture with `charliebot memory add [--file F]`: a capture "
    "is one file, first line `# <title>`, stating one fact to record or one change to "
    "propose, naming the target entry in the body when proposing a change (writes staging/, "
    "never entries/).")


def _selection_from_parts(
    audience: str,
    repo_basename: str | None,
    full_body_entries: list[Entry],
    index_entries: list[Entry],
    *,
    usage_line: str | None,
) -> MemorySelection:
  """Build the selection: one full segment, one index segment, stable sorts, exact text."""
  full_body_entries.sort(key=entry_order_key)
  index_entries.sort(key=entry_order_key)
  segments: list[tuple[str, str, tuple[MemoryEntrySource, ...]]] = []
  if full_body_entries:
    segments.append(
        (
            "full",
            "\n\n".join(full_text(e) for e in full_body_entries),
            tuple(
                MemoryEntrySource(source_ref=f"memory:{e.id}", delivery="full", text=full_text(e))
                for e in full_body_entries),
        ))
  if index_entries:
    segments.append(
        (
            "index",
            _index_lines(index_entries),
            tuple(
                MemoryEntrySource(
                    source_ref=f"memory:{e.id}", delivery="index", text=f"{e.topic}/{e.slug} · {e.title}")
                for e in index_entries),
        ))
  if usage_line is not None:
    # The worker usage line rides the index segment (it is query guidance); a
    # selection with no index entries gets a usage-only segment so the joined
    # text still equals the legacy string byte for byte.
    if segments and segments[-1][0] == "index":
      delivery, text, sources = segments[-1]
      segments[-1] = (delivery, f"{text}\n\n{usage_line}", sources)
    else:
      segments.append(("index", usage_line, ()))
  return MemorySelection(
      audience=audience,
      repo_basename=repo_basename,
      segments=tuple(segments),
      usage_line=usage_line,
      text="\n\n".join(s[1] for s in segments) if segments else None,
  )


def select_master_memory(memory_dir: pathlib.Path) -> MemorySelection | None:
  """Select the master-audience memory for a spawn (the provenance-bearing result).

  Full bodies of entries in resident topics whose audience contains ``master``,
  then the index lines for all other master-audience entries, each group
  stably sorted by ``(topic, slug)``. Returns None when the store has no
  master-audience entries to inject. A malformed store propagates
  :class:`MemoryFormatError` (fail-loud).
  """
  store = load_store(memory_dir)
  resident_names = resident_topic_names(store)
  full_body_entries: list[Entry] = []
  index_entries: list[Entry] = []
  for e in store.entries:
    if not audience_allows(e, "master"):
      continue
    if e.topic in resident_names:
      full_body_entries.append(e)
    else:
      index_entries.append(e)
  if not full_body_entries and not index_entries:
    return None
  return _selection_from_parts("master", None, full_body_entries, index_entries, usage_line=None)


def select_worker_memory(memory_dir: pathlib.Path, repo_basename: str) -> MemorySelection:
  """Select the worker-audience memory for *repo_basename* (the provenance-bearing result).

  Full bodies of entries whose topic equals *repo_basename* and whose audience
  contains ``worker``, then index lines for all other worker-audience entries,
  then the worker usage line, which keeps the selection non-empty. A malformed
  store propagates :class:`MemoryFormatError` (fail-loud). Staging candidates
  never enter: :func:`load_store` reads ``entries/`` only.
  """
  store = load_store(memory_dir)
  full_body_entries: list[Entry] = []
  index_entries: list[Entry] = []
  for e in store.entries:
    if not audience_allows(e, "worker"):
      continue
    if e.topic == repo_basename:
      full_body_entries.append(e)
    else:
      index_entries.append(e)
  return _selection_from_parts("worker", repo_basename, full_body_entries, index_entries, usage_line=WORKER_USAGE_LINE)


def entry_order_key(entry: Entry) -> tuple[str | None, str]:
  """The store's canonical entry order: ``(topic, slug)``.

  Every listing of entries — both prompt selections and the CLI query — sorts
  with this key, so one change moves them all.
  """
  return (entry.topic, entry.slug)


def resident_topic_names(store: Store) -> set[str]:
  """Names of the store's resident topics.

  Master prompt selection injects resident-topic entries in full and serves the
  rest as index lines; the CLI query's ``--resident`` filter matches the same
  set. Worker prompt selection does not consult residency — it splits on
  ``topic == repo_basename``.
  """
  return {t.name for t in store.topics.values() if t.resident}

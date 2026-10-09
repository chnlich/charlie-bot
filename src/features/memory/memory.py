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
``worker``), and ``title``. Only the first header block is parsed, so the
body may contain ``---`` lines.

All logic lives here; the CLI (``src/features/memory/cli.py``) is a thin wrapper, and task
prompt snapshots use the selection functions directly.

The store creates its own scaffold: :func:`ensure_store` runs first in every entry
point that reads or writes the live store (:func:`load_store`, ``memory add``, the
``memory proposal`` verbs), so a fresh home has no store until its first use.
:func:`lint` reports the tree as it finds it and never calls it.
"""

import dataclasses
import pathlib
import re
import subprocess
import threading
from collections.abc import Callable

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

# Header line: ``field: value`` where field is lower_snake. Value charset is
# validated per field below (slug-charset for most, free text for ``title``).
_HEADER_RE = re.compile(r"^([a-z_]+): (.+)$")
# Topic vocabulary line: ``name`` or ``name resident``.
_TOPIC_LINE_RE = re.compile(r"^([a-z0-9][a-z0-9-]*)( resident)?$")
# Topic name: the store's namespace charset for entry header fields.
TOPIC_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")
# Slug charset (entry filename stem / header value charset).
_SLUG_RE = re.compile(r"^[A-Za-z0-9._-]+$")
# Audience value: comma list of slug-charset elements.
_AUDIENCE_VALUE_RE = re.compile(r"^[A-Za-z0-9._-]+( *, *[A-Za-z0-9._-]+)*$")

_KNOWN_FIELDS = frozenset({"scope", "topic", "audience", "title"})
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


@dataclasses.dataclass
class Topic:
  """One vocabulary line: the topic name and whether it is resident."""

  name: str
  resident: bool


@dataclasses.dataclass
class Entry:
  """One parsed entry file.

  ``audience`` is the comma list parsed out of the frontmatter value. ``title``
  is the frontmatter ``title``.
  """

  path: pathlib.Path
  topic: str | None
  slug: str
  scope: str | None
  audience: list[str] | None
  title: str
  body: str

  @property
  def id(self) -> str:
    return f"{self.topic}/{self.slug}"


@dataclasses.dataclass
class Store:
  """One loaded store: the memory dir, the topics vocabulary, the parsed entries."""

  memory_dir: pathlib.Path
  topics: dict[str, Topic]
  entries: list[Entry]


def _parse_audience(raw: str) -> list[str]:
  """Split a comma-list audience value."""
  return [part.strip() for part in raw.split(",")]


def parse_entry(path: pathlib.Path) -> Entry:
  """Parse one entry file into an :class:`Entry`, or raise :class:`MemoryFormatError`.

  Structural validation only: the ``---`` framing, header line format, known
  field names, and per-field value charsets. Semantic checks (required fields,
  topic vocabulary membership, value domains) are done by :func:`load_store`
  and :func:`lint`. The body may contain ``---`` lines; only the first header
  block is parsed.
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
  if "title" not in header:
    raise MemoryFormatError(f"{entry_path}: missing header field 'title'")
  audience = header.get("audience")
  return Entry(
      path=entry_path,
      topic=header.get("topic"),
      slug=entry_path.stem,
      scope=header.get("scope"),
      audience=_parse_audience(audience) if audience is not None else None,
      title=header["title"],
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


def _validate_entry(entry: Entry, topics: dict[str, Topic]) -> list[str]:
  """Return a list of semantic violations for an entries/ *entry* (empty = valid).

  Requires ``scope``/``audience``, topic vocabulary membership, and a matching
  directory name.
  """
  topic_label = entry.topic or "?"

  def v(msg: str) -> str:
    return f"{_ENTRIES_DIRNAME}/{topic_label}/{entry.slug}.md: {msg}"

  violations: list[str] = []
  if not _SLUG_RE.match(entry.slug):
    violations.append(v(f"filename slug {entry.slug!r} does not match slug charset [A-Za-z0-9._-]"))
  if not entry.topic:
    violations.append(v("missing required header field 'topic'"))
  elif not TOPIC_NAME_RE.match(entry.topic):
    violations.append(v(f"topic {entry.topic!r} is not a valid topic name"))
  if entry.topic and entry.topic not in topics:
    violations.append(v(f"topic {entry.topic!r} not in topics vocabulary"))
  parent_name = entry.path.parent.name
  if entry.topic and parent_name != entry.topic:
    violations.append(v(f"directory name {parent_name!r} != topic {entry.topic!r}"))
  violations.extend(
      v(f"missing required header field {field!r}") for field in ("scope", "audience") if getattr(entry, field) is None)
  if entry.scope is not None and entry.scope not in _SCOPES:
    violations.append(v(f"scope {entry.scope!r} not in {{user, host}}"))
  violations.extend(_audience_violations(entry, v))
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


def _seed_if_missing(path: pathlib.Path, content: str) -> None:
  """Write content to path only if the file does not already exist."""
  if not path.exists():
    path.write_text(content, encoding="utf-8")


# Each launch's prompt assembly reaches this on its own worker thread, so first-use calls overlap. A
# ``git init`` that starts while another runs in the same directory exits 128: git creates its template
# files and ``.git/config.lock`` with exclusive opens. The lock makes the ``.git`` check and the
# ``git init`` one step.
_ENSURE_STORE_LOCK = threading.Lock()


def ensure_store(memory_dir: pathlib.Path) -> None:
  """Create the labeled-entry store scaffold at *memory_dir* (idempotent, thread-safe).

  Creates the directory, runs ``git init`` when it is not already a repo, seeds
  the topics vocabulary and .gitignore (never overwriting existing files), and
  creates the ``entries/`` and ``staging/`` directories. The canon (entries/
  and topics) is populated only by user-approved curation diffs, never here.
  Concurrent calls in one process run one after the other; a call that returns
  leaves the whole scaffold in place.
  """
  with _ENSURE_STORE_LOCK:
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
  charset, a missing ``title`` header, or an unknown header field all raise
  :class:`MemoryFormatError`.
  """
  ensure_store(memory_dir)
  topics = _load_topics(memory_dir)
  entries: list[Entry] = []
  for md_file in _iter_entry_files(memory_dir):
    entry = parse_entry(md_file)
    violations = _validate_entry(entry, topics)
    if violations:
      raise MemoryFormatError(violations[0])
    entries.append(entry)
  return Store(memory_dir=memory_dir, topics=topics, entries=entries)


def lint(memory_dir: pathlib.Path) -> list[str]:
  """Return all store violations (empty = clean).

  Validates entries/ against the entry grammar. staging/ files are free-form
  captures, each valid iff it is non-empty and its first line is a non-empty
  ``# <title>``.
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
    violations.extend(_validate_entry(entry, topics))
  staging_dir = memory_dir / _STAGING_DIRNAME
  if staging_dir.is_dir():
    for md_file in sorted(staging_dir.glob("*.md")):
      text = md_file.read_text(encoding="utf-8")
      first_line = text.split("\n", 1)[0]
      if not text:
        violations.append(f"{md_file}: empty capture file")
      elif not first_line.startswith("# "):
        violations.append(f"{md_file}: line 1: capture must start with '# <title>'")
      elif not first_line[2:].strip():
        violations.append(f"{md_file}: line 1: empty title after '# '")
  return violations


def full_text(entry: Entry) -> str:
  """The entry's presentable full text: ``# {title}`` + blank line + body.

  A body that already opens with ``# `` is returned as-is so its own heading
  is not duplicated. Trailing newlines are stripped.
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


@dataclasses.dataclass(frozen=True)
class MemoryEntrySource:
  """One selected entry's provenance and contribution.

  ``source_ref`` names the store origin (``memory:<topic>/<slug>``); the slug
  identifies the entry file under ``entries/<topic>/``. ``delivery`` is the
  form actually injected: ``full`` (whole entry text) or ``index`` (its index
  line).
  """

  source_ref: str
  delivery: str  # "full" | "index"
  text: str


@dataclasses.dataclass(frozen=True)
class MemorySelection:
  """The provenance-bearing memory assembly result one audience selection produced.

  ``text`` is the exact block selected for the audience — every caller that
  needs the string reads it here, never re-filtering. ``segments`` is the
  ordered (full-then-index) split the block is joined from, so a snapshot can
  label what the model actually received per delivery mode. ``usage_line``
  rides only on worker selections.
  """

  audience: str
  repo_basename: str | None
  segments: tuple[tuple[str, str, tuple[MemoryEntrySource, ...]], ...]  # (delivery, text, sources)
  usage_line: str | None
  text: str | None


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

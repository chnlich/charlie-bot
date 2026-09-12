"""Replay manifest: the frozen-input format the replay runner reads (schema v1).

The manifest is the only file the replay pipeline interprets. It carries the
frozen candidate material, the current relevant entries as complete texts,
owning-document evidence, the admission guideline, and the pool of prior user
comments with approved before/after texts and provenance ids. Evaluation
metadata (scoring answers, holdout labels) has no home here — unknown keys are
rejected — and stays in separate files the runner never reads.

File format (YAML, one document):

  version: 1
  base_commit: <opaque label of the frozen official-memory base revision>
  topics: [<topic name>, ...]          # frozen topics vocabulary
  sources:
    - ref: <unique slug-charset id>
      kind: candidate | entry | guideline | document
      path: entries/<topic>/<slug>.md  # entry kind only; the diff path
      text: <frozen text>              # or `file:` (a path relative to the manifest)
      remember_request: true           # candidate kind only; optional
  feedback_examples:
    - comment_event: <unique opaque provenance id>
      comment_text: <original user comment>
      tags: [<principle tag>, ...]     # rebuildable retrieval index, never a user rule
      approved_change:                 # nullable
        approved_change_ref: <provenance id of the approved diff>
        before: <text before approval>
        after: <text after approval>
  themes:                              # optional explicit assignments
    <theme name>:
      principles: [<principle tag>, ...]
      candidate_refs: [<source ref>, ...]
      entry_refs: [<source ref>, ...]
      document_refs: [<source ref>, ...]

Without ``themes`` one implicit theme ``default`` groups every candidate with
every entry and document source. With it, every candidate is assigned exactly
once; entry and document sources left unassigned stay inert and are reported
as unused in the run record. Guideline sources are global and never assigned.
"""

import hashlib
import re
from dataclasses import dataclass, field
from pathlib import Path

import yaml
from pydantic import BaseModel, ConfigDict, ValidationError

from src.core.memory_replay.errors import ReplayManifestError

MANIFEST_VERSION = 1

# A store-relative entry path: exactly entries/<topic>/<slug>.md, both segments in their
# store charsets. The same shape doubles as the traversal guard for every path a model
# returns and every entry path a manifest declares.
ENTRY_PATH_RE = re.compile(r"^entries/([a-z0-9][a-z0-9-]*)/([A-Za-z0-9._-]+)\.md$")
REF_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
TOPIC_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")

SOURCE_KINDS = ("candidate", "entry", "guideline", "document")


class _StrictModel(BaseModel):
  """Manifest-section base: unknown keys are rejected so evaluation metadata
  cannot ride along inside the manifest the runner sends to models."""

  model_config = ConfigDict(extra="forbid")


class SourceSpec(_StrictModel):
  ref: str
  kind: str
  path: str | None = None
  text: str | None = None
  file: str | None = None
  remember_request: bool = False


class ApprovedChangeSpec(_StrictModel):
  approved_change_ref: str
  before: str
  after: str


class FeedbackSpec(_StrictModel):
  comment_event: str
  comment_text: str
  tags: list[str] = []
  approved_change: ApprovedChangeSpec | None = None


class ThemeSpec(_StrictModel):
  principles: list[str] = []
  candidate_refs: list[str] = []
  entry_refs: list[str] = []
  document_refs: list[str] = []


class ManifestSpec(_StrictModel):
  version: int
  base_commit: str
  topics: list[str]
  sources: list[SourceSpec]
  feedback_examples: list[FeedbackSpec] = []
  themes: dict[str, ThemeSpec] | None = None


@dataclass
class ApprovedChange:
  approved_change_ref: str
  before: str
  after: str


@dataclass
class FeedbackExample:
  comment_event: str
  comment_text: str
  tags: list[str]
  approved_change: ApprovedChange | None


@dataclass
class Source:
  ref: str
  kind: str
  text: str
  path: str | None
  sha256: str
  # Absolute path when the text came from an external frozen file; part of the
  # frozen-input set the output root must not overlap. None for inline text.
  file: Path | None = None
  remember_request: bool = False


@dataclass
class Theme:
  name: str
  principles: list[str]
  candidate_refs: list[str]
  entry_refs: list[str]
  document_refs: list[str]


@dataclass
class Manifest:
  base_commit: str
  topics: list[str]
  sources: list[Source]
  feedback_examples: list[FeedbackExample]
  themes: list[Theme]

  _by_ref: dict[str, Source] = field(init=False, repr=False, default_factory=dict)
  _theme_of_candidate: dict[str, Theme] = field(init=False, repr=False, default_factory=dict)

  def __post_init__(self) -> None:
    self._by_ref = {s.ref: s for s in self.sources}
    for theme in self.themes:
      for ref in theme.candidate_refs:
        self._theme_of_candidate[ref] = theme

  def source(self, ref: str) -> Source | None:
    return self._by_ref.get(ref)

  @property
  def base_paths(self) -> set[str]:
    return {s.path for s in self.sources if s.kind == "entry"}

  def base_entries(self) -> dict[str, str]:
    """The frozen official-memory base: entry path -> complete current text."""
    return {s.path: s.text for s in self.sources if s.kind == "entry"}

  def theme_entry_paths(self, theme: Theme) -> dict[str, Source]:
    return {self._by_ref[ref].path: self._by_ref[ref] for ref in theme.entry_refs}

  def theme_sources(self, theme: Theme, kind: str) -> list[Source]:
    refs = {"candidate": theme.candidate_refs, "entry": theme.entry_refs, "document": theme.document_refs}[kind]
    return sorted((self._by_ref[ref] for ref in refs), key=lambda s: s.ref)

  def guidelines(self) -> list[Source]:
    return sorted((s for s in self.sources if s.kind == "guideline"), key=lambda s: s.ref)

  def frozen_files(self) -> list[Path]:
    """Every frozen input file: the manifest is prepended by the caller."""
    return [s.file for s in self.sources if s.file is not None]

  def unused_refs(self) -> list[str]:
    """Entry/document refs no theme assigned (inert evidence, reported in the run record)."""
    assigned = {ref for t in self.themes for ref in t.entry_refs + t.document_refs}
    return sorted(s.ref for s in self.sources if s.kind in ("entry", "document") and s.ref not in assigned)


def load_manifest(path: Path) -> Manifest:
  """Read and validate one replay manifest; raise :class:`ReplayManifestError` on any violation."""
  try:
    raw_text = path.read_text(encoding="utf-8")
  except OSError as e:
    raise ReplayManifestError(f"cannot read replay manifest {path}: {e}") from e
  try:
    data = yaml.safe_load(raw_text)
  except yaml.YAMLError as e:
    raise ReplayManifestError(f"replay manifest {path} is not valid YAML: {e}") from e
  if not isinstance(data, dict):
    raise ReplayManifestError(f"replay manifest {path} must be a YAML mapping")
  try:
    spec = ManifestSpec.model_validate(data)
  except ValidationError as e:
    raise ReplayManifestError(f"replay manifest {path} is invalid: {e}") from e
  if spec.version != MANIFEST_VERSION:
    raise ReplayManifestError(
        f"replay manifest {path}: unsupported version {spec.version} (this runner reads version "
        f"{MANIFEST_VERSION})")
  if not spec.base_commit.strip():
    raise ReplayManifestError(f"replay manifest {path}: base_commit must be a non-empty label")
  _check_topics(spec, path)
  sources = _load_sources(spec, path)
  feedback = _load_feedback(spec, path)
  themes = _load_themes(spec, sources, path)
  return Manifest(
      base_commit=spec.base_commit.strip(),
      topics=spec.topics,
      sources=sources,
      feedback_examples=feedback,
      themes=themes)


def _check_topics(spec: ManifestSpec, path: Path) -> None:
  if not spec.topics:
    raise ReplayManifestError(f"replay manifest {path}: topics must list at least one topic name")
  seen: set[str] = set()
  for name in spec.topics:
    if not TOPIC_RE.match(name):
      raise ReplayManifestError(f"replay manifest {path}: topic {name!r} is not a valid topic name")
    if name in seen:
      raise ReplayManifestError(f"replay manifest {path}: duplicate topic {name!r}")
    seen.add(name)


def _load_sources(spec: ManifestSpec, path: Path) -> list[Source]:
  sources: list[Source] = []
  seen_refs: set[str] = set()
  seen_paths: set[str] = set()
  for item in spec.sources:
    if not REF_RE.match(item.ref):
      raise ReplayManifestError(f"replay manifest {path}: source ref {item.ref!r} is not a valid ref")
    if item.ref in seen_refs:
      raise ReplayManifestError(f"replay manifest {path}: duplicate source ref {item.ref!r}")
    seen_refs.add(item.ref)
    if item.kind not in SOURCE_KINDS:
      raise ReplayManifestError(
          f"replay manifest {path}: source {item.ref!r} has kind {item.kind!r} "
          f"(expected one of {', '.join(SOURCE_KINDS)})")
    if (item.text is None) == (item.file is None):
      raise ReplayManifestError(f"replay manifest {path}: source {item.ref!r} needs exactly one of `text` or `file`")
    if item.file is not None:
      file_path = Path(item.file).expanduser()
      if not file_path.is_absolute():
        file_path = path.parent / file_path
      try:
        text = file_path.read_text(encoding="utf-8")
      except OSError as e:
        raise ReplayManifestError(f"replay manifest {path}: source {item.ref!r} file is unreadable: {e}") from e
    else:
      file_path = None
      text = item.text
    if not text.strip():
      raise ReplayManifestError(f"replay manifest {path}: source {item.ref!r} has empty text")
    item_path = None
    if item.kind == "entry":
      item_path = _check_entry_source(item, seen_paths, path)
    elif item.path is not None:
      raise ReplayManifestError(f"replay manifest {path}: source {item.ref!r} (kind {item.kind}) must not carry `path`")
    sources.append(
        Source(
            ref=item.ref,
            kind=item.kind,
            text=text,
            path=item_path,
            sha256=hashlib.sha256(text.encode("utf-8")).hexdigest(),
            file=file_path,
            remember_request=item.remember_request))
  if not any(s.kind == "candidate" for s in sources):
    raise ReplayManifestError(f"replay manifest {path}: no candidate sources; a replay needs frozen candidates")
  if not any(s.kind == "guideline" for s in sources):
    raise ReplayManifestError(
        f"replay manifest {path}: no guideline source; the admission policy is part of the frozen inputs")
  return sources


def _check_entry_source(item: SourceSpec, seen_paths: set[str], path: Path) -> str:
  item_path = item.path or ""
  match = ENTRY_PATH_RE.match(item_path)
  if match is None:
    raise ReplayManifestError(
        f"replay manifest {path}: entry source {item.ref!r} needs a path shaped "
        f"entries/<topic>/<slug>.md (got {item_path!r})")
  if item.path in seen_paths:
    raise ReplayManifestError(f"replay manifest {path}: duplicate entry path {item.path!r}")
  seen_paths.add(item.path)
  return item.path


def _load_feedback(spec: ManifestSpec, path: Path) -> list[FeedbackExample]:
  feedback: list[FeedbackExample] = []
  seen: set[str] = set()
  for item in spec.feedback_examples:
    if not REF_RE.match(item.comment_event):
      raise ReplayManifestError(
          f"replay manifest {path}: feedback comment_event {item.comment_event!r} is not a valid ref")
    if item.comment_event in seen:
      raise ReplayManifestError(f"replay manifest {path}: duplicate comment_event {item.comment_event!r}")
    seen.add(item.comment_event)
    if not item.comment_text.strip():
      raise ReplayManifestError(f"replay manifest {path}: feedback {item.comment_event!r} has empty comment_text")
    for tag in item.tags:
      if not REF_RE.match(tag):
        raise ReplayManifestError(f"replay manifest {path}: feedback {item.comment_event!r} has invalid tag {tag!r}")
    approved = None
    if item.approved_change is not None:
      change = item.approved_change
      if not REF_RE.match(change.approved_change_ref):
        raise ReplayManifestError(
            f"replay manifest {path}: feedback {item.comment_event!r} has invalid approved_change_ref "
            f"{change.approved_change_ref!r}")
      if not change.before.strip() or not change.after.strip():
        raise ReplayManifestError(
            f"replay manifest {path}: feedback {item.comment_event!r} approved_change needs "
            "non-empty before and after texts")
      approved = ApprovedChange(
          approved_change_ref=change.approved_change_ref, before=change.before, after=change.after)
    feedback.append(
        FeedbackExample(
            comment_event=item.comment_event,
            comment_text=item.comment_text,
            tags=list(item.tags),
            approved_change=approved))
  return feedback


def _load_themes(spec: ManifestSpec, sources: list[Source], path: Path) -> list[Theme]:
  by_ref = {s.ref: s for s in sources}
  candidates = sorted(s.ref for s in sources if s.kind == "candidate")
  if spec.themes is None:
    return [
        Theme(
            name="default",
            principles=[],
            candidate_refs=candidates,
            entry_refs=sorted(s.ref for s in sources if s.kind == "entry"),
            document_refs=sorted(s.ref for s in sources if s.kind == "document"))
    ]
  themes: list[Theme] = []
  assigned_candidates: list[str] = []
  assigned_evidence: set[str] = set()
  for name, item in spec.themes.items():
    if not REF_RE.match(name):
      raise ReplayManifestError(f"replay manifest {path}: theme name {name!r} is not a valid ref")
    if not item.candidate_refs:
      raise ReplayManifestError(f"replay manifest {path}: theme {name!r} assigns no candidates")
    for tag in item.principles:
      if not REF_RE.match(tag):
        raise ReplayManifestError(f"replay manifest {path}: theme {name!r} has invalid principle {tag!r}")
    for ref in item.candidate_refs:
      if ref not in by_ref or by_ref[ref].kind != "candidate":
        raise ReplayManifestError(
            f"replay manifest {path}: theme {name!r} candidate ref {ref!r} is not a candidate source")
      assigned_candidates.append(ref)
    for kind, refs in (("entry", item.entry_refs), ("document", item.document_refs)):
      for ref in refs:
        if ref not in by_ref or by_ref[ref].kind != kind:
          raise ReplayManifestError(f"replay manifest {path}: theme {name!r} {kind}_ref {ref!r} is not a {kind} source")
        if ref in assigned_evidence:
          raise ReplayManifestError(f"replay manifest {path}: source {ref!r} is assigned to more than one theme")
        assigned_evidence.add(ref)
    themes.append(
        Theme(
            name=name,
            principles=list(item.principles),
            candidate_refs=list(item.candidate_refs),
            entry_refs=list(item.entry_refs),
            document_refs=list(item.document_refs)))
  missing = sorted(set(candidates) - set(assigned_candidates))
  if missing:
    raise ReplayManifestError(f"replay manifest {path}: candidates {', '.join(missing)} are not assigned to any theme")
  duplicated = sorted(ref for ref in assigned_candidates if assigned_candidates.count(ref) > 1)
  if duplicated:
    raise ReplayManifestError(
        f"replay manifest {path}: candidates {', '.join(sorted(set(duplicated)))} are assigned to more than one theme")
  return sorted(themes, key=lambda t: t.name)

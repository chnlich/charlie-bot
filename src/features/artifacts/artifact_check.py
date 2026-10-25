"""Artifact check — the single owner of "genre -> assertion set -> probe".

An artifact genre (plan, understanding, sitrep, debug, explain) maps to an assertion set
here and nowhere else: adding a genre means registering its assertion set in this module,
naming it in ``src.features.artifacts.constants.ARTIFACT_GENRES`` (the vocabulary the artifact CLI parses;
the import-time equality check below fails a missed step), and giving it a template mapping
in ``src.features.artifacts.artifact_shared.GENRE_TEMPLATES`` (set-equality with the registry is pinned by
test_artifact_check, so a missed entry fails there, not as a KeyError when style-verbatim
reads the template). Each assertion is the DOM-decidable
half of the genre's GRAMMAR; rules needing judgment stay with the reader and the probe.

``run_assertions`` runs every applicable assertion and returns one printable outcome per
line — never stopping at the first failure. Structure parsing uses the standard library's
``html.parser``; only ``page-height`` shells out to headless chrome, so every other
assertion runs on a host without a renderer. ``run_probe`` sends the page text plus the
cold-read eight questions (``prompts/cold_read.md``, their only home) to CharlieBot's
backends.pool members in rotation order and returns the attempts, the
answering backend, and its verbatim answer; every genre's delivery gate routes through it
after the assertions pass.

The goal-length and page-height measurements (the goal budget, the page-height target, and
the headless-chrome probe page) live here as the goal-budget / page-height assertions; the
plan registration gate (src/features/artifacts/plans.py) enforces exactly the set
``run_assertions("plan", ...)`` returns.

``ordinal-named`` is the lexical check for the master prompt's Naming rule: a label pointing
off the page carries a content name at first use.

``run-stated`` makes a page that shows a decimal in a table cell state what its run changed and
what it held fixed, each with a value, in another table or dl. ``req-chips`` requires each span.req
chip's own sentence to carry readable text after the chip: its gloss.
"""

from __future__ import annotations

import dataclasses
import hashlib
import html
import pathlib
import re
import uuid
from collections.abc import Callable, Iterator
from html import parser
from typing import TYPE_CHECKING

from src.features.artifacts import artifact_shared, constants, plan_diff
from src.infra import constants as infra_constants
from src.infra import text_length
from src.runtime import cold_read

if TYPE_CHECKING:
  from src.infra import config

# One name per assertion: the outcome name a check stamps, its _ASSERTION_RUNNERS key, and its
# _ASSERTION_SETS member are the same string, so the registry and the genre sets build on these
# constants and a typo fails at import instead of as a KeyError when that check runs.
STYLE_VERBATIM = "style-verbatim"
SECTIONS_NUMBERED = "sections-numbered"
FOOT_PRESENT = "foot-present"
EXPLAIN_TRIAD = "explain-triad"
FORK_OPEN_SHAPE = "fork-open-shape"
FORK_EXPLAINER = "fork-explainer"
FACT_ANCHORED = "fact-anchored"
REQ_CHIPS = "req-chips"
RUN_STATED = "run-stated"
BYTE_INTEGRITY = "byte-integrity"
RENDER_PATH = "render-path"
GOAL_BUDGET = "goal-budget"
PAGE_HEIGHT = "page-height"
ORDINAL_NAMED = "ordinal-named"

# ---------------------------------------------------------------------------
# Budgets and measurements
# ---------------------------------------------------------------------------

GOAL_WEIGHTED_BUDGET = 240

_GOAL_SECTION_RE = re.compile(r"Problem / Goal</h2>(.*?)</section>", re.DOTALL)
_TAG_RE = re.compile(r"<[^>]+>")
# Revision marks ride outside the goal budget: the plan template clears them at the next revision,
# and the page-height probe hides these same two classes before measuring. Each is dropped whole,
# inline tags and content included; the two elements do not nest in practice.


def _revision_mark_re(tag: str, cls: str) -> re.Pattern[str]:
  """A regex dropping one revision-mark element whole; the class token must match exactly."""
  return re.compile(
      rf'<{tag}\b(?=[^>]*(?<!\S)class\s*=\s*'
      rf'(?:(?:"(?:[^"]*\s)?{cls}(?:\s[^"]*)?")|(?:\'(?:[^\']*\s)?{cls}(?:\s[^\']*)?\')|{cls}(?:\s|>))'
      rf')[^>]*>.*?</{tag}>', re.DOTALL)


_REVNOTE_RE = _revision_mark_re("div", "revnote")
_REVBADGE_RE = _revision_mark_re("span", "revbadge")


def _measure_goal_weighted(artifact: pathlib.Path) -> int:
  """Weighted length of the artifact's Problem / Goal section; a missing section raises."""
  section = _GOAL_SECTION_RE.search(artifact.read_text(encoding="utf-8"))
  if section is None:
    raise ValueError(f"artifact {artifact.name!r} has no 'Problem / Goal' section")
  body = section.group(1)
  for mark in (_REVNOTE_RE, _REVBADGE_RE):
    body = mark.sub("", body)
  text = html.unescape(_TAG_RE.sub("", body))
  return text_length.weighted_length(re.sub(r"\s+", " ", text).strip())


PAGE_HEIGHT_TARGET = 2000

_PAGE_PROBE_WIDTH_PX = 1280

# The probe loads the artifact in a fixed-width iframe over file://, hides the revision
# marks (revision badges and revnotes ride outside the measured height, per the plan
# template's Page budget rule), leaves details elements in their default collapsed state,
# then writes the artifact's measured height into its own DOM so the renderer reads it back.
_PAGE_PROBE_TEMPLATE = """<!doctype html>
<html><head><meta charset="utf-8">
<style>html,body{{margin:0;padding:0}}iframe{{width:{width}px;border:0;display:block}}</style>
</head><body>
<iframe id="artifact" src="{src}"></iframe>
<script>
  const frame = document.getElementById('artifact');
  frame.addEventListener('load', () => {{
    const doc = frame.contentDocument;
    for (const mark of doc.querySelectorAll('.revbadge,.revnote')) mark.style.display = 'none';
    const marker = document.createElement('pre');
    marker.id = 'page-height';
    marker.textContent = String(doc.documentElement.scrollHeight);
    document.body.appendChild(marker);
  }});
</script>
</body></html>
"""


def _measure_page_height(chrome_bin: pathlib.Path, artifact: pathlib.Path) -> int:
  """Render *artifact* headlessly through a session-unique probe page; return its scroll height."""
  from src.features.artifacts import headless_render  # deferred: charliebot plan --help
  probe = artifact.parent / f".page-height-probe-{uuid.uuid4().hex}.html"
  probe.write_text(
      _PAGE_PROBE_TEMPLATE.format(width=_PAGE_PROBE_WIDTH_PX, src=artifact.resolve().as_uri()), encoding="utf-8")
  try:
    return headless_render.render_height(chrome_bin, probe.as_uri())
  finally:
    probe.unlink()


def _require_chrome_bin(cfg: config.CharlieBotConfig) -> pathlib.Path:
  """Return the configured headless renderer path; raise when unset or unusable."""
  chrome_bin = cfg.headless_chrome_bin
  if not chrome_bin:
    raise ValueError(
        "headless_chrome_bin is required for plan registration: set it in the host config.yaml "
        "to the absolute path of a headless-chromium-compatible binary")
  chrome = pathlib.Path(chrome_bin)
  if not chrome.exists():
    raise ValueError(
        "headless_chrome_bin is required for plan registration but the configured path "
        f"does not exist: {chrome}")
  return chrome


# ---------------------------------------------------------------------------
# Minimal DOM over the standard-library HTML parser
# ---------------------------------------------------------------------------


class _Element:
  """DOM element: tag, attributes, class tokens, ordered children (elements and text chunks), parent link."""

  __slots__ = ("attrs", "children", "classes", "parent", "tag")

  def __init__(self, tag: str, attrs: dict, parent: _Element | None) -> None:
    self.tag = tag
    self.attrs = attrs
    self.classes = frozenset((attrs.get("class") or "").split())
    self.children: list = []
    self.parent = parent


class _TreeBuilder(parser.HTMLParser):

  def __init__(self) -> None:
    super().__init__(convert_charrefs=True)
    self.root = _Element("#root", {}, None)
    self._stack = [self.root]

  def handle_starttag(self, tag: str, attrs: list) -> None:
    el = _Element(tag, dict(attrs), self._stack[-1])
    self._stack[-1].children.append(el)
    if tag not in plan_diff.VOID_TAGS:
      self._stack.append(el)

  def handle_startendtag(self, tag: str, attrs: list) -> None:
    self._stack[-1].children.append(_Element(tag, dict(attrs), self._stack[-1]))

  def handle_endtag(self, tag: str) -> None:
    # Pop to the nearest open element with this tag; implicitly closes anything left open inside it.
    for i in range(len(self._stack) - 1, 0, -1):
      if self._stack[i].tag == tag:
        del self._stack[i:]
        return

  def handle_data(self, data: str) -> None:
    children = self._stack[-1].children
    if children and isinstance(children[-1], str):
      children[-1] += data
    else:
      children.append(data)


def _parse_dom(text: str) -> _Element:
  builder = _TreeBuilder()
  builder.feed(text)
  builder.close()
  return builder.root


def _descendants(el: _Element) -> Iterator[_Element]:
  for child in el.children:
    if isinstance(child, _Element):
      yield child
      yield from _descendants(child)


def _find(el: _Element, tag: str, classes: tuple = ()) -> list[_Element]:
  """Descendant elements (document order) with this tag carrying every listed class token."""
  wanted = frozenset(classes)
  return [d for d in _descendants(el) if d.tag == tag and wanted <= d.classes]


def _text(el: _Element) -> str:
  return "".join(child if isinstance(child, str) else _text(child) for child in el.children)


def _section_heading(el: _Element) -> str:
  """Text of the first h2 inside the nearest enclosing section; a placeholder when there is neither."""
  node = el.parent
  while node is not None:
    if node.tag == "section":
      for h2 in _find(node, "h2"):
        return " ".join(_text(h2).split())
      return "<unnamed section>"
    node = node.parent
  return "<no section>"


# ---------------------------------------------------------------------------
# Assertion outcomes and the per-assertion checks
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class AssertionOutcome:
  """One printed check line: the assertion name, pass/fail, and the measurement (pass) or failure
  location (fail). Element-scoped assertions return one outcome per offending element."""

  name: str
  passed: bool
  detail: str = ""


def _ok(name: str, detail: str = "") -> AssertionOutcome:
  return AssertionOutcome(name=name, passed=True, detail=detail)


def _fail(name: str, detail: str) -> AssertionOutcome:
  return AssertionOutcome(name=name, passed=False, detail=detail)


@dataclasses.dataclass
class _Context:
  genre: str
  artifact: pathlib.Path
  root: _Element
  cfg: config.CharlieBotConfig | None


# Numbered-h2 count per genre as (required, exact): plan/sitrep/debug/explain demand exactly their
# count; understanding demands at least its five mandatory blocks (the repo-tracked Linear block is optional).
_SECTION_COUNT_RULES = {
    "plan": (6, True),
    "understanding": (5, False),
    "sitrep": (5, True),
    "debug": (5, True),
    "explain": (5, True),
}

_FACT_ANCESTOR_TAGS = ("p", "li", "td", "blockquote", "h3")

_EXPLAINER_BODY_TAGS = frozenset({"li", "p", "table", "pre"})


def _check_style_verbatim(ctx: _Context) -> list[AssertionOutcome]:
  name = STYLE_VERBATIM
  page_styles = _find(ctx.root, "style")
  if len(page_styles) != 1:
    return [_fail(name, f"page carries {len(page_styles)} <style> blocks, expected exactly one")]
  template_rel = f"prompts/{artifact_shared.GENRE_TEMPLATES[ctx.genre]}"
  template_styles = _find(_parse_dom((infra_constants.REPO_ROOT / template_rel).read_text(encoding="utf-8")), "style")
  if len(template_styles) != 1:
    raise RuntimeError(f"genre template {template_rel} carries {len(template_styles)} <style> blocks, expected one")
  if " ".join(_text(page_styles[0]).split()) == " ".join(_text(template_styles[0]).split()):
    return [_ok(name)]
  return [_fail(name, f"<style> block differs from the genre template {template_rel}")]


def _check_sections_numbered(ctx: _Context) -> list[AssertionOutcome]:
  name = SECTIONS_NUMBERED
  numbered: list[tuple[_Element, str]] = []
  for h2 in _find(ctx.root, "h2"):
    numbers = _find(h2, "span", ("n",))
    if len(numbers) > 1:
      return [_fail(name, f"h2 {' '.join(_text(h2).split())!r} carries {len(numbers)} span.n numbers")]
    if numbers:
      numbered.append((h2, " ".join(_text(numbers[0]).split())))
  values: list[int] = []
  for h2, num_text in numbered:
    if not num_text.isdigit():
      return [_fail(name, f"h2 {' '.join(_text(h2).split())!r} span.n reads {num_text!r}, not an integer")]
    values.append(int(num_text))
  if values != list(range(1, len(values) + 1)):
    read = ", ".join(str(v) for v in values) or "(none)"
    return [_fail(name, f"section numbers read {read}, expected 1..{len(values)} contiguously")]
  required, exact = _SECTION_COUNT_RULES[ctx.genre]
  if exact and len(values) != required:
    return [_fail(name, f"{ctx.genre} requires exactly {required} numbered sections, found {len(values)}")]
  if not exact and len(values) < required:
    return [_fail(name, f"{ctx.genre} requires at least {required} numbered sections, found {len(values)}")]
  return [_ok(name)]


def _open_forks(root: _Element) -> list[tuple[int, _Element]]:
  """(1-based document index, div.fork) pairs for forks with no p.resolved descendant."""
  return [(i, f) for i, f in enumerate(_find(root, "div", ("fork",)), 1) if not _find(f, "p", ("resolved",))]


def _check_fork_open_shape(ctx: _Context) -> list[AssertionOutcome]:
  name = FORK_OPEN_SHAPE
  failures: list[AssertionOutcome] = []
  for i, fork in _open_forks(ctx.root):
    missing: list[str] = []
    if not any(_find(q, "span", ("fn",)) for q in _find(fork, "p", ("q",))):
      missing.append("p.q with span.fn")
    if not _find(fork, "p", ("rec",)):
      missing.append("p.rec")
    if not _find(fork, "p", ("trade",)):
      missing.append("p.trade")
    if missing:
      failures.append(_fail(name, f"fork #{i} (section {_section_heading(fork)!r}) is missing {', '.join(missing)}"))
  return failures or [_ok(name)]


def _check_fork_explainer(ctx: _Context) -> list[AssertionOutcome]:
  name = FORK_EXPLAINER
  failures: list[AssertionOutcome] = []
  for i, fork in _open_forks(ctx.root):
    layers = _find(fork, "details", ("details-layer",))
    if not layers:
      failures.append(_fail(name, f"fork #{i} (section {_section_heading(fork)!r}) has no details.details-layer"))
    elif not any(_text(el).strip() for el in _descendants(layers[0]) if el.tag in _EXPLAINER_BODY_TAGS):
      failures.append(_fail(name, f"fork #{i} (section {_section_heading(fork)!r}) explainer has no body"))
  return failures or [_ok(name)]


def _check_fact_anchored(ctx: _Context) -> list[AssertionOutcome]:
  name = FACT_ANCHORED
  failures: list[AssertionOutcome] = []
  for i, fact in enumerate(_find(ctx.root, "span", ("tag", "fact")), 1):
    block = fact.parent
    while block is not None and block.tag not in _FACT_ANCESTOR_TAGS:
      block = block.parent
    heading = _section_heading(fact)
    if block is None:
      failures.append(_fail(name, f"fact label #{i} (section {heading!r}) has no p/li/td/blockquote/h3 ancestor"))
    elif not _find(block, "span", ("src",)):
      failures.append(_fail(name, f"fact label #{i} (section {heading!r}) sits in a {block.tag} with no span.src"))
  return failures or [_ok(name)]


# ---------------------------------------------------------------------------
# req-chips and run-stated — the requirement gloss and the run's variable block
# ---------------------------------------------------------------------------

# Readable text carries a Unicode letter or digit; punctuation alone is not a gloss or a value.


def _has_readable_text(value: str) -> bool:
  """Return whether *value* contains a Unicode letter or digit."""
  return any(char.isalnum() for char in value)


# A requirement chip glosses in the block that holds it: the chip is fenced so the text after it
# can be read without guessing which occurrence of the chip's name came first.
_REQ_BLOCK_TAGS = frozenset({"p", "li", "td", "th", "dt", "dd", "blockquote", "h1", "h2", "h3", "h4"})
_REQ_START = "\x03"
_REQ_END = "\x04"
_SENTENCE_END_RE = re.compile(r"[.。!?！？]")

# run-stated reads a page that shows measured numbers: a table cell holding a decimal marks a
# reported run, and the page must then say which factors the run changed and which it held fixed.
# Role words match in English and Chinese on every page; the check never reads the page's lang.
_RUN_DECIMAL_RE = re.compile(r"\d+\.\d+")
_RUN_HELD_RE = re.compile(
    r"\b(?:held|kept|stayed|stays|remained|remains)\s+(?:the\s+)?(?:fixed|constant|unchanged|same)\b"
    r"|\b(?:fixed|constant|unchanged|retained|controlled|same)\b"
    r"|保持不变|固定不变|保持一致|不变|未变|没变|未改动|没改动|未改|没改|恒定|固定|控制变量", re.IGNORECASE)
_RUN_CHANGED_RE = re.compile(
    r"\b(?:changed?|changing|changes?|varied|varying|modified|altered|variable|tweaked)\b"
    r"|改(?:动|变|了|成|为)|更改|修改|变更|变量|变化", re.IGNORECASE)


def _marked_element_text(el: _Element, target: _Element) -> str:
  """Element text with *target* fenced by the request sentinels, so the text after it is unambiguous."""
  parts: list[str] = []

  def walk(node: _Element) -> None:
    for child in node.children:
      if child is target:
        parts.extend((_REQ_START, _text(child), _REQ_END))
      elif isinstance(child, str):
        parts.append(child)
      else:
        walk(child)

  walk(el)
  return "".join(parts)


def _check_req_chips(ctx: _Context) -> list[AssertionOutcome]:
  """Each span.req chip carries a gloss: readable text after the chip in the sentence that holds it.

  The gloss sits in the chip's own block and before that sentence's terminator, so a later
  sentence or a separate list or table does not gloss a chip left bare at the end of its sentence.
  """
  name = REQ_CHIPS
  chips = _find(ctx.root, "span", ("req",))
  if not chips:
    return [_fail(name, "no span.req requirement chips on the page")]
  failures: list[AssertionOutcome] = []
  for index, chip in enumerate(chips, 1):
    block = chip.parent
    while block is not None and block.tag not in _REQ_BLOCK_TAGS:
      block = block.parent
    heading = _section_heading(chip)
    chip_text = " ".join(_text(chip).split())
    if block is None:
      failures.append(
          _fail(
              name, f"requirement chip #{index} {chip_text!r} (section {heading!r}) has no "
              "p/li/td/blockquote ancestor to hold a gloss"))
      continue
    after = _marked_element_text(block, chip).split(_REQ_END, 1)[1]
    tail = _SENTENCE_END_RE.split(after, 1)[0]
    if not _has_readable_text(tail):
      failures.append(
          _fail(
              name, f"requirement chip #{index} {chip_text!r} (section {heading!r}) carries no gloss "
              "after it in its sentence"))
  return failures or [_ok(name, f"{len(chips)} requirement chips carry a gloss")]


def _table_rows(block: _Element) -> Iterator[list[str]]:
  """Text chunks per table row, one chunk per td/th cell; the role label and its value are among them."""
  for row in _find(block, "tr"):
    cells = [child for child in row.children if isinstance(child, _Element) and child.tag in ("td", "th")]
    if cells:
      yield [_text(cell) for cell in cells]


def _dl_rows(block: _Element) -> Iterator[list[str]]:
  """Text chunks per dl entry: the dt text followed by the dd texts that belong to it."""
  chunks: list[str] | None = None
  for el in _descendants(block):
    if el.tag == "dt":
      if chunks is not None:
        yield chunks
      chunks = [_text(el)]
    elif el.tag == "dd" and chunks is not None:
      chunks.append(_text(el))
  if chunks is not None:
    yield chunks


def _run_row_role(chunks: list[str]) -> str | None:
  """``changed`` or ``held`` when the row names that role and carries readable text besides it."""
  for role, pattern in (("held", _RUN_HELD_RE), ("changed", _RUN_CHANGED_RE)):
    for index, chunk in enumerate(chunks):
      match = pattern.search(chunk)
      if match is None:
        continue
      value = " ".join([chunk[:match.start()] + chunk[match.end():], *chunks[:index], *chunks[index + 1:]])
      if _has_readable_text(value):
        return role
  return None


def _check_run_stated(ctx: _Context) -> list[AssertionOutcome]:
  """A page that shows a decimal in a table cell states its run's changed and held-fixed factors.

  A dl is separate from the measured table. A table can state the factors when another
  decimal-bearing table is present, so numeric factor values remain valid. A block satisfies the
  assertion when one row names the changed role and another names the held-fixed role, each row
  carrying readable text besides the role word. The failure detail names the role row or same-block
  pairing that is missing.
  """
  name = RUN_STATED
  cells = _find(ctx.root, "td") + _find(ctx.root, "th")
  numeric_cells = [cell for cell in cells if _RUN_DECIMAL_RE.search(_text(cell))]
  if not numeric_cells:
    return [_ok(name, "no numeric table cell")]

  numeric_table_ids: set[int] = set()
  for cell in numeric_cells:
    table = cell.parent
    while table is not None and table.tag != "table":
      table = table.parent
    if table is not None:
      numeric_table_ids.add(id(table))

  observed_roles: set[str] = set()
  complete_block = False
  for block in _descendants(ctx.root):
    if block.tag not in ("table", "dl"):
      continue
    if id(block) in numeric_table_ids and len(numeric_table_ids) == 1:
      continue
    rows = _table_rows(block) if block.tag == "table" else _dl_rows(block)
    roles = {role for role in (_run_row_role(chunks) for chunks in rows) if role is not None}
    if {"changed", "held"} <= roles:
      complete_block = True
      break
    observed_roles.update(roles)
  if complete_block:
    return [_ok(name, "another table or dl states the changed and held-fixed factors")]

  missing = [
      label for role, label in (("changed", "changed-factor row"), ("held", "held-fixed-factor row"))
      if role not in observed_roles
  ]
  if not missing:
    missing = ["changed-factor row and held-fixed-factor row in the same block"]
  return [
      _fail(
          name, "a table cell holds a decimal number, but no other table or dl states the "
          f"{' and the '.join(missing)} with a value")
  ]


def _check_goal_budget(ctx: _Context) -> list[AssertionOutcome]:
  name = GOAL_BUDGET
  try:
    weighted = _measure_goal_weighted(ctx.artifact)
  except ValueError as e:
    return [_fail(name, str(e))]
  if weighted > GOAL_WEIGHTED_BUDGET:
    return [
        _fail(
            name, f"plan goal is {weighted} weighted chars (budget {GOAL_WEIGHTED_BUDGET}): keep the goal "
            "and non-goals; demote diagnosis, thresholds, paths, and justifications to Context or 4.1")
    ]
  return [_ok(name, f"{weighted} weighted chars (budget {GOAL_WEIGHTED_BUDGET})")]


def _check_page_height(ctx: _Context) -> list[AssertionOutcome]:
  """Measure the page's rendered height against PAGE_HEIGHT_TARGET; any measured height passes.

  The height is a report, not a gate: over the target the outcome names the overage, and
  only an unmeasurable page (no renderer, renderer error) fails.
  """
  name = PAGE_HEIGHT
  try:
    height = _measure_page_height(_require_chrome_bin(ctx.cfg), ctx.artifact)
  except ValueError as e:
    return [_fail(name, str(e))]
  if height > PAGE_HEIGHT_TARGET:
    return [_ok(name, f"{height} px: {height - PAGE_HEIGHT_TARGET} px over the {PAGE_HEIGHT_TARGET} px target")]
  return [_ok(name, f"{height} px (target {PAGE_HEIGHT_TARGET})")]


# ---------------------------------------------------------------------------
# ordinal-named — lexical check for the Naming rule
# ---------------------------------------------------------------------------

# An ordinal label is a bare numbered reference (plan 3, 第 2 节, Trade-off 5, 4.1). The Chinese
# label words are the language of the checked pages, so they stay literal.
ORD = re.compile(
    r"(?<![A-Za-z0-9_.])(?:(?:Trade-offs?|sections?|Section|fork|Fork|round|Round|"
    r"divergence|Divergence|question|Question|item|Item|plan|Plan)(?:\s+\(|\s*)(\d+)\)?(?!\w)"
    r"|第\s*([0-9一二三四五六七八九十]+)\s*(?:节|章|题|轮|条|项|问)"
    r"|分歧\s*([0-9一二三四五六七八九十]+)"
    r"|(?:(?<=§)|(?<=section )|(?<=Section ))\s*([1-9])\.(\d)(?![\d.%])"
    r"|(?<![\d.])([1-9])\.(\d)(?=\s*(?:节|Schema|Design Details|reading note))"
    r")")
# Labels that order the page's own sequence (rounds, items, questions) are internal unless a
# document word qualifies them.
INTERNAL_UNLESS_QUALIFIED = re.compile(r"round|Round|question|Question|item|Item|轮|条|项|题|问")
# Document words: a label directly after one of these points off the page.
QUAL = re.compile(
    r"(计划|plan|Plan|sitrep|战况|理解页|understanding|任务书|debug|explain|讲解页|上一?份|上一?版|"
    r"前一?份|那份|另一份|earlier|previous)(?:\s*的|\s*'s|\s*里|\s*中)?\s*$")
# Content-name forms: a sentence carrying one of these has a name in reach.
ANCHOR = re.compile(
    r"(https?://\S+"  # URL
    r"|(?<![\w/])(?:~|\.{1,2})?/[\w.~-]+(?:/[\w.~-]+)*"  # rooted path: /a, ~/a/b, ./a
    r"|\b[\w.~-]+(?:/[\w.~-]+){2,}"  # relative path with two or more slashes
    r"|\b[\w-]+\.(?:html|md|py|json|yaml|txt)\b"  # file name with a document extension
    r"|\b[A-Z]{2,}-\d+\b"  # ticket id
    r")")
SENT_SPLIT = re.compile(
    r"(?<=[。!?！？])")  # clauses joined by semicolons share one sentence: a name in one covers the other
QUOTE = re.compile(r"[「“][^」”]{1,80}[」”]|\"[^\"]{1,80}\"")  # a label inside quotes is a mention, not a use
GLOSS = re.compile(r"^\s*(?:v\d+\s*)?(?:[（(][^（）()]{2,}[）)]|·\s*\S{2,}|v\d+\s+[\w-]{6,})")
COLON_GLOSS = re.compile(r"^\s*(?:v\d+\s*)?[:：]\s*\S{2,}")
TITLE_ANCHOR = re.compile(r"(?:标题|题为|题目|titled?|named|called)\s*[:：]?\s*[「“][^」”]{2,}[」”]")

_CODE_START = "\x01"
_CODE_END = "\x02"

# Elements whose text the ordinal scan reads, one block per element; span.mtag is a header meta chip,
# the only block kind whose chip-value rule applies.
_ORDINAL_BLOCK_TAGS = frozenset(
    {"p", "li", "td", "th", "h1", "h2", "h3", "h4", "summary", "blockquote", "dt", "dd", "figcaption"})
# Literal-only subtrees: nothing inside them is scanned, not even nested blocks.
_ORDINAL_SKIP_TAGS = frozenset({"style", "script", "pre"})

_CJK_DIGITS = {c: i for i, c in enumerate("零一二三四五六七八九", 0)}


def _toint(n: str | None) -> int | None:
  """ASCII-digit or Chinese-numeral label number to int; unknown forms read as out of every range."""
  if n is None:
    return None
  if n.isdigit():
    return int(n)
  if n == "十":
    return 10
  if "十" in n:
    a, b = n.split("十")
    return (_CJK_DIGITS.get(a, 1) if a else 1) * 10 + (_CJK_DIGITS.get(b, 0) if b else 0)
  return _CJK_DIGITS.get(n, 99) if len(n) == 1 else 99


def _ordinal_label(kind: str) -> str:
  """Normalized label key for the named set: whitespace collapsed, ends trimmed."""
  return re.sub(r"\s+", " ", kind).strip()


def _ordinal_blocks(root: _Element) -> Iterator[_Element]:
  """Yield every ordinal-scan block element in document order: block tags plus span.mtag chips.
  Nested blocks are yielded after their enclosing block and again on their own."""

  def walk(el: _Element) -> Iterator[_Element]:
    for child in el.children:
      if not isinstance(child, _Element):
        continue
      if child.tag in _ORDINAL_SKIP_TAGS:
        continue
      if child.tag in _ORDINAL_BLOCK_TAGS or (child.tag == "span" and "mtag" in child.classes):
        yield child
      yield from walk(child)

  yield from walk(root)


def _block_scan_text(block: _Element) -> str:
  """A block's text in document order with <code> content fenced by sentinel chars: nested blocks
  (visited as their own blocks) and style/script/pre subtrees are excluded, code text is kept."""

  def walk(el: _Element) -> None:
    for child in el.children:
      if isinstance(child, str):
        parts.append(child)
      elif child.tag in _ORDINAL_SKIP_TAGS or child.tag in _ORDINAL_BLOCK_TAGS or (child.tag == "span" and
                                                                                   "mtag" in child.classes):
        continue
      elif child.tag == "code":
        parts.append(_CODE_START)
        walk(child)
        parts.append(_CODE_END)
      else:
        walk(child)

  parts: list[str] = []
  walk(block)
  return "".join(parts)


def split_code_spans(s: str) -> tuple[str, list[tuple[int, int]]]:
  """Strip the code sentinels, returning the plain text and the [start, end) ranges that were inside <code>."""
  out: list[str] = []
  spans: list[tuple[int, int]] = []
  start: int | None = None
  for ch in s:
    if ch == _CODE_START:
      start = len(out)
    elif ch == _CODE_END:
      if start is not None:
        spans.append((start, len(out)))
        start = None
    else:
      out.append(ch)
  if start is not None:
    spans.append((start, len(out)))
  return "".join(out), spans


def _ordinal_named_scan(ctx: _Context) -> tuple[set[str], list[tuple[_Element, list[str], str]]]:
  """Scan the page for bare ordinal labels pointing off it; return the set of labels that received a
  content name (carried forward across blocks in document order) and one (block, labels, sentence)
  triple per failing sentence."""

  def named_add(kind: str) -> None:
    named.add(_ordinal_label(kind))

  root = ctx.root
  h2_count = len(_find(root, "h2"))
  fork_count = len(_find(root, "div", ("fork",)))
  sn_set = {" ".join(_text(sn).split()) for sn in _find(root, "span", ("sn",))}
  named: set[str] = set()
  flagged: list[tuple[_Element, list[str], str]] = []
  for block in _ordinal_blocks(root):
    text = html.unescape(re.sub(r"\s+", " ", _block_scan_text(block)))
    for sentence in SENT_SPLIT.split(text):
      sentence = sentence.strip()
      if not sentence:
        continue
      sentence = QUOTE.sub("「…」", sentence)
      sentence, code_spans = split_code_spans(sentence)
      hits: list[str] = []
      for m in ORD.finditer(sentence):
        # A label match starting inside a code range is a mention; the code text still serves below.
        if any(start <= m.start() < end for start, end in code_spans):
          continue
        kind = m.group(0)
        n = next((g for g in m.groups()[:3] if g), None)
        before = sentence[:m.start()]
        external = bool(QUAL.search(before))
        own = False
        if not external:
          if INTERNAL_UNLESS_QUALIFIED.search(kind):
            continue
          dotted = (m.group(4), m.group(5)) if m.group(4) else ((m.group(6), m.group(7)) if m.group(6) else None)
          if dotted:
            own = f"{dotted[0]}.{dotted[1]}" in sn_set or 1 <= int(dotted[0]) <= h2_count
          elif re.search(r"节|章|section|Section", kind):
            own = n is not None and 0 <= _toint(n) <= h2_count  # 0 admits pages whose first section is numbered 0
          elif re.search(r"项|条|题|问|Trade|fork|Fork|divergence|Divergence|分歧|item|Item|question|Question", kind):
            own = n is not None and 1 <= _toint(n) <= max(fork_count, 0)
        if own:
          continue
        # Appositive: the label sits inside a parenthetical that follows a content run, e.g. 页面检查流水线（plan 3）.
        op = max(before.rfind("（"), before.rfind("("))
        cl = max(before.rfind("）"), before.rfind(")"))
        if op > cl and len(before[:op].strip()) >= 2:
          named_add(kind)
          continue
        if GLOSS.match(sentence[m.end():]) or (not before.strip(" -—·1234567890.") and
                                               COLON_GLOSS.match(sentence[m.end():])):
          named_add(kind)
          continue
        # Inside a header meta chip the text after the label is the chip's value.
        if block.tag == "span" and len(sentence[m.end():].strip(" ·")) >= 2:
          named_add(kind)
          continue
        hits.append(kind)
      if not hits:
        continue
      if ANCHOR.search(sentence) or TITLE_ANCHOR.search(sentence):
        for h in hits:
          named_add(h)
        continue
      rest = [h for h in hits if _ordinal_label(h) not in named]
      if rest:
        flagged.append((block, rest, sentence))
  return named, flagged


def _check_ordinal_named(ctx: _Context) -> list[AssertionOutcome]:
  name = ORDINAL_NAMED
  named, flagged = _ordinal_named_scan(ctx)
  if not flagged:
    return [_ok(name, f"{len(named)} external labels named in reach")]
  failures: list[AssertionOutcome] = []
  for block, labels, sentence in flagged:
    joined = labels[0] if len(labels) == 1 else ", ".join(labels)
    failures.append(
        _fail(
            name,
            f"(section {_section_heading(block)!r}) {joined!r} names something outside this page and no content name "
            f"is in reach; name it at first use (path, title, ticket, or a parenthetical): {sentence[:160]}"))
  return failures


def _check_byte_integrity(ctx: _Context) -> list[AssertionOutcome]:
  name = BYTE_INTEGRITY
  bad = artifact_shared.non_lf_control_bytes(ctx.artifact.read_bytes())
  if not bad:
    return [_ok(name)]
  return [_fail(name, f"{len(bad)} non-LF control bytes: {artifact_shared.named_control_bytes(bad)}")]


_KATEX_CSS_HINT = "katex.min.css"
_KATEX_JS_HINT = "katex.min.js"
_KATEX_AUTO_RENDER_HINT = "auto-render.min.js"


def _check_render_path(ctx: _Context) -> list[AssertionOutcome]:
  """A page carries a working math render path when it holds pre-rendered
  class="katex" markup (the ``artifact wrap`` product) or the full KaTeX script
  injection a hand-composed page copies from the genre template (katex.min.css
  link + katex.min.js script + auto-render.min.js script)."""
  name = RENDER_PATH
  if any("katex" in el.classes for el in _descendants(ctx.root)):
    return [_ok(name, 'pre-rendered class="katex" markup')]

  def carries(attr: str, hint: str) -> bool:
    return any(hint in (el.attrs.get(attr) or "") for el in _descendants(ctx.root))

  has_css = carries("href", _KATEX_CSS_HINT)
  has_katex_js = carries("src", _KATEX_JS_HINT)
  has_auto_render = carries("src", _KATEX_AUTO_RENDER_HINT)
  if has_css and has_katex_js and has_auto_render:
    return [_ok(name, "KaTeX script injection")]
  return [
      _fail(
          name, "no math render path: neither pre-rendered class=\"katex\" markup nor the KaTeX script "
          f"injection ({_KATEX_CSS_HINT} link + {_KATEX_JS_HINT} + {_KATEX_AUTO_RENDER_HINT})")
  ]


def _presence_check(
    name: str,
    tag: str,
    classes: tuple[str, ...],
    absent_detail: str,
) -> Callable[[_Context], list[AssertionOutcome]]:
  """Assertion that the page carries a ``tag`` with ``classes``; ``absent_detail`` is the miss message."""

  def check(ctx: _Context) -> list[AssertionOutcome]:
    if _find(ctx.root, tag, classes):
      return [_ok(name)]
    return [_fail(name, absent_detail)]

  return check


_ASSERTION_RUNNERS = {
    STYLE_VERBATIM: _check_style_verbatim,
    SECTIONS_NUMBERED: _check_sections_numbered,
    FOOT_PRESENT: _presence_check(FOOT_PRESENT, "div", ("foot",), "no div.foot on the page"),
    EXPLAIN_TRIAD: _presence_check(EXPLAIN_TRIAD, "div", ("triad",), "no div.triad on the page"),
    FORK_OPEN_SHAPE: _check_fork_open_shape,
    FORK_EXPLAINER: _check_fork_explainer,
    FACT_ANCHORED: _check_fact_anchored,
    REQ_CHIPS: _check_req_chips,
    RUN_STATED: _check_run_stated,
    GOAL_BUDGET: _check_goal_budget,
    PAGE_HEIGHT: _check_page_height,
    ORDINAL_NAMED: _check_ordinal_named,
    BYTE_INTEGRITY: _check_byte_integrity,
    RENDER_PATH: _check_render_path,
}

# The genre -> assertion-set table: the only place genres and their sets are stated.
# byte-integrity rides every genre set: mangled control bytes are genre-blind.
_ASSERTION_SETS: dict[str, tuple[str, ...]] = {
    "plan":
        (
            BYTE_INTEGRITY, STYLE_VERBATIM, SECTIONS_NUMBERED, FOOT_PRESENT, FORK_OPEN_SHAPE, FORK_EXPLAINER,
            GOAL_BUDGET, PAGE_HEIGHT, ORDINAL_NAMED),
    "understanding":
        (
            BYTE_INTEGRITY, STYLE_VERBATIM, SECTIONS_NUMBERED, FOOT_PRESENT, FORK_OPEN_SHAPE, FORK_EXPLAINER,
            PAGE_HEIGHT, ORDINAL_NAMED),
    "sitrep":
        (
            BYTE_INTEGRITY, STYLE_VERBATIM, SECTIONS_NUMBERED, FORK_OPEN_SHAPE, FORK_EXPLAINER, FACT_ANCHORED,
            REQ_CHIPS, RUN_STATED, ORDINAL_NAMED),
    "debug": (BYTE_INTEGRITY, STYLE_VERBATIM, SECTIONS_NUMBERED, FORK_OPEN_SHAPE, FACT_ANCHORED, ORDINAL_NAMED),
    "explain":
        (BYTE_INTEGRITY, STYLE_VERBATIM, RENDER_PATH, SECTIONS_NUMBERED, EXPLAIN_TRIAD, FORK_OPEN_SHAPE, ORDINAL_NAMED),
}

# The CLI parses GENRES from src.features.artifacts.constants (the artifact chain must not load this
# module to build its parser), so the registry and the parsed vocabulary must state the
# same genres; a registration that skips the constants tuple fails here, at import.
GENRES: tuple[str, ...] = constants.ARTIFACT_GENRES
if GENRES != tuple(_ASSERTION_SETS):
  raise ValueError(
      f"src.features.artifacts.constants.ARTIFACT_GENRES {GENRES} drifted from _ASSERTION_SETS "
      f"{tuple(_ASSERTION_SETS)}; name every registered genre in both")


def run_assertions(genre: str, artifact: pathlib.Path, cfg: config.CharlieBotConfig) -> list[AssertionOutcome]:
  """Run every assertion of *genre*'s set against *artifact*; return one outcome per printed line.

  Never stops at the first failure — a fix round clears every defect in one pass. A genre with
  no registered assertion set raises ValueError. The page-height genres (plan, understanding)
  read *cfg*; the others ignore it.
  """
  names = _ASSERTION_SETS.get(genre)
  if names is None:
    raise ValueError(f"no assertion set registered for genre {genre!r}; genres with a set: {', '.join(GENRES)}")
  ctx = _Context(genre=genre, artifact=artifact, root=_parse_dom(artifact.read_text(encoding="utf-8")), cfg=cfg)
  outcomes: list[AssertionOutcome] = []
  for name in names:
    outcomes.extend(_ASSERTION_RUNNERS[name](ctx))
  return outcomes


# ---------------------------------------------------------------------------
# Cold-read probe
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class ProbeResult:
  """Cold-read probe outcome: one (backend id, ``ExceptionType: message``) per tried backend, then the
  answering backend's id and its verbatim answer — both None when every backend failed — and the
  sha256 of the page bytes the probe read."""

  attempts: list[tuple[str, str]]
  backend_id: str | None
  answer: str | None
  page_sha256: str


def run_probe(cfg: config.CharlieBotConfig, artifact: pathlib.Path, trigger: str) -> ProbeResult:
  """Send the page's full text plus the seven-question prompt to the preferred light backends.

  The system prompt and the questions read from ``prompts/cold_read.md`` (their only home)
  through :mod:`src.runtime.cold_read`. Backends are tried from backends.pool in rotation order; each failure is
  recorded and the next member is tried.
  Returns the answering backend's id and answer, or a ProbeResult of attempts only when
  every backend failed.
  """
  system_prompt, questions = cold_read.load_texts(cfg.charlie_bot_repo)
  page_bytes = artifact.read_bytes()
  page_sha256 = hashlib.sha256(page_bytes).hexdigest()
  prompt = cold_read.probe_input(page_bytes.decode("utf-8"), questions, trigger)
  result = cold_read.run_light_probe(cfg, prompt, system_prompt)
  return ProbeResult(
      attempts=result.attempts,
      backend_id=result.backend_id,
      answer=result.answer,
      page_sha256=page_sha256,
  )

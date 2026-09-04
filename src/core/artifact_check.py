"""Artifact check — the single owner of "genre -> assertion set -> probe".

An artifact genre (plan, understanding, sitrep, debug, explain) maps to an assertion set
here and nowhere else: adding a genre means registering its assertion set in this module,
and nothing else in the codebase enumerates genres. Each assertion is the DOM-decidable
half of the genre's GRAMMAR; rules needing judgment stay with the reader and the probe.

``run_assertions`` runs every applicable assertion and returns one printable outcome per
line — never stopping at the first failure. Structure parsing uses the standard library's
``html.parser``; only ``page-height`` shells out to headless chrome, so every other
assertion runs on a host without a renderer. ``run_probe`` sends the page text plus the
cold-read seven questions to CharlieBot's preferred light backends (config model_preference
order) and returns the attempts, the answering backend, and its verbatim answer; every
genre's delivery gate routes through it after the assertions pass.

The goal-length and page-height measurements (with their budgets and the headless-chrome
probe page) live here as the goal-budget / page-height assertions; the plan registration
gate (src/core/plans.py) enforces exactly the set ``run_assertions("plan", ...)`` returns.

``ordinal-named`` is the lexical check for the master prompt's Naming rule: a label pointing
off the page carries a content name at first use.
"""

import asyncio
import dataclasses
import html
import re
import subprocess
import uuid
from html.parser import HTMLParser
from pathlib import Path

from src.agents.backends.registry import build_backend
from src.core.autonamer import iter_light_backends
from src.core.config import CharlieBotConfig
from src.core.timeouts import ARTIFACT_PROBE_TIMEOUT

# Repo root derived from this file: src/core/artifact_check.py -> parents[2] == repo root.
_REPO_ROOT = Path(__file__).resolve().parents[2]

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
# CJK ideographs, CJK punctuation, and fullwidth forms count double, so the same
# information density spends the same budget in Chinese and English.
_CJK_RANGES = ((0x3000, 0x303F), (0x4E00, 0x9FFF), (0xFF00, 0xFFEF))


def _weighted_goal_length(text: str) -> int:
  return len(text) + sum(1 for c in text if any(lo <= ord(c) <= hi for lo, hi in _CJK_RANGES))


def _measure_goal_weighted(artifact: Path) -> int:
  """Weighted length of the artifact's Problem / Goal section; a missing section raises."""
  section = _GOAL_SECTION_RE.search(artifact.read_text(encoding="utf-8"))
  if section is None:
    raise ValueError(f"artifact {artifact.name!r} has no 'Problem / Goal' section")
  body = section.group(1)
  for mark in (_REVNOTE_RE, _REVBADGE_RE):
    body = mark.sub("", body)
  text = html.unescape(_TAG_RE.sub("", body))
  return _weighted_goal_length(re.sub(r"\s+", " ", text).strip())


PAGE_HEIGHT_BUDGET = 1600

_PAGE_PROBE_WIDTH_PX = 1280
_RENDER_TIMEOUT_S = 60
_HEIGHT_MARKER_RE = re.compile(r'<pre id="page-height">(\d+)</pre>')

# The probe loads the artifact in a fixed-width iframe over file://, hides the revision
# marks (revision badges and revnotes ride outside the budget, per the plan template's
# Page budget rule), leaves details elements in their default collapsed state, then
# writes the artifact's measured height into its own DOM so --dump-dom hands it back.
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


def _measure_page_height(chrome_bin: Path, artifact: Path) -> int:
  """Render *artifact* headlessly through a session-unique probe page; return its scroll height."""
  probe = artifact.parent / f".page-height-probe-{uuid.uuid4().hex}.html"
  probe.write_text(
      _PAGE_PROBE_TEMPLATE.format(width=_PAGE_PROBE_WIDTH_PX, src=artifact.resolve().as_uri()), encoding="utf-8")
  try:
    try:
      proc = subprocess.run(
          [
              str(chrome_bin),
              "--headless",
              "--disable-gpu",
              "--no-sandbox",
              "--allow-file-access-from-files",
              "--virtual-time-budget=8000",
              "--dump-dom",
              probe.as_uri(),
          ],
          capture_output=True,
          check=False,
          timeout=_RENDER_TIMEOUT_S,
      )
    except subprocess.TimeoutExpired as e:
      raise ValueError(
          f"headless renderer timed out after {_RENDER_TIMEOUT_S}s while measuring the plan page height") from e
    except OSError as e:
      raise ValueError(f"headless renderer could not be launched: {chrome_bin} ({e})") from e
    if proc.returncode != 0:
      stderr = proc.stderr.decode("utf-8", errors="replace").strip()
      tail = stderr[-400:] if stderr else "no stderr output"
      raise ValueError(f"headless renderer exited {proc.returncode} while measuring the plan page height: {tail}")
    match = _HEIGHT_MARKER_RE.search(proc.stdout.decode("utf-8", errors="replace"))
    if match is None:
      raise ValueError("headless renderer output carried no page-height marker; cannot measure the plan page")
    return int(match.group(1))
  finally:
    probe.unlink()


def _require_chrome_bin(cfg: CharlieBotConfig) -> Path:
  """Return the configured headless renderer path; raise when unset or unusable."""
  chrome_bin = cfg.headless_chrome_bin
  if not chrome_bin:
    raise ValueError(
        "headless_chrome_bin is required for plan registration: set it in the host config.yaml "
        "to the absolute path of a headless-chromium-compatible binary")
  chrome = Path(chrome_bin)
  if not chrome.exists():
    raise ValueError(
        "headless_chrome_bin is required for plan registration but the configured path "
        f"does not exist: {chrome}")
  return chrome


# ---------------------------------------------------------------------------
# Minimal DOM over the standard-library HTML parser
# ---------------------------------------------------------------------------

_VOID_TAGS = frozenset(
    {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"})


class _Element:
  """DOM element: tag, class tokens, ordered children (elements and text chunks), parent link."""

  __slots__ = ("children", "classes", "parent", "tag")

  def __init__(self, tag: str, attrs: dict, parent: "_Element | None") -> None:
    self.tag = tag
    self.classes = frozenset((attrs.get("class") or "").split())
    self.children: list = []
    self.parent = parent


class _TreeBuilder(HTMLParser):

  def __init__(self) -> None:
    super().__init__(convert_charrefs=True)
    self.root = _Element("#root", {}, None)
    self._stack = [self.root]

  def handle_starttag(self, tag: str, attrs: list) -> None:
    el = _Element(tag, dict(attrs), self._stack[-1])
    self._stack[-1].children.append(el)
    if tag not in _VOID_TAGS:
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


def _descendants(el: _Element):
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
  artifact: Path
  root: _Element
  cfg: CharlieBotConfig | None


_GENRE_TEMPLATES = {
    "plan": "plan_template.html",
    "understanding": "plan_template.html",
    "sitrep": "sitrep_template.html",
    "debug": "debug_template.html",
    "explain": "explain_template.html",
}

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
  name = "style-verbatim"
  page_styles = _find(ctx.root, "style")
  if len(page_styles) != 1:
    return [_fail(name, f"page carries {len(page_styles)} <style> blocks, expected exactly one")]
  template_rel = f"prompts/{_GENRE_TEMPLATES[ctx.genre]}"
  template_styles = _find(_parse_dom((_REPO_ROOT / template_rel).read_text(encoding="utf-8")), "style")
  if len(template_styles) != 1:
    raise RuntimeError(f"genre template {template_rel} carries {len(template_styles)} <style> blocks, expected one")
  if " ".join(_text(page_styles[0]).split()) == " ".join(_text(template_styles[0]).split()):
    return [_ok(name)]
  return [_fail(name, f"<style> block differs from the genre template {template_rel}")]


def _check_sections_numbered(ctx: _Context) -> list[AssertionOutcome]:
  name = "sections-numbered"
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


def _check_foot_present(ctx: _Context) -> list[AssertionOutcome]:
  name = "foot-present"
  if _find(ctx.root, "div", ("foot",)):
    return [_ok(name)]
  return [_fail(name, "no div.foot on the page")]


def _check_explain_triad(ctx: _Context) -> list[AssertionOutcome]:
  name = "explain-triad"
  if _find(ctx.root, "div", ("triad",)):
    return [_ok(name)]
  return [_fail(name, "no div.triad on the page")]


def _open_forks(root: _Element) -> list[tuple[int, _Element]]:
  """(1-based document index, div.fork) pairs for forks with no p.resolved descendant."""
  return [(i, f) for i, f in enumerate(_find(root, "div", ("fork",)), 1) if not _find(f, "p", ("resolved",))]


def _check_fork_open_shape(ctx: _Context) -> list[AssertionOutcome]:
  name = "fork-open-shape"
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
  name = "fork-explainer"
  failures: list[AssertionOutcome] = []
  for i, fork in _open_forks(ctx.root):
    layers = _find(fork, "details", ("details-layer",))
    if not layers:
      failures.append(_fail(name, f"fork #{i} (section {_section_heading(fork)!r}) has no details.details-layer"))
    elif not any(_text(el).strip() for el in _descendants(layers[0]) if el.tag in _EXPLAINER_BODY_TAGS):
      failures.append(_fail(name, f"fork #{i} (section {_section_heading(fork)!r}) explainer has no body"))
  return failures or [_ok(name)]


def _check_fact_anchored(ctx: _Context) -> list[AssertionOutcome]:
  name = "fact-anchored"
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


def _check_req_chips(ctx: _Context) -> list[AssertionOutcome]:
  name = "req-chips"
  if _find(ctx.root, "span", ("req",)):
    return [_ok(name)]
  return [_fail(name, "no span.req requirement chips on the page")]


def _check_goal_budget(ctx: _Context) -> list[AssertionOutcome]:
  name = "goal-budget"
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
  name = "page-height"
  try:
    height = _measure_page_height(_require_chrome_bin(ctx.cfg), ctx.artifact)
  except ValueError as e:
    return [_fail(name, str(e))]
  if height > PAGE_HEIGHT_BUDGET:
    return [
        _fail(
            name, f"plan page measures {height} px as it opens: {height - PAGE_HEIGHT_BUDGET} px over the "
            f"{PAGE_HEIGHT_BUDGET} px budget")
    ]
  return [_ok(name, f"{height} px (budget {PAGE_HEIGHT_BUDGET})")]


# ---------------------------------------------------------------------------
# ordinal-named — lexical check for the Naming rule, ported from the prototype
# ---------------------------------------------------------------------------

# An ordinal label is a bare numbered reference (plan 3, 第 2 节, Trade-off 5, 4.1). The Chinese
# label words are the language of the checked pages and stay as the prototype wrote them.
ORD = re.compile(
    r"(?<![A-Za-z0-9_.])(?:(?:Trade-offs?|sections?|Section|fork|Fork|round|Round|divergence|Divergence|question|Question|item|Item|plan|Plan)(?:\s+\(|\s*)(\d+)\)?(?!\w)"
    r"|第\s*([0-9一二三四五六七八九十]+)\s*(?:节|章|题|轮|条|项|问)"
    r"|分歧\s*([0-9一二三四五六七八九十]+)"
    r"|(?:(?<=§)|(?<=section )|(?<=Section ))\s*([1-9])\.(\d)(?![\d.%])|(?<![\d.])([1-9])\.(\d)(?=\s*(?:节|Schema|Design Details|reading note))"
    r")")
# Labels that order the page's own sequence (rounds, items, questions) are internal unless a document word qualifies them.
INTERNAL_UNLESS_QUALIFIED = re.compile(r"round|Round|question|Question|item|Item|轮|条|项|题|问")
# Document words: a label directly after one of these points off the page.
QUAL = re.compile(
    r"(计划|plan|Plan|sitrep|战况|理解页|understanding|任务书|debug|explain|讲解页|上一?份|上一?版|前一?份|那份|另一份|earlier|previous)(?:\s*的|\s*'s|\s*里|\s*中)?\s*$"
)
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


def _ordinal_blocks(root: _Element):
  """Yield every ordinal-scan block element in document order: block tags plus span.mtag chips.
  Nested blocks are yielded after their enclosing block and again on their own."""

  def walk(el: _Element):
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
  name = "ordinal-named"
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


_ASSERTION_RUNNERS = {
    "style-verbatim": _check_style_verbatim,
    "sections-numbered": _check_sections_numbered,
    "foot-present": _check_foot_present,
    "explain-triad": _check_explain_triad,
    "fork-open-shape": _check_fork_open_shape,
    "fork-explainer": _check_fork_explainer,
    "fact-anchored": _check_fact_anchored,
    "req-chips": _check_req_chips,
    "goal-budget": _check_goal_budget,
    "page-height": _check_page_height,
    "ordinal-named": _check_ordinal_named,
}

# The genre -> assertion-set table: the only place genres and their sets are stated.
_ASSERTION_SETS: dict[str, tuple[str, ...]] = {
    "plan":
        (
            "style-verbatim", "sections-numbered", "foot-present", "fork-open-shape", "fork-explainer", "goal-budget",
            "page-height", "ordinal-named"),
    "understanding":
        (
            "style-verbatim", "sections-numbered", "foot-present", "fork-open-shape", "fork-explainer", "page-height",
            "ordinal-named"),
    "sitrep":
        (
            "style-verbatim", "sections-numbered", "fork-open-shape", "fork-explainer", "fact-anchored", "req-chips",
            "ordinal-named"),
    "debug": ("style-verbatim", "sections-numbered", "fork-open-shape", "fact-anchored", "ordinal-named"),
    "explain": ("style-verbatim", "sections-numbered", "explain-triad", "fork-open-shape", "ordinal-named"),
}

GENRES: tuple[str, ...] = tuple(_ASSERTION_SETS)


def run_assertions(genre: str, artifact: Path, cfg: CharlieBotConfig | None = None) -> list[AssertionOutcome]:
  """Run every assertion of *genre*'s set against *artifact*; return one outcome per printed line.

  Never stops at the first failure — a fix round clears every defect in one pass. A genre with
  no registered assertion set raises ValueError. ``cfg`` is required only for genres whose set
  includes page-height (plan, understanding).
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

_PROBE_SYSTEM_PROMPT = "You are a careful cold reader. Answer every question from the page alone."

# The single canonical copy of the cold-read seven-question prompt. skills/file-server/SKILL.md's
# Cold-Read Gate section points at this module instead of restating the text.
_PROBE_QUESTIONS = """You are reading one HTML page cold: the page source is the piped input, and you have no
context beyond the file itself. Answer seven questions, each in one or two sentences and in
the page's own language: (1) Whose problem does this page describe, and what is the
problem? (2) What is the page's conclusion, and what epistemic state does the page itself
claim for it (confirmed, hypothesis, refuted, or a stated mix)? (3) What does the page ask
of the reader, if anything? (4) In which numbered section did you first become clear on
what the problem is? (5) Name up to five points you had to re-read to follow the page.
(6) The chat message that triggered this page was: "<trigger message verbatim>".
Does the page answer that message, every part of it?
(7) List every term, abbreviation, or name the page uses without explaining it and that you could only guess at; write none when there is none."""


@dataclasses.dataclass
class ProbeResult:
  """Cold-read probe outcome: one (backend id, failure) per tried backend, then the answering
  backend's id and its verbatim answer — both None when every backend failed."""

  attempts: list[tuple[str, str]]
  backend_id: "str | None"
  answer: "str | None"


def run_probe(cfg: CharlieBotConfig, artifact: Path, trigger: str) -> ProbeResult:
  """Send the page's full text plus the seven-question prompt to the preferred light backends.

  Backends are tried in config model_preference order (iter_light_backends); each failure is
  recorded and the next backend is tried. Returns the answering backend's id and answer, or
  a ProbeResult of attempts only when every backend failed.
  """
  questions = _PROBE_QUESTIONS.replace("<trigger message verbatim>", trigger)
  prompt = f"{artifact.read_text(encoding='utf-8')}\n\n{questions}"
  options = list(iter_light_backends(cfg))
  if not options:
    raise ValueError("no light backends resolvable from config model_preference")
  attempts: list[tuple[str, str]] = []
  for option in options:
    try:
      answer = asyncio.run(
          build_backend(option, cfg).one_shot_text(prompt, _PROBE_SYSTEM_PROMPT, timeout=ARTIFACT_PROBE_TIMEOUT))
      return ProbeResult(attempts=attempts, backend_id=option.id, answer=answer)
    except Exception as e:
      attempts.append((option.id, str(e)))
  return ProbeResult(attempts=attempts, backend_id=None, answer=None)

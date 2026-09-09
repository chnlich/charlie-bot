import random
import re
from dataclasses import dataclass, field
from html import unescape
from html.parser import HTMLParser
from pathlib import Path

from src.core.plan_diff import annotate, diff_text

_ROOT = Path(__file__).resolve().parents[1]
_BLOCK_TAGS = {
    "address", "article", "aside", "blockquote", "body", "caption", "dd", "details", "dialog", "div", "dl", "dt",
    "fieldset", "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hgroup", "hr",
    "li", "main", "nav", "ol", "p", "pre", "section", "summary", "table", "tbody", "td", "tfoot", "th", "thead", "tr",
    "ul"
}
_IGNORED_TAGS = {"head", "style", "script", "template", "noscript", "title"}
_VOID_TAGS = {
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"
}


@dataclass
class _Element:
  tag: str
  attrs: dict[str, str | None]
  parent: "_Element | None"
  children: list["_Element | str"] = field(default_factory=list)

  def text(self) -> str:
    if self.tag in _IGNORED_TAGS:
      return ""
    return "".join(child if isinstance(child, str) else child.text() for child in self.children)


class _DomParser(HTMLParser):

  def __init__(self) -> None:
    super().__init__(convert_charrefs=True)
    self.root = _Element("#root", {}, None)
    self.stack = [self.root]

  def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
    node = _Element(tag, dict(attrs), self.stack[-1])
    self.stack[-1].children.append(node)
    if tag not in _VOID_TAGS:
      self.stack.append(node)

  def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
    self.stack[-1].children.append(_Element(tag, dict(attrs), self.stack[-1]))

  def handle_endtag(self, tag: str) -> None:
    for index in range(len(self.stack) - 1, 0, -1):
      if self.stack[index].tag == tag:
        del self.stack[index:]
        return

  def handle_data(self, data: str) -> None:
    self.stack[-1].children.append(data)


def _parse(html: str) -> _Element:
  parser = _DomParser()
  parser.feed(html)
  parser.close()
  return next((node for node in _descendants(parser.root) if node.tag == "body"), parser.root)


def _descendants(node: _Element):
  for child in node.children:
    if isinstance(child, _Element):
      yield child
      yield from _descendants(child)


def _direct_text(node: _Element) -> str:
  return "".join(child for child in node.children if isinstance(child, str))


def _commentable(node: _Element) -> bool:
  if node.tag not in _BLOCK_TAGS:
    return False
  if re.search(r"\S", _direct_text(node)):
    return True
  return node.tag in {"pre", "td", "th"} and bool(node.text().strip())


def _commentable_blocks(html: str) -> list[_Element]:
  return [node for node in _descendants(_parse(html)) if _commentable(node)]


def _quote(node: _Element) -> str:
  return re.sub(r"\s+", " ", node.text()).strip()[:400]


def _document_text(html: str) -> str:
  return _parse(html).text()


def _marks(html: str) -> list[tuple[str, re.Match[str]]]:
  marks: list[tuple[str, re.Match[str]]] = []
  ignored = [match.span() for match in re.finditer(r"<style\b[^>]*>.*?</style\s*>", html, re.IGNORECASE | re.DOTALL)]

  def in_ignored(match: re.Match[str]) -> bool:
    return any(start <= match.start() < end for start, end in ignored)

  for match in re.finditer(r"<ins\b[^>]*\bcbd-ins\b[^>]*>.*?</ins\s*>", html, re.IGNORECASE | re.DOTALL):
    if not in_ignored(match):
      marks.append(("ins", match))
  for match in re.finditer(
      r"<([A-Za-z][\w:-]*)\b(?=[^>]*\bcbd-del\b)(?=[^>]*\bdata-del\s*=\s*\"[^\"]*\")[^>]*>.*?</\1\s*>", html,
      re.IGNORECASE | re.DOTALL):
    if not in_ignored(match):
      marks.append(("del", match))
  for match in re.finditer(r"<([A-Za-z][\w:-]*)\b(?=[^>]*\bcbd-new\b)[^>]*>.*?</\1\s*>", html,
                           re.IGNORECASE | re.DOTALL):
    if not in_ignored(match):
      marks.append(("new", match))
  return sorted(marks, key=lambda item: item[1].start())


def _remove_mark(html: str, mark: tuple[str, re.Match[str]]) -> str:
  match = mark[1]
  return html[:match.start()] + html[match.end():]


def _restore(html: str) -> str:
  result = html
  while True:
    marks = _marks(result)
    if not marks:
      return result
    kind, match = marks[0]
    replacement = ""
    if kind == "del":
      data = re.search(r'\bdata-del\s*=\s*"([^"]*)"', match.group(0), re.IGNORECASE)
      assert data is not None
      replacement = unescape(data.group(1))
    result = result[:match.start()] + replacement + result[match.end():]


def _assert_invariants(base: str, new: str) -> str:
  annotated = annotate(base, new)
  clean_blocks = _commentable_blocks(new)
  marked_blocks = _commentable_blocks(annotated)
  assert len(clean_blocks) == len(marked_blocks)
  assert [_quote(node) for node in clean_blocks] == [_quote(node) for node in marked_blocks]
  assert _document_text(new) == _document_text(annotated)
  assert re.sub(r"\s+", " ", _document_text(base)).strip() == re.sub(r"\s+", " ",
                                                                     _document_text(_restore(annotated))).strip()
  for mark in _marks(annotated):
    without = _remove_mark(annotated, mark)
    assert (
        _document_text(without) != _document_text(new) or
        [_quote(node) for node in _commentable_blocks(without)] != [_quote(node) for node in clean_blocks] or
        re.sub(r"\s+", " ", _document_text(_restore(without))).strip() != re.sub(r"\s+", " ",
                                                                                 _document_text(base)).strip())
  return annotated


_SYNTHETIC_BASE = """\
<html><head><title>ignored</title></head><body>
<details id="outer"><summary>outer</summary>
  <details id="inner"><summary>inner</summary><p>alpha old beta</p></details>
</details>
<details id="unrelated"><summary>unrelated</summary><p>same text</p></details>
<div class="meta"><span class="mtag">chip v1</span></div>
</body></html>
"""
_SYNTHETIC_NEW = """\
<html><head><title>ignored</title></head><body>
<details id="outer"><summary>outer</summary>
  <details id="inner"><summary>inner</summary><p>alpha new beta</p></details>
</details>
<details id="unrelated"><summary>unrelated</summary><p>same text</p></details>
<div class="meta"><span class="mtag">chip v2</span></div>
</body></html>
"""


def test_four_invariants_hold_for_synthetic_pair() -> None:
  annotated = _assert_invariants(_SYNTHETIC_BASE, _SYNTHETIC_NEW)
  assert annotated.count('<ins class="cbd-ins">') == 2
  assert annotated.count('class="cbd-del"') == 2


def test_four_invariants_hold_for_real_fixture_pair() -> None:
  base = (_ROOT / "tests/data/plan_move2-direct-kill_v10.html").read_text(encoding="utf-8")
  new = (_ROOT / "tests/data/plan_move2-direct-kill_v11.html").read_text(encoding="utf-8")
  annotated = _assert_invariants(base, new)
  assert '<span class="cbd-del" data-del="v10"></span>' in annotated
  assert '<ins class="cbd-ins">v11</ins>' in annotated


def test_details_on_a_mark_path_open_and_unrelated_details_stay_closed() -> None:
  annotated = annotate(_SYNTHETIC_BASE, _SYNTHETIC_NEW)
  details = [node for node in _descendants(_parse(annotated)) if node.tag == "details"]
  assert ["open" in node.attrs for node in details] == [True, True, False]


def test_deleted_rows_and_list_items_stay_in_their_containers() -> None:
  base = "<html><body><ul><li>gone</li><li>kept</li></ul><table><tbody><tr><td>gone row</td></tr><tr><td>kept row</td></tr></tbody></table></body></html>"
  new = "<html><body><ul><li>kept</li></ul><table><tbody><tr><td>kept row</td></tr></tbody></table></body></html>"
  annotated = annotate(base, new)
  dom = _parse(annotated)
  ghosts = [node for node in _descendants(dom) if node.attrs.get("class") == "cbd-del"]
  assert [(node.tag, node.parent.tag if node.parent else None) for node in ghosts] == [("li", "ul"), ("tr", "tbody")]
  assert all(node.text() == "" for node in ghosts)
  assert 'colspan="1"' in annotated


def test_entirely_new_block_is_commentable_without_an_ins_wrapper() -> None:
  base = "<html><body><p>unchanged</p></body></html>"
  new = "<html><body><p>unchanged</p><p>new passage to comment</p></body></html>"
  annotated = annotate(base, new)
  assert '<p class="cbd-new">new passage to comment</p>' in annotated
  assert _quote(_commentable_blocks(annotated)[-1]) == "new passage to comment"
  assert '<ins class="cbd-ins">new passage to comment</ins>' not in annotated


def test_word_deletion_and_short_gap_merge_restore_the_base_text() -> None:
  merged_base = "<html><body><p>one two three four five</p></body></html>"
  merged_new = "<html><body><p>one TWO three FOUR five</p></body></html>"
  merged = _assert_invariants(merged_base, merged_new)
  assert 'data-del="two three four"' in merged
  assert '<ins class="cbd-ins">TWO three FOUR</ins>' in merged

  deletion_base = "<html><body><p>one old two</p></body></html>"
  deletion_new = "<html><body><p>one two</p></body></html>"
  deletion = _assert_invariants(deletion_base, deletion_new)
  assert 'data-del="old "' in deletion


def test_token_never_spans_a_text_node_boundary() -> None:
  base = '<html><body><p>alpha beta<b>gamma</b></p></body></html>'
  new = '<html><body><p>alpha beta</p></body></html>'
  annotated = annotate(base, new)
  assert 'data-del="gamma"' in annotated
  assert '<span class="cbd-del" data-del="betagamma"></span>' not in annotated
  assert '<ins class="cbd-ins">beta</ins>' not in annotated
  assert [kind for kind, _ in _marks(annotated)] == ["del"]


def test_cjk_tokens_stay_per_character_and_restore_the_base_text() -> None:
  base = '<html><body><p>中文 旧 文本</p><p>kept</p></body></html>'
  new = '<html><body><p>中文 新 文本</p><p>kept</p></body></html>'
  annotated = _assert_invariants(base, new)
  assert 'data-del="旧"' in annotated
  assert '<ins class="cbd-ins">新</ins>' in annotated


_CJK_REFERENCE_RANGES = ((0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xF900, 0xFAFF), (0x20000, 0x2FA1F))
_TOKEN_FUZZ_PIECES = [
    " ", "\n", "\t", "\u3000", "\xa0", "word", "x_1", "2", "中文", "ＣＫ", "！", "é", "-", "--", "...", "a&b", "😀"
]


def _reference_tokenise(text: str) -> list[tuple[str, int, int]]:
  tokens: list[tuple[str, int, int]] = []
  index = 0
  while index < len(text):
    char = text[index]
    if char.isspace():
      end = index + 1
      while end < len(text) and text[end].isspace():
        end += 1
      index = end
      continue
    value = ord(char)
    if any(start <= value <= end for start, end in _CJK_REFERENCE_RANGES):
      tokens.append((char, index, index + 1))
      index += 1
      continue
    if char.isascii() and (char.isalnum() or char == "_"):
      end = index + 1
      while end < len(text) and text[end].isascii() and (text[end].isalnum() or text[end] == "_"):
        end += 1
      tokens.append((text[index:end], index, end))
      index = end
      continue
    tokens.append((char, index, index + 1))
    index += 1
  return tokens


def test_tokeniser_matches_the_per_character_reference_on_a_randomized_corpus() -> None:
  from src.core.plan_diff import _tokenise

  rng = random.Random(20260908)
  for _ in range(500):
    text = "".join(rng.choice(_TOKEN_FUZZ_PIECES) for _ in range(rng.randint(0, 40)))
    assert _tokenise(text) == _reference_tokenise(text), f"token drift on {text!r}"


def test_leaf_token_raw_spans_match_the_per_character_range_reference() -> None:
  from src.core.plan_diff import _collect_leaves, _document_root, _Leaf, _leaf_tokens, _parse

  def reference(leaf: _Leaf) -> list[tuple[str, int, int, int, int]]:
    result: list[tuple[str, int, int, int, int]] = []
    offset = 0
    for part in leaf.parts:
      if part.text_is_raw:
        ranges = [(part.start + i, part.start + i + 1) for i in range(len(part.text))]
      else:
        ranges = [(part.start, part.end)] * len(part.text)
      for value, start, end in _reference_tokenise(part.text):
        result.append((value, offset + start, offset + end, ranges[start][0], ranges[end - 1][1]))
      offset += len(part.text)
    return result

  source = '<html><body><p>alpha &amp; beta</p><p>中文 text</p></body></html>'
  leaves = _collect_leaves(_document_root(_parse(source)))
  assert len(leaves) == 2
  for leaf in leaves:
    got = [(t.value, t.logical_start, t.logical_end, t.raw_start, t.raw_end) for t in _leaf_tokens(leaf)]
    assert got == reference(leaf), f"raw-span drift on leaf {leaf.text!r}"


def test_pure_inline_markup_move_with_unchanged_text_produces_no_marks() -> None:
  base = '<html><body><p>alpha beta<b>gamma</b></p></body></html>'
  new = '<html><body><p>alpha <b>beta</b>gamma</p></body></html>'
  assert not _marks(annotate(base, new))


def test_replaced_block_keeps_a_direct_text_node_and_stays_commentable() -> None:
  base = '<html><body><h2><span class="n">2</span> Context<span class="revbadge">changed · r4</span></h2></body></html>'
  new = '<html><body><h2><span class="n">2</span> Context</h2></body></html>'
  annotated = _assert_invariants(base, new)
  assert '<h2><span class="n">2</span> Context' in annotated
  headings = [node for node in _commentable_blocks(annotated) if node.tag == "h2"]
  assert len(headings) == 1
  assert 'data-del="changed · r4"' in annotated
  assert '<ins class="cbd-ins">Context</ins>' not in annotated

  replaced = _assert_invariants(
      '<html><body><h2><span class="n">2</span> Alpha Beta</h2></body></html>',
      '<html><body><h2><span class="n">2</span> Gamma Delta</h2></body></html>')
  assert '<h2 class="cbd-new"><span class="n">2</span> Gamma Delta</h2>' in replaced
  assert 'class="cbd-del" data-del="2 Alpha Beta"' in replaced


_BOUNDARY_BASE = """\
<html><body>
<h2><span class="n">2</span> Context<span class="revbadge">changed · r4</span></h2>
<p>alpha beta<b>gamma</b></p>
<p>old words here</p>
<p>unchanged</p>
<section id="risks">
<h2><span class="n">3</span> Risks<span class="revbadge">changed · r4</span></h2>
<div class="revnote">NOTE</div>
<p>section body kept</p>
</section>
</body></html>
"""
_BOUNDARY_NEW = """\
<html><body>
<h2><span class="n">2</span> Context</h2>
<p>alpha beta</p>
<p>fresh text now</p>
<p>unchanged</p>
<section id="risks">
<h2><span class="n">3</span> Risks</h2>
<p>section body kept</p>
</section>
</body></html>
"""


def test_four_invariants_hold_for_boundary_pair() -> None:
  annotated = _assert_invariants(_BOUNDARY_BASE, _BOUNDARY_NEW)
  assert 'data-del="changed · r4"' in annotated
  assert 'data-del="gamma"' in annotated
  assert '<p class="cbd-new">fresh text now</p>' in annotated
  assert '<ins class="cbd-ins">beta</ins>' not in annotated
  assert '<ins class="cbd-ins">Context</ins>' not in annotated
  assert len(_commentable_blocks(annotated)) == len(_commentable_blocks(_BOUNDARY_NEW))


def test_ghost_follows_a_heading_that_carries_its_own_inline_mark() -> None:
  new = '<html><body><h2>Head</h2><p>tail</p></body></html>'
  repro = '<html><body><h2>Head<span class="revbadge">X</span></h2><div class="revnote">NOTE</div><p>tail</p></body></html>'
  control = '<html><body><h2>Head</h2><div class="revnote">NOTE</div><p>tail</p></body></html>'
  for base, badge in ((control, False), (repro, True)):
    annotated = _assert_invariants(base, new)
    order = [
        (child.tag, child.attrs.get("data-del")) for child in _parse(annotated).children if isinstance(child, _Element)
    ]
    assert order == [("div", None), ("h2", None), ("div", "NOTE"), ("p", None)]
    assert ('data-del="X"' in annotated) == badge


def test_ghost_follows_the_heading_when_a_section_drops_badge_and_block_together() -> None:
  annotated = _assert_invariants(_BOUNDARY_BASE, _BOUNDARY_NEW)
  section = next(node for node in _descendants(_parse(annotated)) if node.attrs.get("id") == "risks")
  order = [(child.tag, child.attrs.get("class")) for child in section.children if isinstance(child, _Element)]
  assert order == [("h2", None), ("div", "cbd-del"), ("p", None)]
  # The ghost carries the whitespace that separated the note from the body
  # paragraph, so restoring it separates the returned text again.
  assert 'data-del="NOTE\n"' in annotated
  assert '<h2><span class="n">3</span> Risks<span class="cbd-del" data-del="changed · r4"></span></h2>' in annotated


def test_same_document_has_no_marks_and_diff_text_names_real_changes() -> None:
  base = (_ROOT / "tests/data/plan_move2-direct-kill_v10.html").read_text(encoding="utf-8")
  new = (_ROOT / "tests/data/plan_move2-direct-kill_v11.html").read_text(encoding="utf-8")
  same = annotate(base, base)
  assert not _marks(same)
  assert diff_text(base, base) == ""
  plain = diff_text(base, new)
  assert "header chip" in plain
  assert "Context" in plain
  assert "Trade-offs" in plain
  assert "v10" in plain and "v11" in plain


def test_replaced_direct_text_with_a_nested_block_stays_commentable() -> None:
  base = '<html><body><div>old direct <p>unchanged child</p></div></body></html>'
  new = '<html><body><div>new fresh <p>unchanged child</p></div></body></html>'
  annotated = _assert_invariants(base, new)
  assert '<div class="cbd-new">new fresh <p>unchanged child</p></div>' in annotated
  assert '<ins class="cbd-ins">new fresh</ins>' not in annotated


def test_style_and_header_splice_positions() -> None:
  base = '<html><head><title>t</title></head><body><p>alpha</p></body></html>'
  new = '<html><head><title>t</title></head><body><p>alpha beta</p></body></html>'
  annotated = annotate(base, new)
  assert annotated.index("<style data-cbd-style>") == annotated.rindex("</title>") + len("</title>")
  assert annotated.index('<div class="cbd-header"') == annotated.index("<body>") + len("<body>")

  headless_new = '<html><body><p>alpha beta</p></body></html>'
  annotated = annotate('<html><body><p>alpha</p></body></html>', headless_new)
  assert annotated.index("<style data-cbd-style>") == annotated.index("<html>") + len("<html>")

  fragment_new = '<div>alpha beta</div>'
  annotated = annotate('<div>alpha</div>', fragment_new)
  assert annotated.startswith("<style data-cbd-style>")
  assert '<div class="cbd-header"' in annotated


def _anchors_from_full_parse(source: str) -> tuple[tuple | None, tuple | None]:
  from src.core.plan_diff import _first_descendant, _Node, _parse

  parser = _parse(source)

  def quad(node: "_Node | None") -> tuple | None:
    return (node.start, node.start_end, node.end, node.end_end) if node is not None else None

  return quad(_first_descendant(parser.root, "head")), quad(_first_descendant(parser.root, "body"))


def _anchors_as_quads(anchors: tuple) -> tuple[tuple | None, tuple | None]:
  return tuple(
      None if anchor is None else (anchor.start, anchor.start_end, anchor.end, anchor.end_end) for anchor in anchors)


_FUZZ_TAGS = [
    "html", "head", "body", "div", "p", "span", "section", "table", "tr", "td", "ul", "li", "h1", "h2", "em", "strong",
    "code", "pre", "script", "style", "title", "meta", "br", "hr", "img"
]
_FUZZ_ATTRS = ["class", "id", "data-x", "style", "open"]
_FUZZ_ATTR_VALUES = ["a", "b c", "", "x>y", "a&amp;b"]


def _fuzz_document(rng: random.Random) -> str:
  pieces: list[str] = []
  stack: list[str] = []
  for _ in range(rng.randint(1, 14)):
    roll = rng.random()
    if roll < 0.10 and stack:
      tag = rng.choice(stack)
      pieces.append(f"</{tag}>")
      stack.remove(tag)
    elif roll < 0.16:
      pieces.append(rng.choice(["hello world", "alpha beta", "&amp; &lt;", "  \n  ", "e" * 5]))
    elif roll < 0.20:
      pieces.append(f"<!-- {rng.choice(['</head>', '<body>', '---', 'x'])} -->")
    elif roll < 0.26:
      tag = rng.choice(["script", "style"])
      pieces.append(f"<{tag}>{rng.choice(['</head> inside script', 'a < b', 'var x=1;'])}</{tag}>")
    elif roll < 0.30:
      pieces.append(f"<{rng.choice(_FUZZ_TAGS)}{_fuzz_attrs(rng)}/>")
    else:
      tag = rng.choice(_FUZZ_TAGS)
      pieces.append(f"<{tag}{_fuzz_attrs(rng)}>")
      if tag not in _VOID_TAGS:
        stack.append(tag)
  for tag in reversed(stack):
    if rng.random() < 0.7:
      pieces.append(f"</{tag}>")
  return "".join(pieces)


def _fuzz_attrs(rng: random.Random) -> str:
  count = rng.randint(0, 3)
  if not count:
    return ""
  return " " + " ".join(f'{rng.choice(_FUZZ_ATTRS)}="{rng.choice(_FUZZ_ATTR_VALUES)}"' for _ in range(count))


def test_boundary_anchors_match_the_full_parse_on_a_randomized_corpus() -> None:
  from src.core.plan_diff import _parse_anchors

  rng = random.Random(20260907)
  for _ in range(1500):
    doc = _fuzz_document(rng)
    assert _anchors_as_quads(_parse_anchors(doc)) == _anchors_from_full_parse(doc), f"anchor drift on {doc!r}"


def test_boundary_anchors_match_the_full_parse_on_the_fixture_pair_and_spliced_output() -> None:
  from src.core.plan_diff import _parse_anchors, annotate

  base = (_ROOT / "tests/data/plan_move2-direct-kill_v10.html").read_text(encoding="utf-8")
  new = (_ROOT / "tests/data/plan_move2-direct-kill_v11.html").read_text(encoding="utf-8")
  for source in (base, new, annotate(base, new)):
    assert _anchors_as_quads(_parse_anchors(source)) == _anchors_from_full_parse(source)

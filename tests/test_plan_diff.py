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


def test_pure_inline_markup_move_with_unchanged_text_produces_no_marks() -> None:
  base = '<html><body><p>alpha beta<b>gamma</b></p></body></html>'
  new = '<html><body><p>alpha <b>beta</b>gamma</p></body></html>'
  assert not _marks(annotate(base, new))


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

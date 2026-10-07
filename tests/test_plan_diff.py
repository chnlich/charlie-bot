import random
import re
from html import unescape

from conftest import ROOT

from src.features.artifacts.artifact_check import _descendants, _Element, _parse_dom
from src.features.artifacts.plan_diff import _BLOCK_TAGS, _IGNORED_TAGS, _first_descendant, _parse_anchors, annotate


def _parse(html: str) -> _Element:
  root = _parse_dom(html)
  return next((node for node in _descendants(root) if node.tag == "body"), root)


def _text(node: _Element) -> str:
  """The node's text with the ignored-tag subtrees (head, style, script, ...) dropped.

  The plan-differ invariants compare annotated pages against their sources;
  the pages the differ injects carry a CBD <style> whose CSS text must not
  count as document text, so the walk skips every _IGNORED_TAGS subtree.
  """
  if node.tag in _IGNORED_TAGS:
    return ""
  return "".join(child if isinstance(child, str) else _text(child) for child in node.children)


def _direct_text(node: _Element) -> str:
  return "".join(child for child in node.children if isinstance(child, str))


def _commentable(node: _Element) -> bool:
  if node.tag not in _BLOCK_TAGS:
    return False
  if re.search(r"\S", _direct_text(node)):
    return True
  return node.tag in {"pre", "td", "th"} and bool(_text(node).strip())


def _commentable_blocks(html: str) -> list[_Element]:
  return [node for node in _descendants(_parse(html)) if _commentable(node)]


def _quote(node: _Element) -> str:
  return re.sub(r"\s+", " ", _text(node)).strip()[:400]


def _document_text(html: str) -> str:
  return _text(_parse(html))


def _marks(html: str) -> list[tuple[str, re.Match[str]]]:
  marks: list[tuple[str, re.Match[str]]] = []
  ignored = [match.span() for match in re.finditer(r"<style\b[^>]*>.*?</style\s*>", html, re.IGNORECASE | re.DOTALL)]

  def in_ignored(match: re.Match[str]) -> bool:
    return any(start <= match.start() < end for start, end in ignored)

  marks.extend(
      ("ins", match)
      for match in re.finditer(r"<ins\b[^>]*\bcbd-ins\b[^>]*>.*?</ins\s*>", html, re.IGNORECASE | re.DOTALL)
      if not in_ignored(match))
  marks.extend(
      ("del", match) for match in re.finditer(
          r"<([A-Za-z][\w:-]*)\b(?=[^>]*\bcbd-del\b)(?=[^>]*\bdata-del\s*=\s*\"[^\"]*\")[^>]*>.*?</\1\s*>", html,
          re.IGNORECASE | re.DOTALL) if not in_ignored(match))
  marks.extend(
      ("new", match)
      for match in re.finditer(
          r"<([A-Za-z][\w:-]*)\b(?=[^>]*\bcbd-new\b)[^>]*>.*?</\1\s*>", html, re.IGNORECASE | re.DOTALL)
      if not in_ignored(match))
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


def _fixture_pair() -> tuple[str, str]:
  """The real captured plan page pair the whole-pipeline tests annotate: v10 base, v11 new."""
  data = ROOT / "tests/data"
  base = (data / "plan_move2-direct-kill_v10.html").read_text(encoding="utf-8")
  new = (data / "plan_move2-direct-kill_v11.html").read_text(encoding="utf-8")
  return base, new


def test_four_invariants_hold_for_real_fixture_pair() -> None:
  base, new = _fixture_pair()
  annotated = _assert_invariants(base, new)
  assert '<span class="cbd-del" data-del="v10"></span>' in annotated
  assert '<ins class="cbd-ins">v11</ins>' in annotated


def test_entirely_new_block_is_commentable_without_an_ins_wrapper() -> None:
  base = "<html><body><p>unchanged</p></body></html>"
  new = "<html><body><p>unchanged</p><p>new passage to comment</p></body></html>"
  annotated = annotate(base, new)
  assert '<p class="cbd-new">new passage to comment</p>' in annotated
  assert _quote(_commentable_blocks(annotated)[-1]) == "new passage to comment"
  assert '<ins class="cbd-ins">new passage to comment</ins>' not in annotated


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
  from src.features.artifacts.plan_diff import _tokenise

  rng = random.Random(20260908)
  for _ in range(500):
    text = "".join(rng.choice(_TOKEN_FUZZ_PIECES) for _ in range(rng.randint(0, 40)))
    assert _tokenise(text) == _reference_tokenise(text), f"token drift on {text!r}"


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


def _anchors_from_full_parse(source: str) -> tuple[tuple | None, tuple | None]:
  from src.features.artifacts.plan_diff import _Node, _parse

  parser = _parse(source)

  def quad(node: _Node | None) -> tuple | None:
    return (node.start, node.start_end, node.end, node.end_end) if node is not None else None

  return quad(_first_descendant(parser.root, "head")), quad(_first_descendant(parser.root, "body"))


def _anchors_as_quads(anchors: tuple) -> tuple[tuple | None, tuple | None]:
  return tuple(
      None if anchor is None else (anchor.start, anchor.start_end, anchor.end, anchor.end_end) for anchor in anchors)


def test_boundary_anchors_match_the_full_parse_on_the_fixture_pair_and_spliced_output() -> None:
  base, new = _fixture_pair()
  for source in (base, new, annotate(base, new)):
    assert _anchors_as_quads(_parse_anchors(source)) == _anchors_from_full_parse(source)

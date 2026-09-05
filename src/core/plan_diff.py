"""Word-level comparison and annotation for plan HTML artifacts.

The parser in this module deliberately keeps the source offsets of every text
piece.  An annotated artifact is therefore the new artifact with small source
splices, rather than a re-serialized DOM.  In particular, removed text is
stored only in ``data-del`` attributes; it never becomes a document text node.
"""

from __future__ import annotations

import difflib
import html as _html
import re
from dataclasses import dataclass, field
from html.parser import HTMLParser
from typing import Iterable

_IGNORED_TAGS = frozenset({"head", "style", "script", "template", "noscript", "title"})
_VOID_TAGS = frozenset(
    {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"})
_BLOCK_TAGS = frozenset(
    {
        "address", "article", "aside", "blockquote", "body", "caption", "dd", "details", "dialog", "div", "dl", "dt",
        "fieldset", "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hgroup",
        "hr", "li", "main", "nav", "ol", "p", "pre", "section", "summary", "table", "tbody", "td", "tfoot", "th",
        "thead", "tr", "ul"
    })
_CJK_RANGES = (
    (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF),
    (0xF900, 0xFAFF),
    (0x20000, 0x2FA1F),
)

_CBD_STYLE = """\
.cbd-header { margin: 0 0 14px; padding: 8px 12px; border: 1px solid #d9dfe6; border-radius: 7px; background: #fff8df; color: #7a5a00; font-weight: 600; }
.cbd-header::before { content: attr(data-cbd-header); }
ins.cbd-ins { background: rgba(46, 160, 67, .18); text-decoration: none; }
.cbd-del::after { content: attr(data-del); color: #cf222e; text-decoration: line-through; }
.cbd-new { background: rgba(46, 160, 67, .10); }
tr.cbd-del > td::after { content: attr(data-cbd-del); color: #cf222e; text-decoration: line-through; }
"""
_HEADER_TEXT = "本版 · 对比上一版"
_CLASS_RE = re.compile(r"(?<![\w:-])class\s*=\s*(?P<quote>[\"'])(?P<value>[^\"']*)(?P=quote)", re.IGNORECASE)
_UNQUOTED_CLASS_RE = re.compile(r"(?<![\w:-])class\s*=\s*(?P<value>[^\s>]+)", re.IGNORECASE)


@dataclass
class _TextPart:
  start: int
  end: int
  text: str
  raw_ranges: list[tuple[int, int]]
  node: "_Node"


@dataclass(eq=False)
class _Node:
  tag: str
  attrs: dict[str, str | None]
  parent: "_Node | None"
  start: int | None = None
  start_end: int | None = None
  end: int | None = None
  end_end: int | None = None
  children: list["_Node | _TextPart"] = field(default_factory=list)
  text_parts: list[_TextPart] = field(default_factory=list)


class _Parser(HTMLParser):
  """DOM builder retaining source offsets and decoded text ranges."""

  def __init__(self, source: str) -> None:
    super().__init__(convert_charrefs=False)
    self.source = source
    self.root = _Node("#root", {}, None)
    self._stack = [self.root]
    self._line_starts = [0]
    for match in re.finditer("\\n", source):
      self._line_starts.append(match.end())

  def _offset(self) -> int:
    line, column = self.getpos()
    return self._line_starts[line - 1] + column

  def _append_part(self, raw_start: int, raw_end: int, text: str, raw_ranges: list[tuple[int, int]]) -> None:
    node = self._stack[-1]
    part = _TextPart(raw_start, raw_end, text, raw_ranges, node)
    node.children.append(part)
    node.text_parts.append(part)

  def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
    node = self._open_node(tag, attrs)
    if tag not in _VOID_TAGS:
      self._stack.append(node)

  def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
    self._open_node(tag, attrs)

  def _open_node(self, tag: str, attrs: list[tuple[str, str | None]]) -> _Node:
    start = self._offset()
    raw = self.get_starttag_text()
    if raw is None or self.source[start:start + len(raw)] != raw:
      raise ValueError(f"could not locate start tag at offset {start}")
    node = _Node(tag, {name.lower(): value for name, value in attrs}, self._stack[-1], start, start + len(raw))
    self._stack[-1].children.append(node)
    return node

  def handle_endtag(self, tag: str) -> None:
    start = self._offset()
    end = self.source.find(">", start)
    if end < 0:
      raise ValueError(f"could not locate end tag at offset {start}")
    for index in range(len(self._stack) - 1, 0, -1):
      node = self._stack[index]
      if node.tag == tag:
        node.end = start
        node.end_end = end + 1
        del self._stack[index:]
        return

  def handle_data(self, data: str) -> None:
    start = self._offset()
    end = start + len(data)
    if self.source[start:end] != data:
      raise ValueError(f"could not locate text data at offset {start}")
    self._append_part(start, end, data, [(start + index, start + index + 1) for index in range(len(data))])

  def handle_entityref(self, name: str) -> None:
    self._append_ref(name, ("&" + name + ";",), 1)

  def handle_charref(self, name: str) -> None:
    self._append_ref(name, ("&#x" + name + ";", "&#" + name + ";"), 2)

  def _append_ref(self, name: str, refs: tuple[str, ...], base_punct: int) -> None:
    # The tokenizer consumes the reference body plus its opening punctuation
    # ("&" vs "&#") but reports the body alone; the trailing semicolon is part
    # of the source span only when present. Unescape must run over exactly the
    # consumed span, so the length rule and the splice offsets stay in one place.
    start = self._offset()
    raw = self.source[start:]
    length = len(name) + base_punct
    if any(raw.startswith(ref) for ref in refs):
      length += 1
    text = _html.unescape(self.source[start:start + length])
    self._append_part(start, start + length, text, [(start, start + length)] * len(text))


@dataclass
class _Leaf:
  element: _Node
  parts: list[_TextPart]
  whole: bool

  @property
  def text(self) -> str:
    return "".join(part.text for part in self.parts)


@dataclass(frozen=True)
class _Token:
  value: str
  logical_start: int
  logical_end: int
  raw_start: int
  raw_end: int


@dataclass
class _LeafChange:
  old: _Leaf | None
  new: _Leaf | None
  new_index: int | None


def _is_cjk(char: str) -> bool:
  value = ord(char)
  return any(start <= value <= end for start, end in _CJK_RANGES)


def _is_boundary(node: _Node) -> bool:
  if node.tag in _BLOCK_TAGS:
    return True
  classes = set((node.attrs.get("class") or "").split())
  if node.tag == "span" and classes.intersection({"mtag", "b"}):
    return True
  style = (node.attrs.get("style") or "").lower()
  display = re.search(r"(?:^|;)\s*display\s*:\s*([^;]+)", style)
  if display:
    value = display.group(1).strip()
    return value in {
        "block", "flow-root", "flex", "grid", "list-item", "table", "table-caption", "table-cell", "table-row"
    }
  return False


def _visible_parts(node: _Node, ignored: bool = False) -> list[_TextPart]:
  ignored = ignored or node.tag in _IGNORED_TAGS
  if ignored:
    return []
  result: list[_TextPart] = []
  for child in node.children:
    if isinstance(child, _TextPart):
      result.append(child)
    else:
      result.extend(_visible_parts(child, ignored))
  return result


def _has_visible_text(parts: Iterable[_TextPart]) -> bool:
  return any(part.text.strip() for part in parts)


def _collect_leaves(root: _Node) -> list[_Leaf]:
  leaves: list[_Leaf] = []

  def visit(node: _Node) -> None:
    all_parts = _visible_parts(node)
    if not _has_visible_text(all_parts):
      return
    # A row is kept as one diff leaf even though its cells are table-cell
    # blocks.  This is what lets a deleted row remain inside <tbody> and lets
    # its ghost carry one full-width cell.  The cells themselves remain in
    # the DOM, so the browser's TD fallback commentability is preserved.
    if node.tag == "tr":
      leaves.append(_Leaf(node, all_parts, True))
      return
    direct_parts = [child for child in node.children if isinstance(child, _TextPart)]
    block_children = [child for child in node.children if isinstance(child, _Node) and _is_boundary(child)]
    if block_children:
      if _has_visible_text(direct_parts):
        leaves.append(_Leaf(node, direct_parts, False))
      for child in node.children:
        if isinstance(child, _Node):
          visit(child)
      return
    leaves.append(_Leaf(node, all_parts, True))

  visit(root)
  return [leaf for leaf in leaves if _has_visible_text(leaf.parts)]


def _document_root(parser: _Parser) -> _Node:
  return _first_descendant(parser.root, "body") or parser.root


def _normalise(text: str) -> str:
  return re.sub(r"\s+", " ", text).strip()


def _tokenise(text: str) -> list[tuple[str, int, int]]:
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
    if _is_cjk(char):
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


def _leaf_tokens(leaf: _Leaf) -> list[_Token]:
  # Tokenise each text node separately: a token never spans a text-node
  # boundary, so tokens and source ranges agree where markup meets text with
  # no whitespace.  The tokens stay one sequence in leaf-text offsets, so
  # alignment still sees the whole concatenated leaf at once.
  result: list[_Token] = []
  offset = 0
  for part in leaf.parts:
    for value, start, end in _tokenise(part.text):
      result.append(
          _Token(
              value,
              offset + start,
              offset + end,
              part.raw_ranges[start][0],
              part.raw_ranges[end - 1][1],
          ))
    offset += len(part.text)
  return result


def _leaf_raw_map(leaf: _Leaf) -> tuple[list[tuple[int, int]], list[int]]:
  raw_ranges: list[tuple[int, int]] = []
  part_indices: list[int] = []
  for part_index, part in enumerate(leaf.parts):
    raw_ranges.extend(part.raw_ranges)
    part_indices.extend([part_index] * len(part.text))
  return raw_ranges, part_indices


def _merged_opcodes(old: list[_Token], new: list[_Token]) -> list[tuple[str, int, int, int, int]]:
  opcodes = difflib.SequenceMatcher(
      None, [token.value for token in old], [token.value for token in new], autojunk=False).get_opcodes()
  merged: list[tuple[str, int, int, int, int]] = []
  index = 0
  while index < len(opcodes):
    tag, old_start, old_end, new_start, new_end = opcodes[index]
    if tag == "equal":
      merged.append((tag, old_start, old_end, new_start, new_end))
      index += 1
      continue
    while index + 1 < len(opcodes) and opcodes[index + 1][0] != "equal":
      index += 1
      _, _, next_old_end, _, next_new_end = opcodes[index]
      old_end = next_old_end
      new_end = next_new_end
    while index + 2 < len(opcodes):
      equal = opcodes[index + 1]
      following = opcodes[index + 2]
      if equal[0] != "equal" or equal[2] - equal[1] >= 3:
        break
      old_end = following[2]
      new_end = following[4]
      index += 2
    merged.append(("replace", old_start, old_end, new_start, new_end))
    index += 1
  return merged


def _align(old: list[_Leaf], new: list[_Leaf]) -> list[_LeafChange]:
  old_keys = [_normalise(leaf.text) for leaf in old]
  new_keys = [_normalise(leaf.text) for leaf in new]
  result: list[_LeafChange] = []
  opcodes = difflib.SequenceMatcher(None, old_keys, new_keys, autojunk=False).get_opcodes()
  for tag, old_start, old_end, new_start, new_end in opcodes:
    if tag == "equal":
      result.extend(
          _LeafChange(old[index], new[new_start + index - old_start], new_start + index - old_start)
          for index in range(old_start, old_end))
      continue
    if tag == "replace":
      paired = min(old_end - old_start, new_end - new_start)
      result.extend(
          _LeafChange(old[old_start + index], new[new_start + index], new_start + index) for index in range(paired))
      result.extend(
          _LeafChange(old[old_start + index], None, new_start) for index in range(paired, old_end - old_start))
      result.extend(
          _LeafChange(None, new[new_start + index], new_start + index) for index in range(paired, new_end - new_start))
      continue
    if tag == "delete":
      result.extend(_LeafChange(old[index], None, new_start) for index in range(old_start, old_end))
      continue
    result.extend(_LeafChange(None, new[index], index) for index in range(new_start, new_end))
  return result


def _ancestor_details(node: _Node) -> Iterable[_Node]:
  current: _Node | None = node
  while current is not None:
    if current.tag == "details":
      yield current
    current = current.parent


def _section_label(leaf: _Leaf) -> str:
  current: _Node | None = leaf.element
  while current is not None:
    if current.tag == "section":
      h2 = _first_descendant(current, "h2")
      if h2 is not None:
        heading = _normalise("".join(part.text for part in _visible_parts(h2)))
        heading = re.sub(r"^\d+\s*", "", heading)
        return re.sub(r"changed\s*·\s*r\d+\s*$", "", heading).strip()
      return "<unnamed section>"
    current = current.parent
  classes = set((leaf.element.attrs.get("class") or "").split())
  if "mtag" in classes:
    return "header chip"
  return leaf.element.tag


def _first_descendant(node: _Node, tag: str) -> _Node | None:
  for child in node.children:
    if isinstance(child, _Node):
      if child.tag == tag:
        return child
      found = _first_descendant(child, tag)
      if found is not None:
        return found
  return None


def _add_insertion(insertions: dict[int, list[str]], offset: int, value: str) -> None:
  insertions.setdefault(offset, []).append(value)


def _add_class(source: str, node: _Node, class_name: str, insertions: dict[int, list[str]]) -> None:
  if node.start is None or node.start_end is None:
    raise ValueError("cannot annotate an element without a start tag")
  raw = source[node.start:node.start_end]
  class_match = _CLASS_RE.search(raw)
  if class_match is not None:
    classes = class_match.group("value").split()
    if class_name not in classes:
      _add_insertion(insertions, node.start + class_match.end("value"), " " + class_name)
    return
  unquoted = _UNQUOTED_CLASS_RE.search(raw)
  if unquoted is not None:
    if class_name not in unquoted.group("value").split():
      _add_insertion(insertions, node.start + unquoted.end("value"), f"&#32;{class_name}")
    return
  close = raw.rfind("/>")
  if close < 0:
    close = raw.rfind(">")
  _add_insertion(insertions, node.start + close, f' class="{class_name}"')


def _add_attr(source: str, node: _Node, name: str, value: str | None, insertions: dict[int, list[str]]) -> None:
  if node.start is None or node.start_end is None:
    raise ValueError("cannot annotate an element without a start tag")
  raw = source[node.start:node.start_end]
  if re.search(rf"(?<![\w:-]){re.escape(name)}\s*(?:=|\s|/?>)", raw, re.IGNORECASE):
    return
  close = raw.rfind("/>")
  if close < 0:
    close = raw.rfind(">")
  if value is None:
    addition = f" {name}"
  else:
    addition = f' {name}="{_html.escape(value, quote=True)}"'
  _add_insertion(insertions, node.start + close, addition)


def _raw_bounds(tokens: list[_Token], start: int, end: int, leaf: _Leaf) -> list[tuple[int, int]]:
  if start >= end:
    return []
  raw_ranges, part_indices = _leaf_raw_map(leaf)
  logical_start = tokens[start].logical_start
  logical_end = tokens[end - 1].logical_end
  result: list[tuple[int, int]] = []
  current_part = part_indices[logical_start]
  current_start = logical_start
  for logical_index in range(logical_start + 1, logical_end):
    part = part_indices[logical_index]
    if part != current_part:
      result.append((raw_ranges[current_start][0], raw_ranges[logical_index - 1][1]))
      current_part = part
      current_start = logical_index
  result.append((raw_ranges[current_start][0], raw_ranges[logical_end - 1][1]))
  return result


def _anchor_offset(tokens: list[_Token], start: int, leaf: _Leaf) -> int:
  if start < len(tokens):
    return tokens[start].raw_start
  if tokens:
    return tokens[-1].raw_end
  if leaf.parts:
    return leaf.parts[0].start
  if leaf.element.start_end is None:
    raise ValueError("cannot anchor a mark without a source position")
  return leaf.element.start_end


def _token_text(leaf: _Leaf, tokens: list[_Token], start: int, end: int, include_boundary_whitespace: bool) -> str:
  if start >= end:
    return ""
  logical_start = tokens[start].logical_start
  logical_end = tokens[end - 1].logical_end
  if include_boundary_whitespace:
    if end < len(tokens):
      logical_end = tokens[end].logical_start
    elif start > 0:
      logical_start = tokens[start - 1].logical_end
  return leaf.text[logical_start:logical_end]


def _wrapping_removes_direct_text(leaf: _Leaf, ranges: list[tuple[int, int]]) -> bool:
  # A block stays commentable only while a direct text node keeps a non-space
  # character (the panel has no heading/paragraph fallback).  Return True when
  # the block has direct text and the planned <ins> ranges cover all of it:
  # wrapping must then be replaced by a class mark on the block itself.  Blocks
  # with no direct text of their own (rows, badge-only headings) are excluded:
  # wrapping never takes a direct text node away from them.
  has_direct_text = False
  for child in leaf.element.children:
    if not isinstance(child, _TextPart):
      continue
    for index, char in enumerate(child.text):
      if char.isspace():
        continue
      has_direct_text = True
      raw_start, raw_end = child.raw_ranges[index]
      if not any(start <= raw_start and raw_end <= end for start, end in ranges):
        return False
  return has_direct_text


def _descendant_nodes(node: _Node) -> Iterable[_Node]:
  for child in node.children:
    if isinstance(child, _Node):
      yield child
      yield from _descendant_nodes(child)


def _is_ancestor(ancestor: _Node, node: _Node) -> bool:
  current: _Node | None = node
  while current is not None:
    if current is ancestor:
      return True
    current = current.parent
  return False


def _ghost_parent(root: _Node, leaves: list[_Leaf], index: int, old: _Leaf) -> _Node | None:
  wanted = old.element.parent
  if wanted is None:
    return None
  candidates = ([root] if root.tag == wanted.tag else []) + list(_descendant_nodes(root))
  for candidate_index in range(index, len(leaves)):
    candidate = leaves[candidate_index].element
    current = candidate.parent
    while current is not None:
      if current.tag == wanted.tag:
        return current
      current = current.parent
  for candidate_index in range(min(index - 1, len(leaves) - 1), -1, -1):
    candidate = leaves[candidate_index].element
    current = candidate.parent
    while current is not None:
      if current.tag == wanted.tag:
        return current
      current = current.parent
  target = leaves[index].element if index < len(leaves) else None
  if target is not None and target.start is not None:
    containing = [
        candidate for candidate in candidates if candidate.tag == wanted.tag and _is_ancestor(candidate, target)
    ]
    if containing:
      return max(containing, key=lambda candidate: candidate.start or 0)
    preceding = [
        candidate for candidate in candidates
        if candidate.tag == wanted.tag and candidate.end_end is not None and candidate.end_end <= target.start
    ]
    if preceding:
      return max(preceding, key=lambda candidate: candidate.end_end or 0)
  candidates = [candidate for candidate in candidates if candidate.tag == wanted.tag]
  if candidates:
    return max(candidates, key=lambda candidate: candidate.end_end or candidate.start or 0)
  return None


def _ghost_markup(old: _Leaf) -> str:
  tag = old.element.tag
  text = old.text if old.whole else "".join(part.text for part in _visible_parts(old.element))
  data = _html.escape(text, quote=True)
  if tag == "tr":
    cells = [child for child in old.element.children if isinstance(child, _Node) and child.tag in {"td", "th"}]
    colspan = max(1, len(cells))
    return f'<tr class="cbd-del" data-del="{data}"><td colspan="{colspan}" data-cbd-del="{data}"></td></tr>'
  return f'<{tag} class="cbd-del" data-del="{data}"></{tag}>'


def _insert_ghost(
    source: str,
    root: _Node,
    old: _Leaf,
    new_leaves: list[_Leaf],
    new_index: int,
    insertions: dict[int, list[str]],
    details: set[_Node],
) -> None:
  parent = _ghost_parent(root, new_leaves, new_index, old)
  if new_index < len(new_leaves):
    target = new_leaves[new_index].element
    if parent is not None and not _is_ancestor(parent, target) and parent.end is not None:
      offset = parent.end
      context = parent
    else:
      if target.start is None:
        raise ValueError("cannot place a ghost before an element without a start tag")
      offset = target.start
      context = target
  elif parent is not None and parent.end is not None:
    offset = parent.end
    context = parent
  elif new_leaves:
    previous = new_leaves[-1].element
    if previous.end_end is None:
      raise ValueError("cannot place a ghost after an element without an end tag")
    offset = previous.end_end
    context = previous
  else:
    offset = len(source)
    context = None
  _add_insertion(insertions, offset, _ghost_markup(old))
  if context is not None:
    details.update(_ancestor_details(context))


def _render_leaf_changes(
    source: str,
    root: _Node,
    changes: list[_LeafChange],
    new_leaves: list[_Leaf],
    insertions: dict[int, list[str]],
    classes: dict[_Node, set[str]],
    details: set[_Node],
) -> None:
  new_positions = {id(leaf): index for index, leaf in enumerate(new_leaves)}
  # A class-marked parent replaces its full visible subtree; do not add nested
  # marks that would duplicate its ghost when the annotation is restored.
  replaced_old: set[_Node] = set()
  replaced_new: set[_Node] = set()
  for change in changes:
    if change.old is not None and any(
        root is not change.old.element and _is_ancestor(root, change.old.element) for root in replaced_old):
      continue
    if change.new is not None and any(
        root is not change.new.element and _is_ancestor(root, change.new.element) for root in replaced_new):
      continue
    if change.new is None:
      if change.old is None:
        raise ValueError("invalid empty leaf change")
      # Deletions of direct text are represented at the nearest new leaf's
      # anchor.  Whole block leaves use a ghost so their tag remains in the
      # block structure (notably for rows and list items).
      position = change.new_index if change.new_index is not None else len(new_leaves)
      if change.old.whole:
        _insert_ghost(source, root, change.old, new_leaves, position, insertions, details)
      elif new_leaves:
        target = new_leaves[position].element if position < len(new_leaves) else new_leaves[-1].element
        offset = target.start if position < len(new_leaves) else target.end_end
        if offset is None:
          raise ValueError("cannot anchor a deleted text leaf")
        _add_insertion(
            insertions, offset, f'<span class="cbd-del" data-del="{_html.escape(change.old.text, quote=True)}"></span>')
        details.update(_ancestor_details(target))
      continue
    new_position = new_positions[id(change.new)]
    if change.old is None:
      classes.setdefault(change.new.element, set()).add("cbd-new")
      details.update(_ancestor_details(change.new.element))
      continue
    old_tokens = _leaf_tokens(change.old)
    new_tokens = _leaf_tokens(change.new)
    opcodes = _merged_opcodes(old_tokens, new_tokens)
    changed_opcodes = [opcode for opcode in opcodes if opcode[0] != "equal"]
    if not changed_opcodes:
      continue
    wrap_ranges: list[tuple[int, int]] = []
    for _, _, _, new_start, new_end in changed_opcodes:
      wrap_ranges.extend(_raw_bounds(new_tokens, new_start, new_end, change.new))
    if _wrapping_removes_direct_text(change.new, wrap_ranges):
      classes.setdefault(change.new.element, set()).add("cbd-new")
      replaced_old.add(change.old.element)
      replaced_new.add(change.new.element)
      _insert_ghost(source, root, change.old, new_leaves, new_position, insertions, details)
      continue
    details.update(_ancestor_details(change.new.element))
    for _, old_start, old_end, new_start, new_end in changed_opcodes:
      old_text = _token_text(change.old, old_tokens, old_start, old_end, new_start == new_end)
      deletion = f'<span class="cbd-del" data-del="{_html.escape(old_text, quote=True)}"></span>' if old_text else ""
      if deletion:
        _add_insertion(insertions, _anchor_offset(new_tokens, new_start, change.new), deletion)
      for start, end in _raw_bounds(new_tokens, new_start, new_end, change.new):
        _add_insertion(insertions, start, '<ins class="cbd-ins">')
        _add_insertion(insertions, end, "</ins>")


def _render_start_tag_additions(
    source: str, classes: dict[_Node, set[str]], details: set[_Node], insertions: dict[int, list[str]]) -> None:
  for node, names in classes.items():
    for name in names:
      _add_class(source, node, name, insertions)
  for node in details:
    _add_attr(source, node, "open", None, insertions)


def _splice(source: str, insertions: dict[int, list[str]]) -> str:
  result: list[str] = []
  for offset in range(len(source) + 1):
    if offset in insertions:
      result.extend(insertions[offset])
    if offset < len(source):
      result.append(source[offset])
  return "".join(result)


def _parse(source: str) -> _Parser:
  parser = _Parser(source)
  parser.feed(source)
  parser.close()
  return parser


def _append_style_and_header(source: str) -> str:
  parser = _parse(source)
  insertions: dict[int, list[str]] = {}
  style_tag = f'<style data-cbd-style>{_CBD_STYLE}</style>'
  head = _first_descendant(parser.root, "head")
  body = _first_descendant(parser.root, "body")
  if head is not None and head.end is not None:
    _add_insertion(insertions, head.end, style_tag)
  else:
    offset = body.start if body is not None and body.start is not None else 0
    _add_insertion(insertions, offset, style_tag)
  if body is not None and body.start_end is not None:
    offset = body.start_end
  elif parser.root.end is not None:
    offset = parser.root.end
  else:
    offset = len(source)
  header = f'<div class="cbd-header" data-cbd-header="{_html.escape(_HEADER_TEXT, quote=True)}"></div>'
  _add_insertion(insertions, offset, header)
  return _splice(source, insertions)


def _analyse(base_html: str, new_html: str) -> tuple[_Parser, _Parser, list[_LeafChange], list[_Leaf], list[_Leaf]]:
  base_parser = _parse(base_html)
  new_parser = _parse(new_html)
  base_leaves = _collect_leaves(_document_root(base_parser))
  new_leaves = _collect_leaves(_document_root(new_parser))
  return base_parser, new_parser, _align(base_leaves, new_leaves), base_leaves, new_leaves


def annotate(base_html: str, new_html: str) -> str:
  """Return ``new_html`` with the word-level comparison against ``base_html`` embedded."""
  _, new_parser, changes, _, new_leaves = _analyse(base_html, new_html)
  insertions: dict[int, list[str]] = {}
  classes: dict[_Node, set[str]] = {}
  details: set[_Node] = set()
  _render_leaf_changes(new_html, _document_root(new_parser), changes, new_leaves, insertions, classes, details)
  _render_start_tag_additions(new_html, classes, details, insertions)
  return _append_style_and_header(_splice(new_html, insertions))


def diff_text(base_html: str, new_html: str) -> str:
  """Return a plain-text, one-section-per-changed-leaf comparison."""
  _, _, changes, _, _ = _analyse(base_html, new_html)
  sections: list[str] = []
  for change in changes:
    old_text = _normalise(change.old.text) if change.old is not None else ""
    new_text = _normalise(change.new.text) if change.new is not None else ""
    if change.old is not None and change.new is not None:
      if not any(
          opcode[0] != "equal" for opcode in _merged_opcodes(_leaf_tokens(change.old), _leaf_tokens(change.new))):
        continue
    elif old_text == new_text:
      continue
    leaf = change.new or change.old
    if leaf is None:
      raise ValueError("invalid empty leaf change")
    sections.append(f"[{_section_label(leaf)}]\n- {old_text}\n+ {new_text}")
  return "\n\n".join(sections)

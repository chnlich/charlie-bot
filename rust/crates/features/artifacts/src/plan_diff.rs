//! Word-level annotations that splice marks into the new HTML without reserializing its source.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

use charliebot_core::str_methods;
use charliebot_html::{EventKind, Opcode, OpcodeTag, StreamEvent, escape, opcodes, parse_with, unescape};
use regex::Regex;

const IGNORED_TAGS: &[&str] = &["head", "style", "script", "template", "noscript", "title"];
/// HTML void elements that the tokenizer tree builder never pushes onto its open-element stack.
pub const VOID_TAGS: &[&str] =
  &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];
const BLOCK_TAGS: &[&str] = &[
  "address",
  "article",
  "aside",
  "blockquote",
  "body",
  "caption",
  "dd",
  "details",
  "dialog",
  "div",
  "dl",
  "dt",
  "fieldset",
  "figcaption",
  "figure",
  "footer",
  "form",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "header",
  "hgroup",
  "hr",
  "li",
  "main",
  "nav",
  "ol",
  "p",
  "pre",
  "section",
  "summary",
  "table",
  "tbody",
  "td",
  "tfoot",
  "th",
  "thead",
  "tr",
  "ul",
];
const CBD_STYLE: &str = ".cbd-header { margin: 0 0 14px; padding: 8px 12px; border: 1px solid #d9dfe6; border-radius: 7px; background: #fff8df; color: #7a5a00; font-weight: 600; }\n\
.cbd-header::before { content: attr(data-cbd-header); }\n\
ins.cbd-ins { background: rgba(46, 160, 67, .18); text-decoration: none; }\n\
.cbd-del::after { content: attr(data-del); color: #cf222e; text-decoration: line-through; }\n\
.cbd-new { background: rgba(46, 160, 67, .10); }\n\
tr.cbd-del > td::after { content: attr(data-cbd-del); color: #cf222e; text-decoration: line-through; }\n";
const HEADER_TEXT: &str = "This version · vs previous";
const CJK_RANGES: &[(u32, u32)] = &[(0x3400, 0x4dbf), (0x4e00, 0x9fff), (0xf900, 0xfaff), (0x20000, 0x2fa1f)];
static DISPLAY_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"(?:^|;)\s*display\s*:\s*([^;]+)").expect("the display rule is a valid regex"));
/// `_section_label`'s leading section number, and the revision badge a changed heading carries.
static LEADING_DIGITS_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"^\d+\s*").expect("the section-number pattern is a valid regex"));
static TRAILING_CHANGED_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"changed\s*·\s*r\d+\s*$").expect("the revision-badge pattern is a valid regex"));

#[derive(Debug)]
struct Node {
  tag: String,
  attrs: HashMap<String, Option<String>>,
  parent: Option<usize>,
  start: Option<usize>,
  start_end: Option<usize>,
  end: Option<usize>,
  end_end: Option<usize>,
  children: Vec<Child>,
}

#[derive(Debug)]
enum Child {
  Node(usize),
  Text(usize),
}

#[derive(Debug)]
struct TextPart {
  start: usize,
  text: String,
  raw_ranges: Vec<(usize, usize)>,
}

struct Parser {
  source: String,
  nodes: Vec<Node>,
  parts: Vec<TextPart>,
  stack: Vec<usize>,
}

#[derive(Clone, Debug)]
struct Leaf {
  element: usize,
  parts: Vec<usize>,
  whole: bool,
}

#[derive(Clone, Debug)]
struct Token {
  value: String,
  logical_start: usize,
  logical_end: usize,
  raw_start: usize,
  raw_end: usize,
}

#[derive(Clone, Copy, Debug)]
struct LeafChange {
  old: Option<usize>,
  new: Option<usize>,
  new_index: Option<usize>,
}

#[derive(Clone, Copy)]
struct DiffContext<'a> {
  parser: &'a Parser,
  old_parser: &'a Parser,
  root: usize,
  old_leaves: &'a [Leaf],
  new_leaves: &'a [Leaf],
}

type Analysis = (Parser, Parser, Vec<Leaf>, Vec<Leaf>, Vec<LeafChange>);

impl Parser {
  fn parse(source: &str) -> Result<Parser, String> {
    let mut parser = Parser {
      source: source.to_owned(),
      nodes: vec![Node {
        tag: "#root".to_owned(),
        attrs: HashMap::new(),
        parent: None,
        start: None,
        start_end: None,
        end: None,
        end_end: None,
        children: Vec::new(),
      }],
      parts: Vec::new(),
      stack: vec![0],
    };
    parse_with(source, false, |event| parser.handle(event))
      .map_err(|error| format!("{}: {}", error.exception, error.message))?;
    Ok(parser)
  }

  fn handle(&mut self, event: StreamEvent) {
    match event.kind {
      EventKind::StartTag | EventKind::StartEndTag => self.open_node(&event),
      EventKind::EndTag => self.close_node(&event),
      EventKind::Data => self.append_data(event),
      EventKind::EntityReference => self.append_reference(&event, false),
      EventKind::CharacterReference => self.append_reference(&event, true),
      EventKind::Comment
      | EventKind::Declaration
      | EventKind::ProcessingInstruction
      | EventKind::UnknownDeclaration => {}
    }
  }

  fn open_node(&mut self, event: &StreamEvent) {
    let args = event.args.as_array().expect("start-tag events have an argument list");
    let tag = args[0].as_str().expect("start-tag events have a tag").to_owned();
    let attrs = args[1]
      .as_array()
      .expect("start-tag events have an attribute list")
      .iter()
      .map(|attribute| {
        let pair = attribute.as_array().expect("an attribute has a name and value");
        let name = pair[0].as_str().expect("an attribute name is text").to_owned();
        let value = pair[1].as_str().map(str::to_owned);
        (name, value)
      })
      .collect();
    let raw = event.starttag_text.as_ref().expect("start-tag events retain their source text");
    let parent = *self.stack.last().expect("the root node remains on the parser stack");
    let node_index = self.nodes.len();
    self.nodes.push(Node {
      tag: tag.clone(),
      attrs,
      parent: Some(parent),
      start: Some(event.byte_offset),
      start_end: Some(event.byte_offset + raw.len()),
      end: None,
      end_end: None,
      children: Vec::new(),
    });
    self.nodes[parent].children.push(Child::Node(node_index));
    if event.kind == EventKind::StartTag && !is_void_tag(&tag) {
      self.stack.push(node_index);
    }
  }

  fn close_node(&mut self, event: &StreamEvent) {
    let tag = event.args[0].as_str().expect("end-tag events have a tag");
    let start = event.byte_offset;
    let end_end = start + self.source[start..].find('>').expect("an end-tag event includes >") + 1;
    if let Some(index) = (1..self.stack.len()).rev().find(|&index| self.nodes[self.stack[index]].tag == tag) {
      let node = self.stack[index];
      self.nodes[node].end = Some(start);
      self.nodes[node].end_end = Some(end_end);
      self.stack.truncate(index);
    }
  }

  fn append_data(&mut self, mut event: StreamEvent) {
    let text = match std::mem::take(&mut event.args[0]) {
      serde_json::Value::String(text) => text,
      other => unreachable!("data events have text, got {other:?}"),
    };
    let start = event.byte_offset;
    let end = start + text.len();
    assert_eq!(&self.source[start..end], text, "HTMLParser data offsets address the source text");
    let raw_ranges =
      text.char_indices().map(|(offset, character)| (start + offset, start + offset + character.len_utf8())).collect();
    self.append_part(start, text, raw_ranges);
  }

  fn append_reference(&mut self, event: &StreamEvent, character_reference: bool) {
    let name = event.args[0].as_str().expect("reference events have a reference name");
    let start = event.byte_offset;
    let base_punctuation = if character_reference { 2 } else { 1 };
    let mut end = start + name.len() + base_punctuation;
    let raw = &self.source[start..];
    let terminated = if character_reference {
      raw.starts_with(&format!("&#x{name};")) || raw.starts_with(&format!("&#{name};"))
    } else {
      raw.starts_with(&format!("&{name};"))
    };
    if terminated {
      end += 1;
    }
    let text = unescape(&self.source[start..end]);
    let raw_ranges = vec![(start, end); text.chars().count()];
    self.append_part(start, text, raw_ranges);
  }

  fn append_part(&mut self, start: usize, text: String, raw_ranges: Vec<(usize, usize)>) {
    let parent = *self.stack.last().expect("the root node remains on the parser stack");
    let index = self.parts.len();
    self.parts.push(TextPart { start, text, raw_ranges });
    self.nodes[parent].children.push(Child::Text(index));
  }
}

fn is_void_tag(tag: &str) -> bool {
  VOID_TAGS.contains(&tag)
}

fn is_python_word(character: char) -> bool {
  character.is_alphanumeric() || character == '_'
}

fn is_cjk(character: char) -> bool {
  CJK_RANGES.iter().any(|(start, end)| (*start..=*end).contains(&(character as u32)))
}

fn normalise(text: &str) -> String {
  let mut output = String::new();
  let mut pending_space = false;
  for character in text.chars() {
    if str_methods::is_python_whitespace(character) {
      pending_space = !output.is_empty();
    } else {
      if pending_space {
        output.push(' ');
      }
      output.push(character);
      pending_space = false;
    }
  }
  output
}

fn node_is_boundary(node: &Node) -> bool {
  if BLOCK_TAGS.contains(&node.tag.as_str()) {
    return true;
  }
  let classes = node.attrs.get("class").and_then(Option::as_deref).unwrap_or("");
  if node.tag == "span" && str_methods::split_whitespace(classes).iter().any(|class| *class == "mtag" || *class == "b") {
    return true;
  }
  let style = node.attrs.get("style").and_then(Option::as_deref).unwrap_or("").to_lowercase();
  let Some(display) = DISPLAY_RE.captures(&style).and_then(|capture| capture.get(1)) else {
    return false;
  };
  matches!(
    str_methods::strip(display.as_str()),
    "block" | "flow-root" | "flex" | "grid" | "list-item" | "table" | "table-caption" | "table-cell" | "table-row"
  )
}

fn visible_parts(parser: &Parser, node_index: usize) -> Vec<usize> {
  let node = &parser.nodes[node_index];
  if IGNORED_TAGS.contains(&node.tag.as_str()) {
    return Vec::new();
  }
  let mut result = Vec::new();
  for child in &node.children {
    match child {
      Child::Text(part) => result.push(*part),
      Child::Node(child_node) => result.extend(visible_parts(parser, *child_node)),
    }
  }
  result
}

fn has_visible_text(parser: &Parser, parts: &[usize]) -> bool {
  parts.iter().any(|&index| parser.parts[index].text.chars().any(|character| !str_methods::is_python_whitespace(character)))
}

fn collect_leaves(parser: &Parser, root: usize) -> Vec<Leaf> {
  fn visit(parser: &Parser, node_index: usize, leaves: &mut Vec<Leaf>) {
    let all_parts = visible_parts(parser, node_index);
    if !has_visible_text(parser, &all_parts) {
      return;
    }
    let node = &parser.nodes[node_index];
    if node.tag == "tr" {
      leaves.push(Leaf { element: node_index, parts: all_parts, whole: true });
      return;
    }
    let direct_parts: Vec<usize> = node
      .children
      .iter()
      .filter_map(|child| match child {
        Child::Text(part) => Some(*part),
        Child::Node(_) => None,
      })
      .collect();
    let has_block_children = node.children.iter().any(|child| match child {
      Child::Node(child_node) => node_is_boundary(&parser.nodes[*child_node]),
      Child::Text(_) => false,
    });
    if has_block_children {
      if has_visible_text(parser, &direct_parts) {
        leaves.push(Leaf { element: node_index, parts: direct_parts, whole: false });
      }
      for child in &node.children {
        if let Child::Node(child_node) = child {
          visit(parser, *child_node, leaves);
        }
      }
      return;
    }
    leaves.push(Leaf { element: node_index, parts: all_parts, whole: true });
  }

  let mut leaves = Vec::new();
  visit(parser, root, &mut leaves);
  leaves.retain(|leaf| has_visible_text(parser, &leaf.parts));
  leaves
}

fn document_root(parser: &Parser) -> usize {
  first_descendant(parser, 0, "body").unwrap_or(0)
}

fn first_descendant(parser: &Parser, node_index: usize, tag: &str) -> Option<usize> {
  for child in &parser.nodes[node_index].children {
    if let Child::Node(child_node) = child {
      if parser.nodes[*child_node].tag == tag {
        return Some(*child_node);
      }
      if let Some(found) = first_descendant(parser, *child_node, tag) {
        return Some(found);
      }
    }
  }
  None
}

fn first_class_descendant(parser: &Parser, node_index: usize, class_name: &str) -> Option<usize> {
  for child in &parser.nodes[node_index].children {
    if let Child::Node(child_node) = child {
      let classes = parser.nodes[*child_node].attrs.get("class").and_then(Option::as_deref).unwrap_or("");
      if str_methods::split_whitespace(classes).contains(&class_name) {
        return Some(*child_node);
      }
      if let Some(found) = first_class_descendant(parser, *child_node, class_name) {
        return Some(found);
      }
    }
  }
  None
}

fn leaf_text(parser: &Parser, leaf: &Leaf) -> String {
  leaf.parts.iter().map(|&part| parser.parts[part].text.as_str()).collect()
}

fn tokenise(text: &str) -> Vec<(String, usize, usize)> {
  let characters: Vec<char> = text.chars().collect();
  let mut tokens = Vec::new();
  let mut index = 0;
  while index < characters.len() {
    let character = characters[index];
    if str_methods::is_python_whitespace(character) {
      index += 1;
      while index < characters.len() && str_methods::is_python_whitespace(characters[index]) {
        index += 1;
      }
      continue;
    }
    if is_cjk(character) {
      tokens.push((character.to_string(), index, index + 1));
      index += 1;
      continue;
    }
    if character.is_ascii() && (character.is_ascii_alphanumeric() || character == '_') {
      let start = index;
      index += 1;
      while index < characters.len()
        && characters[index].is_ascii()
        && (characters[index].is_ascii_alphanumeric() || characters[index] == '_')
      {
        index += 1;
      }
      tokens.push((characters[start..index].iter().collect(), start, index));
      continue;
    }
    tokens.push((character.to_string(), index, index + 1));
    index += 1;
  }
  tokens
}

fn leaf_tokens(parser: &Parser, leaf: &Leaf) -> Vec<Token> {
  let mut result = Vec::new();
  let mut offset = 0;
  for &part_index in &leaf.parts {
    let part = &parser.parts[part_index];
    for (value, start, end) in tokenise(&part.text) {
      result.push(Token {
        value,
        logical_start: offset + start,
        logical_end: offset + end,
        raw_start: part.raw_ranges[start].0,
        raw_end: part.raw_ranges[end - 1].1,
      });
    }
    offset += part.text.chars().count();
  }
  result
}

fn leaf_raw_map(parser: &Parser, leaf: &Leaf) -> (Vec<(usize, usize)>, Vec<usize>) {
  let mut raw_ranges = Vec::new();
  let mut part_indices = Vec::new();
  for (part_index, &part) in leaf.parts.iter().enumerate() {
    raw_ranges.extend(parser.parts[part].raw_ranges.iter().copied());
    part_indices.extend(std::iter::repeat_n(part_index, parser.parts[part].raw_ranges.len()));
  }
  (raw_ranges, part_indices)
}

fn merged_opcodes(old: &[Token], new: &[Token]) -> Vec<Opcode> {
  let old_values: Vec<&str> = old.iter().map(|token| token.value.as_str()).collect();
  let new_values: Vec<&str> = new.iter().map(|token| token.value.as_str()).collect();
  let opcodes = opcodes(&old_values, &new_values);
  let mut merged = Vec::new();
  let mut index = 0;
  while index < opcodes.len() {
    let opcode = opcodes[index];
    if opcode.tag == OpcodeTag::Equal {
      merged.push(opcode);
      index += 1;
      continue;
    }
    let (mut old_end, mut new_end) = (opcode.a_end, opcode.b_end);
    while index + 1 < opcodes.len() && opcodes[index + 1].tag != OpcodeTag::Equal {
      index += 1;
      old_end = opcodes[index].a_end;
      new_end = opcodes[index].b_end;
    }
    while index + 2 < opcodes.len() {
      let equal = opcodes[index + 1];
      let following = opcodes[index + 2];
      if equal.tag != OpcodeTag::Equal || equal.a_end - equal.a_start >= 3 {
        break;
      }
      old_end = following.a_end;
      new_end = following.b_end;
      index += 2;
    }
    merged.push(Opcode {
      tag: OpcodeTag::Replace,
      a_start: opcode.a_start,
      a_end: old_end,
      b_start: opcode.b_start,
      b_end: new_end,
    });
    index += 1;
  }
  merged
}

fn align(old_parser: &Parser, old: &[Leaf], new_parser: &Parser, new: &[Leaf]) -> Vec<LeafChange> {
  let old_keys: Vec<String> = old.iter().map(|leaf| normalise(&leaf_text(old_parser, leaf))).collect();
  let new_keys: Vec<String> = new.iter().map(|leaf| normalise(&leaf_text(new_parser, leaf))).collect();
  let mut changes = Vec::new();
  for opcode in opcodes(&old_keys, &new_keys) {
    match opcode.tag {
      OpcodeTag::Equal => {
        for old_index in opcode.a_start..opcode.a_end {
          let new_index = opcode.b_start + old_index - opcode.a_start;
          changes.push(LeafChange { old: Some(old_index), new: Some(new_index), new_index: Some(new_index) });
        }
      }
      OpcodeTag::Replace => {
        let paired = (opcode.a_end - opcode.a_start).min(opcode.b_end - opcode.b_start);
        for index in 0..paired {
          let old_index = opcode.a_start + index;
          let new_index = opcode.b_start + index;
          changes.push(LeafChange { old: Some(old_index), new: Some(new_index), new_index: Some(new_index) });
        }
        for index in paired..opcode.a_end - opcode.a_start {
          changes.push(LeafChange {
            old: Some(opcode.a_start + index),
            new: None,
            new_index: Some(opcode.b_start + paired),
          });
        }
        for index in paired..opcode.b_end - opcode.b_start {
          let new_index = opcode.b_start + index;
          changes.push(LeafChange { old: None, new: Some(new_index), new_index: Some(new_index) });
        }
      }
      OpcodeTag::Delete => {
        for old_index in opcode.a_start..opcode.a_end {
          changes.push(LeafChange { old: Some(old_index), new: None, new_index: Some(opcode.b_start) });
        }
      }
      OpcodeTag::Insert => {
        for new_index in opcode.b_start..opcode.b_end {
          changes.push(LeafChange { old: None, new: Some(new_index), new_index: Some(new_index) });
        }
      }
    }
  }
  changes
}

fn ancestor_details(parser: &Parser, node: usize) -> Vec<usize> {
  let mut details = Vec::new();
  let mut current = Some(node);
  while let Some(node) = current {
    if parser.nodes[node].tag == "details" {
      details.push(node);
    }
    current = parser.nodes[node].parent;
  }
  details
}

fn add_insertion(insertions: &mut HashMap<usize, Vec<String>>, offset: usize, value: String) {
  insertions.entry(offset).or_default().push(value);
}

fn start_tag_raw<'a>(source: &'a str, node: &Node) -> Result<(usize, &'a str), String> {
  let start = node.start.ok_or_else(|| "cannot annotate an element without a start tag".to_owned())?;
  let end = node.start_end.ok_or_else(|| "cannot annotate an element without a start tag".to_owned())?;
  Ok((start, &source[start..end]))
}

fn attr_insert_offset(raw: &str) -> usize {
  raw.rfind("/>").or_else(|| raw.rfind('>')).expect("a parsed start tag ends with >")
}

fn preceding_boundary(raw: &str, index: usize) -> bool {
  raw[..index]
    .chars()
    .next_back()
    .is_none_or(|character| !is_python_word(character) && character != ':' && character != '-')
}

fn find_class_attr(raw: &str, quoted: bool) -> Option<(usize, usize, String)> {
  for (index, _) in raw.char_indices() {
    if !preceding_boundary(raw, index)
      || !raw[index..].get(..5).is_some_and(|value| value.eq_ignore_ascii_case("class"))
    {
      continue;
    }
    let mut cursor = index + 5;
    while let Some(character) = raw[cursor..].chars().next().filter(|character| str_methods::is_python_whitespace(*character))
    {
      cursor += character.len_utf8();
    }
    if !raw[cursor..].starts_with('=') {
      continue;
    }
    cursor += 1;
    while let Some(character) = raw[cursor..].chars().next().filter(|character| str_methods::is_python_whitespace(*character))
    {
      cursor += character.len_utf8();
    }
    if quoted {
      let Some(quote @ ('\'' | '"')) = raw[cursor..].chars().next() else { continue };
      let value_start = cursor + quote.len_utf8();
      let Some((offset, closing_quote)) = raw[value_start..].char_indices().find(|(_, ch)| *ch == '\'' || *ch == '"')
      else {
        continue;
      };
      if closing_quote == quote {
        let value_end = value_start + offset;
        return Some((value_start, value_end, raw[value_start..value_end].to_owned()));
      }
      continue;
    }
    let value_start = cursor;
    let mut value_end = cursor;
    for (offset, character) in raw[cursor..].char_indices() {
      if str_methods::is_python_whitespace(character) || character == '>' {
        break;
      }
      value_end = cursor + offset + character.len_utf8();
    }
    if value_end > value_start {
      return Some((value_start, value_end, raw[value_start..value_end].to_owned()));
    }
  }
  None
}

fn add_class(
  source: &str,
  node: &Node,
  class_name: &str,
  insertions: &mut HashMap<usize, Vec<String>>,
) -> Result<(), String> {
  let (start, raw) = start_tag_raw(source, node)?;
  if let Some((_, value_end, value)) = find_class_attr(raw, true) {
    if !str_methods::split_whitespace(&value).contains(&class_name) {
      add_insertion(insertions, start + value_end, format!(" {class_name}"));
    }
    return Ok(());
  }
  if let Some((_, value_end, value)) = find_class_attr(raw, false) {
    if !str_methods::split_whitespace(&value).contains(&class_name) {
      add_insertion(insertions, start + value_end, format!("&#32;{class_name}"));
    }
    return Ok(());
  }
  add_insertion(insertions, start + attr_insert_offset(raw), format!(" class=\"{class_name}\""));
  Ok(())
}

fn has_attribute(raw: &str, name: &str) -> bool {
  for (index, _) in raw.char_indices() {
    if !preceding_boundary(raw, index)
      || !raw[index..].get(..name.len()).is_some_and(|value| value.eq_ignore_ascii_case(name))
    {
      continue;
    }
    let after_name = index + name.len();
    if let Some(character) = raw[after_name..].chars().next()
      && (str_methods::is_python_whitespace(character)
        || matches!(character, '=' | '>')
        || raw[after_name..].starts_with("/>"))
    {
      return true;
    }
  }
  false
}

fn add_attr(
  source: &str,
  node: &Node,
  name: &str,
  value: Option<&str>,
  insertions: &mut HashMap<usize, Vec<String>>,
) -> Result<(), String> {
  let (start, raw) = start_tag_raw(source, node)?;
  if has_attribute(raw, name) {
    return Ok(());
  }
  let addition = match value {
    Some(value) => format!(" {name}=\"{}\"", escape(value, true)),
    None => format!(" {name}"),
  };
  add_insertion(insertions, start + attr_insert_offset(raw), addition);
  Ok(())
}

fn raw_bounds(parser: &Parser, tokens: &[Token], start: usize, end: usize, leaf: &Leaf) -> Vec<(usize, usize)> {
  if start >= end {
    return Vec::new();
  }
  let (raw_ranges, part_indices) = leaf_raw_map(parser, leaf);
  let logical_start = tokens[start].logical_start;
  let logical_end = tokens[end - 1].logical_end;
  let mut result = Vec::new();
  let mut current_part = part_indices[logical_start];
  let mut current_start = logical_start;
  for logical_index in logical_start + 1..logical_end {
    let part = part_indices[logical_index];
    if part != current_part {
      result.push((raw_ranges[current_start].0, raw_ranges[logical_index - 1].1));
      current_part = part;
      current_start = logical_index;
    }
  }
  result.push((raw_ranges[current_start].0, raw_ranges[logical_end - 1].1));
  result
}

fn anchor_offset(tokens: &[Token], start: usize, parser: &Parser, leaf: &Leaf) -> Result<usize, String> {
  if start < tokens.len() {
    return Ok(tokens[start].raw_start);
  }
  if let Some(token) = tokens.last() {
    return Ok(token.raw_end);
  }
  if let Some(&part) = leaf.parts.first() {
    return Ok(parser.parts[part].start);
  }
  parser.nodes[leaf.element].start_end.ok_or_else(|| "cannot anchor a mark without a source position".to_owned())
}

fn token_text(
  parser: &Parser,
  leaf: &Leaf,
  tokens: &[Token],
  start: usize,
  end: usize,
  include_boundary_whitespace: bool,
) -> String {
  if start >= end {
    return String::new();
  }
  let mut logical_start = tokens[start].logical_start;
  let mut logical_end = tokens[end - 1].logical_end;
  if include_boundary_whitespace {
    if end < tokens.len() {
      logical_end = tokens[end].logical_start;
    } else if start > 0 {
      logical_start = tokens[start - 1].logical_end;
    }
  }
  leaf_text(parser, leaf).chars().skip(logical_start).take(logical_end - logical_start).collect()
}

fn wrapping_removes_direct_text(parser: &Parser, leaf: &Leaf, ranges: &[(usize, usize)]) -> bool {
  let mut has_direct_text = false;
  for child in &parser.nodes[leaf.element].children {
    let Child::Text(part_index) = child else { continue };
    let part = &parser.parts[*part_index];
    for (index, character) in part.text.chars().enumerate() {
      if str_methods::is_python_whitespace(character) {
        continue;
      }
      has_direct_text = true;
      let (raw_start, raw_end) = part.raw_ranges[index];
      if !ranges.iter().any(|(start, end)| *start <= raw_start && raw_end <= *end) {
        return false;
      }
    }
  }
  has_direct_text
}

fn descendant_nodes(parser: &Parser, node: usize, output: &mut Vec<usize>) {
  for child in &parser.nodes[node].children {
    if let Child::Node(child_node) = child {
      output.push(*child_node);
      descendant_nodes(parser, *child_node, output);
    }
  }
}

fn is_ancestor(parser: &Parser, ancestor: usize, node: usize) -> bool {
  let mut current = Some(node);
  while let Some(node) = current {
    if node == ancestor {
      return true;
    }
    current = parser.nodes[node].parent;
  }
  false
}

fn ghost_parent(context: &DiffContext<'_>, index: usize, old_leaf: &Leaf) -> Option<usize> {
  let parser = context.parser;
  let old_parser = context.old_parser;
  let root = context.root;
  let leaves = context.new_leaves;
  let wanted = old_parser.nodes[old_leaf.element].parent?;
  let wanted_tag = old_parser.nodes[wanted].tag.as_str();
  let mut scan_indices: Vec<usize> = (index..leaves.len()).collect();
  if index > 0 {
    scan_indices.extend((0..index.min(leaves.len())).rev());
  }
  for scan_index in scan_indices {
    let mut current = parser.nodes[leaves[scan_index].element].parent;
    while let Some(node) = current {
      if parser.nodes[node].tag == wanted_tag {
        return Some(node);
      }
      current = parser.nodes[node].parent;
    }
  }

  let target = leaves.get(index).map(|leaf| leaf.element);
  if let Some(target) = target
    && let Some(target_start) = parser.nodes[target].start
  {
    let mut descendants = Vec::new();
    if parser.nodes[root].tag == wanted_tag {
      descendants.push(root);
    }
    descendant_nodes(parser, root, &mut descendants);
    if let Some(found) = descendants
      .iter()
      .copied()
      .filter(|&candidate| parser.nodes[candidate].tag == wanted_tag && is_ancestor(parser, candidate, target))
      .max_by_key(|&candidate| parser.nodes[candidate].start.unwrap_or(0))
    {
      return Some(found);
    }
    if let Some(found) = descendants
      .iter()
      .copied()
      .filter(|&candidate| {
        parser.nodes[candidate].tag == wanted_tag
          && parser.nodes[candidate].end_end.is_some_and(|end| end <= target_start)
      })
      .max_by_key(|&candidate| parser.nodes[candidate].end_end.unwrap_or(0))
    {
      return Some(found);
    }
  }

  let mut descendants = Vec::new();
  if parser.nodes[root].tag == wanted_tag {
    descendants.push(root);
  }
  descendant_nodes(parser, root, &mut descendants);
  descendants.into_iter().filter(|&candidate| parser.nodes[candidate].tag == wanted_tag).max_by_key(|&candidate| {
    parser.nodes[candidate]
      .end_end
      .filter(|&end| end != 0)
      .or_else(|| parser.nodes[candidate].start.filter(|&start| start != 0))
      .unwrap_or(0)
  })
}

fn ghost_markup(parser: &Parser, leaf: &Leaf, base_source: &str) -> String {
  let node = &parser.nodes[leaf.element];
  let text = if leaf.whole {
    leaf_text(parser, leaf)
  } else {
    leaf_text(parser, &Leaf { element: leaf.element, parts: visible_parts(parser, leaf.element), whole: true })
  };
  let trailing = node
    .end_end
    .map(|end| {
      base_source[end..].chars().take_while(|character| str_methods::is_python_whitespace(*character)).collect::<String>()
    })
    .unwrap_or_default();
  let data = escape(&format!("{text}{trailing}"), true);
  if node.tag == "tr" {
    let cells = node
      .children
      .iter()
      .filter_map(|child| match child {
        Child::Node(index) if matches!(parser.nodes[*index].tag.as_str(), "td" | "th") => Some(*index),
        _ => None,
      })
      .count()
      .max(1);
    format!("<tr class=\"cbd-del\" data-del=\"{data}\"><td colspan=\"{cells}\" data-cbd-del=\"{data}\"></td></tr>")
  } else {
    format!("<{} class=\"cbd-del\" data-del=\"{data}\"></{}>", node.tag, node.tag)
  }
}

fn insert_ghost(
  context: &DiffContext<'_>,
  old_leaf: &Leaf,
  new_index: usize,
  insertions: &mut HashMap<usize, Vec<String>>,
  details: &mut BTreeSet<usize>,
) -> Result<(), String> {
  let parser = context.parser;
  let old_parser = context.old_parser;
  let new_leaves = context.new_leaves;
  let parent = ghost_parent(context, new_index, old_leaf);
  let (offset, target_context) = if new_index < new_leaves.len() {
    let target = new_leaves[new_index].element;
    if let Some(parent) = parent.filter(|parent| !is_ancestor(parser, *parent, target))
      && let Some(end) = parser.nodes[parent].end
    {
      (end, Some(parent))
    } else {
      let start = parser.nodes[target]
        .start
        .ok_or_else(|| "cannot place a ghost before an element without a start tag".to_owned())?;
      (start, Some(target))
    }
  } else if let Some(parent) = parent.filter(|parent| parser.nodes[*parent].end.is_some()) {
    (parser.nodes[parent].end.expect("the selected parent has an end tag"), Some(parent))
  } else if let Some(previous) = new_leaves.last() {
    let end = parser.nodes[previous.element]
      .end_end
      .ok_or_else(|| "cannot place a ghost after an element without an end tag".to_owned())?;
    (end, Some(previous.element))
  } else {
    (parser.source.len(), None)
  };
  add_insertion(insertions, offset, ghost_markup(old_parser, old_leaf, &old_parser.source));
  if let Some(target_context) = target_context {
    details.extend(ancestor_details(parser, target_context));
  }
  Ok(())
}

fn render_leaf_changes(
  context: &DiffContext<'_>,
  changes: &[LeafChange],
  insertions: &mut HashMap<usize, Vec<String>>,
  classes: &mut BTreeMap<usize, BTreeSet<String>>,
  details: &mut BTreeSet<usize>,
) -> Result<(), String> {
  let parser = context.parser;
  let old_parser = context.old_parser;
  let old_leaves = context.old_leaves;
  let new_leaves = context.new_leaves;
  let new_positions: HashMap<usize, usize> =
    new_leaves.iter().enumerate().map(|(index, leaf)| (leaf.element, index)).collect();
  let mut replaced_old = BTreeSet::new();
  let mut replaced_new = BTreeSet::new();
  for change in changes {
    if let Some(old_index) = change.old {
      let element = old_leaves[old_index].element;
      if replaced_old.iter().any(|&ancestor| ancestor != element && is_ancestor(old_parser, ancestor, element)) {
        continue;
      }
    }
    if let Some(new_index) = change.new {
      let element = new_leaves[new_index].element;
      if replaced_new.iter().any(|&ancestor| ancestor != element && is_ancestor(parser, ancestor, element)) {
        continue;
      }
    }
    if change.new.is_none() {
      let old_index = change.old.ok_or_else(|| "invalid empty leaf change".to_owned())?;
      let old_leaf = &old_leaves[old_index];
      let position = change.new_index.unwrap_or(new_leaves.len());
      if old_leaf.whole {
        insert_ghost(context, old_leaf, position, insertions, details)?;
      } else if !new_leaves.is_empty() {
        let target_index = if position < new_leaves.len() { position } else { new_leaves.len() - 1 };
        let target = new_leaves[target_index].element;
        let offset =
          (if position < new_leaves.len() { parser.nodes[target].start } else { parser.nodes[target].end_end })
            .ok_or_else(|| "cannot anchor a deleted text leaf".to_owned())?;
        let data = escape(&leaf_text(old_parser, old_leaf), true);
        add_insertion(insertions, offset, format!("<span class=\"cbd-del\" data-del=\"{data}\"></span>"));
        details.extend(ancestor_details(parser, target));
      }
      continue;
    }

    let new_index = change.new.expect("a non-deletion has a new leaf");
    let new_leaf = &new_leaves[new_index];
    if change.old.is_none() {
      classes.entry(new_leaf.element).or_default().insert("cbd-new".to_owned());
      details.extend(ancestor_details(parser, new_leaf.element));
      continue;
    }

    let old_leaf = &old_leaves[change.old.expect("a paired leaf has an old leaf")];
    let old_tokens = leaf_tokens(old_parser, old_leaf);
    let new_tokens = leaf_tokens(parser, new_leaf);
    let opcodes = merged_opcodes(&old_tokens, &new_tokens);
    let changed: Vec<Opcode> = opcodes.into_iter().filter(|opcode| opcode.tag != OpcodeTag::Equal).collect();
    if changed.is_empty() {
      continue;
    }
    let mut wrap_ranges = Vec::new();
    for opcode in &changed {
      wrap_ranges.extend(raw_bounds(parser, &new_tokens, opcode.b_start, opcode.b_end, new_leaf));
    }
    if wrapping_removes_direct_text(parser, new_leaf, &wrap_ranges) {
      classes.entry(new_leaf.element).or_default().insert("cbd-new".to_owned());
      replaced_old.insert(old_leaf.element);
      replaced_new.insert(new_leaf.element);
      insert_ghost(
        context,
        old_leaf,
        *new_positions.get(&new_leaf.element).expect("every new leaf has a position"),
        insertions,
        details,
      )?;
      continue;
    }

    details.extend(ancestor_details(parser, new_leaf.element));
    for opcode in changed {
      let old_text =
        token_text(old_parser, old_leaf, &old_tokens, opcode.a_start, opcode.a_end, opcode.b_start == opcode.b_end);
      if !old_text.is_empty() {
        let deletion = format!("<span class=\"cbd-del\" data-del=\"{}\"></span>", escape(&old_text, true));
        add_insertion(insertions, anchor_offset(&new_tokens, opcode.b_start, parser, new_leaf)?, deletion);
      }
      for (start, end) in raw_bounds(parser, &new_tokens, opcode.b_start, opcode.b_end, new_leaf) {
        add_insertion(insertions, start, "<ins class=\"cbd-ins\">".to_owned());
        add_insertion(insertions, end, "</ins>".to_owned());
      }
    }
  }
  Ok(())
}

fn render_start_tag_additions(
  source: &str,
  parser: &Parser,
  classes: &BTreeMap<usize, BTreeSet<String>>,
  details: &BTreeSet<usize>,
  insertions: &mut HashMap<usize, Vec<String>>,
) -> Result<(), String> {
  for (&node, names) in classes {
    for name in names {
      add_class(source, &parser.nodes[node], name, insertions)?;
    }
  }
  for &node in details {
    add_attr(source, &parser.nodes[node], "open", None, insertions)?;
  }
  Ok(())
}

fn splice(source: &str, insertions: &HashMap<usize, Vec<String>>) -> String {
  let mut offsets: Vec<usize> = insertions.keys().copied().collect();
  offsets.sort_unstable();
  let mut output = String::with_capacity(source.len());
  let mut cursor = 0;
  for offset in offsets {
    output.push_str(&source[cursor..offset]);
    for insertion in &insertions[&offset] {
      output.push_str(insertion);
    }
    cursor = offset;
  }
  output.push_str(&source[cursor..]);
  output
}

fn append_style_and_header(source: &str) -> Result<String, String> {
  let parser = Parser::parse(source)?;
  let mut insertions = HashMap::new();
  let style_tag = format!("<style data-cbd-style>{CBD_STYLE}</style>");
  let head = first_descendant(&parser, 0, "head");
  let body = first_descendant(&parser, 0, "body");
  if let Some(head) = head.filter(|head| parser.nodes[*head].end.is_some()) {
    add_insertion(&mut insertions, parser.nodes[head].end.expect("the selected head has an end tag"), style_tag);
  } else {
    let offset = body.and_then(|body| parser.nodes[body].start).unwrap_or(0);
    add_insertion(&mut insertions, offset, style_tag);
  }
  let offset = if let Some(body) = body.filter(|body| parser.nodes[*body].start_end.is_some()) {
    let root = document_root(&parser);
    let target = first_class_descendant(&parser, root, "wrap").or_else(|| first_descendant(&parser, root, "main"));
    target
      .and_then(|target| parser.nodes[target].start_end)
      .unwrap_or(parser.nodes[body].start_end.expect("the selected body has a start tag"))
  } else if let Some(end) = parser.nodes[0].end {
    end
  } else {
    source.len()
  };
  let header = format!("<div class=\"cbd-header\" data-cbd-header=\"{}\"></div>", escape(HEADER_TEXT, true));
  add_insertion(&mut insertions, offset, header);
  Ok(splice(source, &insertions))
}

fn analyse(base_html: &str, new_html: &str) -> Result<Analysis, String> {
  let base_parser = Parser::parse(base_html)?;
  let new_parser = Parser::parse(new_html)?;
  let base_leaves = collect_leaves(&base_parser, document_root(&base_parser));
  let new_leaves = collect_leaves(&new_parser, document_root(&new_parser));
  let changes = align(&base_parser, &base_leaves, &new_parser, &new_leaves);
  Ok((base_parser, new_parser, base_leaves, new_leaves, changes))
}

/// Return `new_html` with word-level marks against `base_html` embedded in its source.
pub fn annotate(base_html: &str, new_html: &str) -> Result<String, String> {
  let (base_parser, new_parser, base_leaves, new_leaves, changes) = analyse(base_html, new_html)?;
  let root = document_root(&new_parser);
  let context = DiffContext {
    parser: &new_parser,
    old_parser: &base_parser,
    root,
    old_leaves: &base_leaves,
    new_leaves: &new_leaves,
  };
  let mut insertions = HashMap::new();
  let mut classes = BTreeMap::new();
  let mut details = BTreeSet::new();
  render_leaf_changes(&context, &changes, &mut insertions, &mut classes, &mut details)?;
  render_start_tag_additions(new_html, &new_parser, &classes, &details, &mut insertions)?;
  append_style_and_header(&splice(new_html, &insertions))
}

/// Return a plain-text, one-section-per-changed-leaf comparison of `new_html` against `base_html`.
pub fn diff_text(base_html: &str, new_html: &str) -> Result<String, String> {
  let (base_parser, new_parser, old_leaves, new_leaves, changes) = analyse(base_html, new_html)?;
  let mut sections = Vec::new();
  for change in changes {
    let old_text = change.old.map(|old| normalise(&leaf_text(&base_parser, &old_leaves[old]))).unwrap_or_default();
    let new_text = change.new.map(|new| normalise(&leaf_text(&new_parser, &new_leaves[new]))).unwrap_or_default();
    match (change.old, change.new) {
      (Some(old), Some(new)) => {
        let old_tokens = leaf_tokens(&base_parser, &old_leaves[old]);
        let new_tokens = leaf_tokens(&new_parser, &new_leaves[new]);
        if !merged_opcodes(&old_tokens, &new_tokens).iter().any(|opcode| opcode.tag != OpcodeTag::Equal) {
          continue;
        }
      }
      _ if old_text == new_text => continue,
      _ => {}
    }
    let (parser, leaves, index) = match (change.new, change.old) {
      (Some(new), _) => (&new_parser, &new_leaves, new),
      (None, Some(old)) => (&base_parser, &old_leaves, old),
      (None, None) => return Err("invalid empty leaf change".to_string()),
    };
    let label = section_label(parser, &leaves[index]);
    sections.push(format!("[{label}]\n- {old_text}\n+ {new_text}"));
  }
  Ok(sections.join("\n\n"))
}

/// `_section_label`: the nearest `section`'s heading without its number or revision badge; a leaf outside any section
/// takes the header chip marker for a `span.mtag`, else its tag.
fn section_label(parser: &Parser, leaf: &Leaf) -> String {
  let mut current = Some(leaf.element);
  while let Some(node) = current {
    if parser.nodes[node].tag == "section" {
      let Some(h2) = first_descendant(parser, node, "h2") else { return "<unnamed section>".to_string() };
      let heading: String =
        visible_parts(parser, h2).into_iter().map(|part| parser.parts[part].text.as_str()).collect();
      let heading = LEADING_DIGITS_RE.replace(&normalise(&heading), "").into_owned();
      return str_methods::strip(&TRAILING_CHANGED_RE.replace(&heading, "")).to_string();
    }
    current = parser.nodes[node].parent;
  }
  let element = &parser.nodes[leaf.element];
  let classes = element.attrs.get("class").and_then(Option::as_deref).unwrap_or("");
  if str_methods::split_whitespace(classes).contains(&"mtag") { "header chip".to_string() } else { element.tag.clone() }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Read;

  const BASE: &str = include_str!("../../../../../tests/data/plan_sample_v10.html");
  const NEW: &str = include_str!("../../../../../tests/data/plan_sample_v11.html");

  #[test]
  fn seeded_plan_annotations_match_the_recorded_python_outputs() {
    let mut decoder = flate2::read::GzDecoder::new(include_bytes!("../tests/data/plan_diff_golden.json.gz").as_slice());
    let mut json = String::new();
    decoder.read_to_string(&mut json).expect("the recorded fixture decompresses");
    let fixture: serde_json::Value = serde_json::from_str(&json).expect("the recorded fixture is valid JSON");
    assert_eq!(fixture["source"], "rust/crates/features/artifacts/tests/differential.py");
    assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
    assert_eq!(fixture["seed"], 20261018);
    let pairs = fixture["pairs"].as_array().expect("the fixture holds the seeded pairs");
    assert_eq!(pairs.len(), 200);

    let expected = pairs
      .iter()
      .map(|pair| pair["expected"].as_str().expect("each pair has recorded output").to_string())
      .collect::<Vec<_>>();
    let actual = pairs
      .iter()
      .map(|pair| {
        annotate(
          pair["base"].as_str().expect("each pair has a base page"),
          pair["new"].as_str().expect("each pair has a new page"),
        )
        .expect("the seeded pages parse")
      })
      .collect::<Vec<_>>();
    assert_eq!(actual, expected, "recorded Python outputs differ for at least one pair");

    let mut control = actual.clone();
    control[0].push_str("planted differential control");
    assert_eq!(
      control.iter().zip(&expected).filter(|(actual, expected)| actual != expected).count(),
      1,
      "the planted control difference must produce one mismatch"
    );
  }

  #[test]
  fn void_tags_match_the_python_differ_set() {
    assert_eq!(
      VOID_TAGS,
      &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr",]
    );
  }

  #[test]
  fn sample_plan_versions_keep_word_marks_in_the_new_page() {
    let annotated = annotate(BASE, NEW).expect("the plan pages parse");
    assert!(annotated.contains("data-del=\"v10\""));
    assert!(annotated.contains("<ins class=\"cbd-ins\">v11</ins>"));
    assert!(annotated.contains("data-del=\"旧\""));
    assert!(annotated.contains("<ins class=\"cbd-ins\">新</ins>"));
  }

  #[test]
  fn an_entirely_new_block_uses_a_class_mark() {
    let base = "<html><body><p>unchanged</p></body></html>";
    let new = "<html><body><p>unchanged</p><p>new passage to comment</p></body></html>";
    let annotated = annotate(base, new).expect("the pages parse");
    assert!(annotated.contains("<p class=\"cbd-new\">new passage to comment</p>"));
    assert!(!annotated.contains("<ins class=\"cbd-ins\">new passage to comment</ins>"));
  }

  #[test]
  fn cjk_changes_keep_one_character_per_token() {
    let base = "<html><body><p>中文 旧 文本</p><p>kept</p></body></html>";
    let new = "<html><body><p>中文 新 文本</p><p>kept</p></body></html>";
    let annotated = annotate(base, new).expect("the pages parse");
    assert!(annotated.contains("data-del=\"旧\""));
    assert!(annotated.contains("<ins class=\"cbd-ins\">新</ins>"));
  }

  #[test]
  fn replacing_heading_text_preserves_the_direct_text_node() {
    let badge_base = "<html><body><h2><span class=\"n\">2</span> Context<span class=\"revbadge\">changed · r4</span></h2></body></html>";
    let badge_new = "<html><body><h2><span class=\"n\">2</span> Context</h2></body></html>";
    let badge = annotate(badge_base, badge_new).expect("the pages parse");
    assert!(badge.contains("<h2><span class=\"n\">2</span> Context"));
    assert!(badge.contains("data-del=\"changed · r4\""));
    assert!(!badge.contains("<ins class=\"cbd-ins\">Context</ins>"));

    let base = "<html><body><h2><span class=\"n\">2</span> Alpha Beta</h2></body></html>";
    let new = "<html><body><h2><span class=\"n\">2</span> Gamma Delta</h2></body></html>";
    let annotated = annotate(base, new).expect("the pages parse");
    assert!(annotated.contains("<h2 class=\"cbd-new\"><span class=\"n\">2</span> Gamma Delta</h2>"));
    assert!(annotated.contains("data-del=\"2 Alpha Beta\""));
  }
}

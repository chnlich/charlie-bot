//! The Rust scan: the production code of a Rust file, read through its syntax tree.
//!
//! Items under `#[cfg(test)]` and `#[test]` functions are test code and are left out. Attributes are left out too: they
//! hold documentation and configuration, not code, and a doc comment is an attribute. Comments are no site, since the
//! syntax tree holds none and a comment chooses nothing that runs. Every string literal is read as text (`text_scan`).
//! The positions that select a backend are read from the expression that holds them: a program's arguments, a
//! constant's value, the operand of a default method, the value of a `backend` field, and a `--backend` pair in a list.
//! A macro whose body is a list of expressions is visited as those expressions. Any other macro body is read as its
//! tokens.

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{
  Attribute, Expr, ExprArray, ExprCall, ExprClosure, ExprLit, ExprMethodCall, FieldValue, ImplItem, ImplItemConst,
  Item, ItemConst, ItemStatic, Lit, LitStr, Local, Macro, Member, Meta, Stmt, StmtMacro, Token, TraitItem,
};

use crate::rules::{self, CALL_MARKERS, Class, FIXED_COMMANDS, Found, fixed_command};
use crate::text_scan;
use crate::vocabulary::Vocabulary;

/// Scan the production code of one Rust file. A file that does not parse is an error: its sites cannot be read.
pub fn scan(vocab: &Vocabulary, source: &str) -> syn::Result<Vec<Found>> {
  let file = syn::parse_file(source)?;
  let mut scanner = Scanner { vocab, found: Vec::new() };
  syn::visit::Visit::visit_file(&mut scanner, &file);
  Ok(scanner.found)
}

struct Scanner<'a> {
  vocab: &'a Vocabulary,
  found: Vec<Found>,
}

impl Scanner<'_> {
  fn push(&mut self, line: usize, class: Class, value: impl Into<String>) {
    self.found.push(Found { line, class, value: value.into() });
  }

  /// A default operand: the backend word that `unwrap_or` and its kin fall back to.
  fn default_hit(&mut self, operand: &Expr) {
    if let Some((line, word)) = default_candidate(operand)
      && rules::names_backend(self.vocab, &word)
    {
      self.push(line, Class::DefaultValue, word);
    }
  }

  /// A `Command::new(program).arg(marker)` chain: the program is a backend CLI and a marker makes a call.
  fn command_arg_hit(&mut self, call: &ExprMethodCall) {
    let mut markers = Vec::new();
    let mut current = call;
    let root = loop {
      if current.method == "arg"
        && let Some(marker) = current.args.first().and_then(literal_text)
      {
        markers.push(marker);
      }
      match &*current.receiver {
        Expr::MethodCall(next) => current = next,
        Expr::Call(root) => break root,
        _ => return,
      }
    };
    let Some(command) = program_name(root) else { return };
    for (line, marker) in markers {
      if CALL_MARKERS.contains(&marker.as_str()) {
        self.push(line, Class::FixedCommand, format!("{command} {marker}"));
      }
    }
  }

  /// A list of words: an argv whose first word is a backend CLI and whose later word is a call marker, and a
  /// `--backend` word followed by a pool id.
  fn list_hits(&mut self, elements: &[&Expr]) {
    if let Some((_, first)) = elements.first().and_then(|first| literal_text(first))
      && let Some(command) = fixed_command(&first)
    {
      let marker = elements[1..]
        .iter()
        .filter_map(|element| literal_text(element))
        .find(|(_, text)| CALL_MARKERS.contains(&text.as_str()));
      if let Some((line, marker)) = marker {
        self.push(line, Class::FixedCommand, format!("{command} {marker}"));
      }
    }
    for pair in elements.windows(2) {
      if let Some((_, flag)) = literal_text(pair[0])
        && flag == "--backend"
        && let Some((line, id)) = literal_text(pair[1])
        && self.vocab.pools.contains(&id)
      {
        self.push(line, Class::PoolId, id);
      }
    }
  }

  /// A constant whose value is a backend CLI name.
  fn constant_hit(&mut self, expr: &Expr) {
    if let Expr::Lit(ExprLit { lit: Lit::Str(text), .. }) = expr {
      let value = text.value();
      if FIXED_COMMANDS.contains(&value.as_str()) {
        self.push(line_of(text.span()), Class::FixedCommand, value);
      }
    }
  }

  /// The tokens of a macro that is not a list of expressions: each string literal is text, and a `--backend` flag, a
  /// `backend` key, and a program followed by its marker are read across the tokens.
  fn token_hits(&mut self, tokens: TokenStream) {
    let mut pieces = Vec::new();
    flatten(tokens, &mut pieces);
    for (index, piece) in pieces.iter().enumerate() {
      if let Piece::Str(line, text) = piece {
        let found = text_scan::scan_block(self.vocab, text, *line);
        self.found.extend(found);
        let next = pieces[index + 1..].iter().find(|next| !matches!(next, Piece::Punct(',')));
        if let Some(command) = fixed_command(text)
          && let Some(Piece::Str(_, marker)) = next
          && CALL_MARKERS.contains(&marker.as_str())
        {
          self.push(*line, Class::FixedCommand, format!("{command} {marker}"));
        }
      }
      // A key or a flag is a word, whether a macro writes it as an identifier or as a string literal.
      let Some(word) = piece.word() else { continue };
      if word == "--backend" {
        let value = pieces[index + 1..].iter().find(|next| !matches!(next, Piece::Punct(',' | '=' | ':')));
        if let Some(Piece::Str(value_line, id)) = value
          && self.vocab.pools.contains(id)
        {
          self.push(*value_line, Class::PoolId, id.clone());
        }
      }
      if word == "backend"
        && let Some(Piece::Punct(':' | '=')) = pieces.get(index + 1)
        && let Some(Piece::Str(value_line, id)) = pieces.get(index + 2)
        && self.vocab.pools.contains(id)
      {
        self.push(*value_line, Class::PoolId, id.clone());
      }
    }
  }
}

impl<'ast> Visit<'ast> for Scanner<'_> {
  fn visit_attribute(&mut self, _: &'ast Attribute) {}

  fn visit_item(&mut self, item: &'ast Item) {
    if !is_test_gated(item_attrs(item)) {
      visit::visit_item(self, item);
    }
  }

  fn visit_impl_item(&mut self, item: &'ast ImplItem) {
    if !is_test_gated(impl_item_attrs(item)) {
      visit::visit_impl_item(self, item);
    }
  }

  fn visit_trait_item(&mut self, item: &'ast TraitItem) {
    if !is_test_gated(trait_item_attrs(item)) {
      visit::visit_trait_item(self, item);
    }
  }

  fn visit_local(&mut self, local: &'ast Local) {
    if !is_test_gated(&local.attrs) {
      visit::visit_local(self, local);
    }
  }

  fn visit_stmt_macro(&mut self, stmt: &'ast StmtMacro) {
    if !is_test_gated(&stmt.attrs) {
      visit::visit_stmt_macro(self, stmt);
    }
  }

  fn visit_lit_str(&mut self, lit: &'ast LitStr) {
    let found = text_scan::scan_block(self.vocab, &lit.value(), line_of(lit.span()));
    self.found.extend(found);
  }

  fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
    match call.method.to_string().as_str() {
      "unwrap_or" | "or" => {
        if let Some(operand) = call.args.first() {
          self.default_hit(operand);
        }
      }
      "unwrap_or_else" | "or_else" => {
        if let Some(Expr::Closure(ExprClosure { body, .. })) = call.args.first() {
          self.default_hit(body);
        }
      }
      "arg" => self.command_arg_hit(call),
      _ => {}
    }
    visit::visit_expr_method_call(self, call);
  }

  fn visit_expr_array(&mut self, array: &'ast ExprArray) {
    let elements: Vec<&Expr> = array.elems.iter().collect();
    self.list_hits(&elements);
    visit::visit_expr_array(self, array);
  }

  fn visit_macro(&mut self, mac: &'ast Macro) {
    match Punctuated::<Expr, Token![,]>::parse_terminated.parse2(mac.tokens.clone()) {
      Ok(list) => {
        let elements: Vec<&Expr> = list.iter().collect();
        if mac.path.is_ident("vec") {
          self.list_hits(&elements);
        }
        for element in &list {
          self.visit_expr(element);
        }
      }
      // The body is not a list of expressions, as in `json!({...})`: its tokens carry the sites.
      Err(_) => self.token_hits(mac.tokens.clone()),
    }
  }

  fn visit_item_const(&mut self, item: &'ast ItemConst) {
    self.constant_hit(&item.expr);
    visit::visit_item_const(self, item);
  }

  fn visit_item_static(&mut self, item: &'ast ItemStatic) {
    self.constant_hit(&item.expr);
    visit::visit_item_static(self, item);
  }

  fn visit_impl_item_const(&mut self, item: &'ast ImplItemConst) {
    self.constant_hit(&item.expr);
    visit::visit_impl_item_const(self, item);
  }

  fn visit_field_value(&mut self, field: &'ast FieldValue) {
    if matches!(&field.member, Member::Named(name) if name == "backend")
      && let Some((line, id)) = literal_text(&field.expr)
      && self.vocab.pools.contains(&id)
    {
      self.push(line, Class::PoolId, id);
    }
    visit::visit_field_value(self, field);
  }
}

/// A token of a macro body, flattened. Only a string literal carries its line, since it is the one kind that holds text
/// a scan reads for a site.
enum Piece {
  Str(usize, String),
  Word(String),
  Punct(char),
}

impl Piece {
  /// The text of a string literal or a word, for the positions that name a key or a flag either way.
  fn word(&self) -> Option<&str> {
    match self {
      Piece::Str(_, text) | Piece::Word(text) => Some(text),
      Piece::Punct(_) => None,
    }
  }
}

fn flatten(stream: TokenStream, pieces: &mut Vec<Piece>) {
  for tree in stream {
    match tree {
      TokenTree::Group(group) => flatten(group.stream(), pieces),
      TokenTree::Ident(ident) => pieces.push(Piece::Word(ident.to_string())),
      TokenTree::Punct(punct) => pieces.push(Piece::Punct(punct.as_char())),
      TokenTree::Literal(literal) => {
        // A literal that is not a string, such as a number, holds no text a scan reads.
        if let Ok(text) = syn::parse_str::<LitStr>(&literal.to_string()) {
          pieces.push(Piece::Str(line_of(literal.span()), text.value()));
        }
      }
    }
  }
}

fn line_of(span: Span) -> usize {
  span.start().line
}

/// The text of a string literal, through the forms that build a `String` from one: `.to_string()`, `.to_owned()`,
/// `.into()` and `String::from`. It returns the literal's line too.
fn literal_text(expr: &Expr) -> Option<(usize, String)> {
  match expr {
    Expr::Lit(ExprLit { lit: Lit::Str(text), .. }) => Some((line_of(text.span()), text.value())),
    Expr::MethodCall(call)
      if call.args.is_empty() && matches!(call.method.to_string().as_str(), "to_string" | "to_owned" | "into") =>
    {
      literal_text(&call.receiver)
    }
    Expr::Call(call) if is_path(&call.func, &["String", "from"]) => call.args.first().and_then(literal_text),
    Expr::Paren(paren) => literal_text(&paren.expr),
    Expr::Group(group) => literal_text(&group.expr),
    _ => None,
  }
}

/// The text of a default operand: `Some("x")`, a block's last expression, or a literal.
fn default_candidate(expr: &Expr) -> Option<(usize, String)> {
  match expr {
    Expr::Call(call) if is_path(&call.func, &["Some"]) => call.args.first().and_then(literal_text),
    Expr::Block(block) => match block.block.stmts.last() {
      Some(Stmt::Expr(tail, None)) => default_candidate(tail),
      _ => None,
    },
    _ => literal_text(expr),
  }
}

/// The program a `Command::new("...")` call names, when that name is a backend CLI.
fn program_name(call: &ExprCall) -> Option<&'static str> {
  if !is_path(&call.func, &["Command", "new"]) {
    return None;
  }
  let (_, program) = call.args.first().and_then(literal_text)?;
  fixed_command(&program)
}

/// Whether a path expression ends in `segments`: `Command::new` matches `tokio::process::Command::new`.
fn is_path(func: &Expr, segments: &[&str]) -> bool {
  let Expr::Path(path) = func else { return false };
  let names: Vec<String> = path.path.segments.iter().map(|segment| segment.ident.to_string()).collect();
  names.len() >= segments.len() && names.iter().rev().zip(segments.iter().rev()).all(|(name, want)| name == want)
}

/// Whether attributes mark an item as test code: `#[test]`, or `#[cfg(test)]` exactly. A `cfg(not(test))` is production.
pub fn is_test_gated(attrs: &[Attribute]) -> bool {
  attrs.iter().any(|attr| match &attr.meta {
    Meta::Path(path) => path.is_ident("test"),
    Meta::List(list) => list.path.is_ident("cfg") && list.tokens.to_string() == "test",
    Meta::NameValue(_) => false,
  })
}

fn item_attrs(item: &Item) -> &[Attribute] {
  match item {
    Item::Const(item) => &item.attrs,
    Item::Enum(item) => &item.attrs,
    Item::ExternCrate(item) => &item.attrs,
    Item::Fn(item) => &item.attrs,
    Item::ForeignMod(item) => &item.attrs,
    Item::Impl(item) => &item.attrs,
    Item::Macro(item) => &item.attrs,
    Item::Mod(item) => &item.attrs,
    Item::Static(item) => &item.attrs,
    Item::Struct(item) => &item.attrs,
    Item::Trait(item) => &item.attrs,
    Item::TraitAlias(item) => &item.attrs,
    Item::Type(item) => &item.attrs,
    Item::Union(item) => &item.attrs,
    Item::Use(item) => &item.attrs,
    // A verbatim item keeps no attributes apart from its tokens, and syn adds variants over time.
    _ => &[],
  }
}

fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
  match item {
    ImplItem::Const(item) => &item.attrs,
    ImplItem::Fn(item) => &item.attrs,
    ImplItem::Macro(item) => &item.attrs,
    ImplItem::Type(item) => &item.attrs,
    _ => &[],
  }
}

fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
  match item {
    TraitItem::Const(item) => &item.attrs,
    TraitItem::Fn(item) => &item.attrs,
    TraitItem::Macro(item) => &item.attrs,
    TraitItem::Type(item) => &item.attrs,
    _ => &[],
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeSet;

  use super::*;

  fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| item.to_string()).collect()
  }

  fn vocab() -> Vocabulary {
    Vocabulary {
      options: set(&["claude-opus"]),
      pools: set(&["cheap"]),
      models: set(&["claude-opus-5-5"]),
      hosts: set(&["aigw.example.team"]),
      id_prefixes: set(&["claude-"]),
    }
  }

  fn found(source: &str) -> Vec<(usize, Class, String)> {
    let mut sites: Vec<(usize, Class, String)> =
      scan(&vocab(), source).expect("the fixture parses").into_iter().map(|f| (f.line, f.class, f.value)).collect();
    sites.sort();
    sites
  }

  fn has(source: &str, class: Class, value: &str) -> bool {
    found(source).iter().any(|(_, c, v)| *c == class && v == value)
  }

  /// An option id, a model id and a gateway host in a string literal of production code are sites.
  #[test]
  fn value_classes_are_read_from_production_literals() {
    assert!(has("fn f() { let _ = \"claude-opus\"; }", Class::OptionId, "claude-opus"));
    assert!(has("fn f() { let _ = \"claude-opus-5-5\"; }", Class::ModelId, "claude-opus-5-5"));
    assert!(has(
      "fn f() { let _ = \"https://aigw.example.team/v1\"; }",
      Class::Gateway,
      "https://aigw.example.team/v1"
    ));
  }

  /// Test code is left out: a `#[cfg(test)]` module, a `#[test]` function and an attribute carry no site.
  #[test]
  fn test_code_and_attributes_are_not_sites() {
    let source = "#[doc = \"claude-opus\"] fn f() {}\n#[cfg(test)] mod tests { fn g() { let _ = \"claude-opus\"; } }\n\
                  #[test] fn h() { let _ = \"claude-opus\"; }";
    assert!(found(source).is_empty(), "{:?}", found(source));
  }

  /// A backend CLI that the program runs with a call marker is a fixed command. The same program with `--version` is
  /// not.
  #[test]
  fn a_command_argument_chain_is_a_fixed_command_only_with_a_call_marker() {
    let call = "fn f() { std::process::Command::new(\"claude\").arg(\"-p\"); }";
    assert!(has(call, Class::FixedCommand, "claude -p"));
    let version = "fn f() { tokio::process::Command::new(\"claude\").arg(\"--version\"); }";
    assert!(found(version).is_empty(), "{:?}", found(version));
  }

  /// An argv array whose first word is a backend CLI and whose later word is a marker is a fixed command.
  #[test]
  fn an_argv_array_is_a_fixed_command() {
    let source = "fn f() -> Vec<String> { vec![\"claude\".to_string(), \"-p\".to_string(), \"x\".to_string()] }";
    assert!(has(source, Class::FixedCommand, "claude -p"));
  }

  /// A constant whose value is a backend CLI name is a fixed command. An array constant of the names is not.
  #[test]
  fn a_constant_value_is_a_fixed_command_and_a_list_constant_is_not() {
    assert!(has("const BINARY: &str = \"charlie-code\";", Class::FixedCommand, "charlie-code"));
    assert!(found("const NAMES: [&str; 2] = [\"claude\", \"codex\"];").is_empty());
  }

  /// A default operand that names a backend is a default-value site, the form pages.rs:645 has.
  #[test]
  fn a_default_operand_naming_a_backend_is_a_default_value() {
    let source = "fn f(x: Option<String>) -> String { x.unwrap_or_else(|| \"claude\".to_string()) }";
    assert!(has(source, Class::DefaultValue, "claude"));
    let plain = "fn f(x: Option<String>) -> String { x.unwrap_or(\"hello\".to_string()) }";
    assert!(found(plain).is_empty(), "{:?}", found(plain));
  }

  /// A pool id names a backend after a `--backend` flag, in a list or a `backend` field, and nowhere else.
  #[test]
  fn a_pool_id_is_a_site_only_in_a_selecting_position() {
    assert!(has("fn f() { g(&[\"--backend\", \"cheap\"]); }", Class::PoolId, "cheap"));
    assert!(has("struct S { backend: &'static str } fn f() -> S { S { backend: \"cheap\" } }", Class::PoolId, "cheap"));
    assert!(!has("fn f() { let _ = \"cheap\"; }", Class::PoolId, "cheap"));
  }

  /// A macro body that is not a list of expressions is read as its tokens: a `backend` key and its pool id are a site.
  #[test]
  fn a_macro_body_that_is_not_a_list_is_read_as_tokens() {
    assert!(has("fn f() { json!({ \"backend\": \"cheap\" }); }", Class::PoolId, "cheap"));
    assert!(has("fn f() { json!({ \"argv\": [\"claude\", \"-p\"] }); }", Class::FixedCommand, "claude -p"));
  }

  /// A comment chooses nothing that runs, so a line, doc or inner doc comment and a block comment name no site, whatever
  /// they hold. A model id in a string literal of the production code beside them is one.
  #[test]
  fn comments_are_no_site_and_a_literal_beside_them_is() {
    let source = "//! option claude-opus\n// call claude -p\n/// model claude-opus-5-5\n\
                  /* gateway https://aigw.example.team/v1 */\nfn f() { let _ = \"claude-opus-5-5\"; }\n";
    assert_eq!(found(source), vec![(5, Class::ModelId, "claude-opus-5-5".to_string())]);
  }

  /// A block comment is no site across its lines. A comment marker inside a string or a raw string literal opens no
  /// comment, so the literal is read as text.
  #[test]
  fn block_comments_are_no_site_and_literals_are_read() {
    let source = "fn f() {\n  /* default\n   claude-opus-5-5 */\n  let _ = \"// claude-opus\";\n\
                  \x20 let _ = r#\"/* claude-opus */\"#;\n}\n";
    assert_eq!(
      found(source),
      vec![(4, Class::OptionId, "claude-opus".to_string()), (5, Class::OptionId, "claude-opus".to_string())]
    );
  }
}

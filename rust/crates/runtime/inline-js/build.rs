//! Builds the table the library embeds: one row per loader-served `.js` name, with the sha256 of the source the loader
//! reads for it and, for each compressed name, that source's minified text.
//!
//! A source resolves the way the loader resolves it: the first tree that holds the name wins, in the order web/templates,
//! the package templates, web/static and the package static trees. Two packages that hold one name in the same tree
//! fail the build, because the loader's registration order would decide which of them it serves.
//!
//! The minifier removes whitespace and comments and shortens the locals of each function. It keeps every name a script
//! declares at its top level, because the page's other scripts read those names as globals. The output must parse as a
//! classic script again and bind the same top-level names, or the build fails and names the file.

#[path = "src/names.rs"]
mod names;

use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions, CommentOptions};
use oxc_minifier::{MangleOptions, Minifier, MinifierOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use sha2::{Digest, Sha256};

fn main() {
  let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
  let web = manifest.join("../../../../web");
  let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
  for tree in ["templates", "static", "features", "backends"] {
    println!("cargo:rerun-if-changed={}", web.join(tree).display());
  }
  println!("cargo:rerun-if-changed=src/names.rs");
  println!("cargo:rerun-if-changed=build.rs");

  let packages = package_directories(&web);
  let mut names: Vec<&str> = names::COMPRESSED.iter().chain(names::PASS_THROUGH).copied().collect();
  names.sort_unstable();
  if let Some(pair) = names.windows(2).find(|pair| pair[0] == pair[1]) {
    panic!("{} is listed twice in src/names.rs", pair[0]);
  }

  let mut table = String::from("pub static ROWS: &[Row] = &[\n");
  for name in names {
    let path = resolve(&web, &packages, name);
    let bytes = fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let source = std::str::from_utf8(&bytes).unwrap_or_else(|error| panic!("{}: not UTF-8: {error}", path.display()));
    let sha256 = hex(&Sha256::digest(&bytes));
    let minified =
      if names::COMPRESSED.contains(&name) { format!("Some({:?})", minify(&path, source)) } else { "None".to_string() };
    writeln!(table, "  Row {{ name: {name:?}, sha256: {sha256:?}, minified: {minified} }},")
      .expect("writing to a String cannot fail");
  }
  table.push_str("];\n");
  fs::write(out_dir.join("inline_js_table.rs"), table).expect("OUT_DIR is writable");
}

/// The file the loader serves for `name`: the first tree that holds it, and one file per package tree.
fn resolve(web: &Path, packages: &[PathBuf], name: &str) -> PathBuf {
  let tiers = [
    vec![web.join("templates").join(name)],
    packages.iter().map(|package| package.join("templates").join(name)).collect(),
    vec![web.join("static").join(name)],
    packages.iter().map(|package| package.join("static").join(name)).collect(),
  ];
  for tier in tiers {
    let mut hits = tier.into_iter().filter(|path| path.is_file());
    match (hits.next(), hits.next()) {
      (None, _) => continue,
      (Some(path), None) => return path,
      (Some(first), Some(second)) => {
        panic!("{name} is held by two packages of one tree: {} and {}", first.display(), second.display())
      }
    }
  }
  panic!("{name} is held by no tree under web/");
}

/// The package directories under web/features and web/backends, each kind in name order.
fn package_directories(web: &Path) -> Vec<PathBuf> {
  let mut packages = Vec::new();
  for kind in ["features", "backends"] {
    let kind_dir = web.join(kind);
    let mut dirs: Vec<PathBuf> = fs::read_dir(&kind_dir)
      .unwrap_or_else(|error| panic!("{}: {error}", kind_dir.display()))
      .map(|entry| entry.expect("readable directory entry").path())
      .filter(|path| path.is_dir())
      .collect();
    dirs.sort();
    packages.extend(dirs);
  }
  packages
}

/// The minified text of `source`, a classic browser script read from `path`.
fn minify(path: &Path, source: &str) -> String {
  let before = top_level_names(path, source);
  let allocator = Allocator::default();
  let mut program = parse(&allocator, path, source);
  let options = MinifierOptions {
    mangle: Some(MangleOptions { top_level: Some(false), ..MangleOptions::default() }),
    mangle_properties: None,
    compress: None,
  };
  let minified = Minifier::new(options).minify(&allocator, &mut program);
  let code = Codegen::new()
    .with_options(CodegenOptions { minify: true, comments: CommentOptions::disabled(), ..CodegenOptions::default() })
    .with_scoping(minified.scoping)
    .build(&program)
    .code;
  let after = top_level_names(path, &code);
  if before != after {
    let lost: Vec<&String> = before.difference(&after).collect();
    panic!("{}: minifying drops or renames top-level names {lost:?}", path.display());
  }
  code
}

/// The names `text` binds at its top level, which the page's other scripts can read.
fn top_level_names(path: &Path, text: &str) -> BTreeSet<String> {
  let allocator = Allocator::default();
  let program = parse(&allocator, path, text);
  let built = SemanticBuilder::new().build(&program);
  let scoping = built.semantic.scoping();
  let root = scoping.root_scope_id();
  scoping.get_bindings(root).values().map(|symbol| scoping.symbol_name(*symbol).to_string()).collect()
}

fn parse<'a>(allocator: &'a Allocator, path: &Path, text: &'a str) -> oxc_ast::ast::Program<'a> {
  let parsed = Parser::new(allocator, text, SourceType::default().with_script(true)).parse();
  if !parsed.diagnostics.is_empty() {
    panic!("{}: does not parse as a classic script: {:?}", path.display(), parsed.diagnostics.first());
  }
  parsed.program
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

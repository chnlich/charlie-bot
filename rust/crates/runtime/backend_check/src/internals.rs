//! The scan's internals: the tree walk, the skip rules and the per-file scan.
//!
//! `api` calls these, and no other module does.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_core::{config, repo_root};

use crate::exemptions;
use crate::rust_scan;
use crate::text_scan;
use crate::types::{Hit, Roots};
use crate::vocabulary::Vocabulary;

/// The directories the walk never enters: version control, build output and environments.
const SKIPPED_DIRS: [&str; 5] = [".git", "target", "node_modules", ".venv", "__pycache__"];

/// The text files under `dir`, in walk order. Hidden files count; ignored files and the skipped directories do not.
pub(crate) fn walk(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
  let walker = ignore::WalkBuilder::new(dir)
    .hidden(false)
    .require_git(false)
    .filter_entry(|entry| {
      entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_dir()) || !is_skipped_dir(entry.path())
    })
    .build();
  let mut files = Vec::new();
  for entry in walker {
    let entry = entry.with_context(|| format!("cannot walk {}", dir.display()))?;
    if entry.file_type().is_some_and(|kind| kind.is_file()) && !is_skipped_file(entry.path()) {
      files.push(entry.into_path());
    }
  }
  Ok(files)
}

/// Whether the walk skips a directory: a build or environment directory, a `tests` tree, or a mock or fixture tree.
fn is_skipped_dir(path: &Path) -> bool {
  path
    .file_name()
    .and_then(|name| name.to_str())
    .is_some_and(|name| SKIPPED_DIRS.contains(&name) || name == "tests" || is_mock_or_fixture(name))
}

/// Whether a name marks mock or fixture code, which is test code: the `mock-home` crate, and the fixture files that
/// tests read. Such code carries its own values for a test to read.
fn is_mock_or_fixture(name: &str) -> bool {
  name.contains("mock") || name.contains("fixture")
}

/// The files that carry values by design and are no source: the profile's `config.yaml`, bytecode, and mock or fixture
/// files.
fn is_skipped_file(path: &Path) -> bool {
  let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
  name == config::CONFIG_FILENAME || name.ends_with(".pyc") || is_mock_or_fixture(name)
}

/// Whether a path is a skipped file or sits under a directory the tree walk skips. Explicit file and directory
/// arguments keep the same test and fixture boundary as a repository-root scan.
pub(crate) fn is_skipped_path(path: &Path) -> bool {
  is_skipped_file(path) || path.parent().is_some_and(|parent| parent.ancestors().any(is_skipped_dir))
}

/// Whether a file lies in the repository's `configs/` directory: the example and default configuration that a host copies
/// or overrides. Its values are the host's to choose, so it is no site.
pub(crate) fn is_configuration(roots: &Roots, path: &Path) -> bool {
  roots
    .repo
    .as_deref()
    .and_then(|repo| path.strip_prefix(repo).ok())
    .is_some_and(|relative| relative.starts_with(repo_root::CONFIGS_DIR_NAME))
}

/// The text of a file, or `None` for a binary file: one with a NUL byte or invalid UTF-8 has no text to scan.
fn read_text(path: &Path) -> anyhow::Result<Option<String>> {
  let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
  if bytes.contains(&0) {
    return Ok(None);
  }
  Ok(String::from_utf8(bytes).ok())
}

/// Scan one file and add its hits.
pub(crate) fn scan_file(vocab: &Vocabulary, roots: &Roots, path: &Path, hits: &mut Vec<Hit>) -> anyhow::Result<()> {
  let Some(text) = read_text(path)? else { return Ok(()) };
  let found = if is_rust(path) {
    rust_scan::scan(vocab, &text).with_context(|| format!("{}: not parseable as Rust", path.display()))?
  } else {
    text_scan::scan_file(vocab, &text, path)
  };
  let key = key_path(roots, path);
  for site in found {
    let exempt = key.and_then(|key| exemptions::find(key, site.class, &site.value));
    hits.push(Hit { path: path.to_path_buf(), line: site.line, class: site.class, value: site.value, exempt });
  }
  Ok(())
}

fn is_rust(path: &Path) -> bool {
  path.extension().is_some_and(|ext| ext == "rs")
}

/// The Rust files that are test code by their declaration: an out-of-line module declared under `#[cfg(test)]`, and
/// every module under one. The walk finds the candidate file of each module declaration among the files it was given.
pub(crate) fn test_only_files(files: &[PathBuf]) -> anyhow::Result<HashSet<PathBuf>> {
  let rust: BTreeSet<&PathBuf> = files.iter().filter(|file| is_rust(file)).collect();
  let mut declared: HashMap<PathBuf, Vec<(PathBuf, bool)>> = HashMap::new();
  let mut gated_roots = Vec::new();
  for file in &rust {
    let Some(text) = read_text(file)? else { continue };
    let parsed = syn::parse_file(&text).with_context(|| format!("{}: not parseable as Rust", file.display()))?;
    let mut children = Vec::new();
    let dir = file.parent().unwrap_or(file);
    declared_modules(&parsed.items, &module_dir(file), dir, false, &rust, &mut children);
    for (child, gated) in children {
      if gated {
        gated_roots.push(child.clone());
      }
      declared.entry((*file).clone()).or_default().push((child, gated));
    }
  }
  let mut test_only = HashSet::new();
  let mut pending = gated_roots;
  while let Some(file) = pending.pop() {
    if test_only.insert(file.clone()) {
      for (child, _) in declared.get(&file).into_iter().flatten() {
        pending.push(child.clone());
      }
    }
  }
  Ok(test_only)
}

/// The modules a file declares out of line, with the file each names and whether a `cfg(test)` gate covers it. A
/// `#[path]` attribute names the file relative to `path_dir`; every other module is found under `base`.
fn declared_modules(
  items: &[syn::Item],
  base: &Path,
  path_dir: &Path,
  gated: bool,
  rust: &BTreeSet<&PathBuf>,
  children: &mut Vec<(PathBuf, bool)>,
) {
  for item in items {
    let syn::Item::Mod(module) = item else { continue };
    let covered = gated || rust_scan::is_test_gated(&module.attrs);
    let name = module.ident.to_string();
    match &module.content {
      None => {
        let target = match path_attr(&module.attrs) {
          Some(value) => path_dir.join(value),
          None => {
            let direct = base.join(format!("{name}.rs"));
            let nested = base.join(&name).join("mod.rs");
            if rust.contains(&direct) { direct } else { nested }
          }
        };
        if rust.contains(&target) {
          children.push((target, covered));
        }
      }
      Some((_, inner)) => {
        let inline = base.join(&name);
        declared_modules(inner, &inline, &inline, covered, rust, children);
      }
    }
  }
}

/// The file that a module's `#[path = "..."]` attribute names, or `None` when the module has no such attribute.
fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
  let attr = attrs.iter().find(|attr| attr.path().is_ident("path"))?;
  let syn::Meta::NameValue(pair) = &attr.meta else { return None };
  let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(text), .. }) = &pair.value else { return None };
  Some(text.value())
}

/// The directory a file's out-of-line modules live in: its own directory for a crate root or a `mod.rs`, and a
/// directory named after the file otherwise.
fn module_dir(file: &Path) -> PathBuf {
  match file.file_name().and_then(|name| name.to_str()) {
    Some("lib.rs" | "main.rs" | "mod.rs") => file.parent().unwrap_or(file).to_path_buf(),
    _ => file.with_extension(""),
  }
}

/// The path of a file under the first root that holds it, which is the path an exemption row names.
fn key_path<'a>(roots: &Roots, path: &'a Path) -> Option<&'a str> {
  let relative = roots.repo.iter().chain(std::iter::once(&roots.home)).find_map(|root| path.strip_prefix(root).ok())?;
  relative.to_str()
}

//! The backend check: finds each hardcoded backend choice in the text source files under the paths it is given.
//!
//! `charliebot backend check` runs this scan, and the CI `rust-lint` job runs that command. The match definition is in
//! `rules`, the vocabulary it matches against in `vocabulary`, the reviewed exceptions in `exemptions`. The Rust files
//! are read by `rust_scan` through their syntax trees; every other text file is read by `text_scan`, which leaves a code
//! file's comments out.

pub mod exemptions;
pub mod rules;
pub mod rust_scan;
pub mod text_scan;
pub mod vocabulary;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_core::{config, repo_root};

pub use exemptions::Exemption;
pub use rules::Class;
pub use vocabulary::Vocabulary;

/// The roots that an exemption's path is read under: the repository root, and the profile home.
pub struct Roots {
  pub repo: Option<PathBuf>,
  pub home: PathBuf,
}

/// One site that a class matches. `exempt` names the row that keeps the site, or is `None` for a site to fix.
#[derive(Debug)]
pub struct Hit {
  pub path: PathBuf,
  pub line: usize,
  pub class: Class,
  pub value: String,
  pub exempt: Option<&'static Exemption>,
}

/// The directories the walk never enters: version control, build output and environments.
const SKIPPED_DIRS: [&str; 5] = [".git", "target", "node_modules", ".venv", "__pycache__"];

/// The profile-home directories a default scan reads: the machinery that a profile's runs read and execute.
pub const PROFILE_SCAN_DIRS: [&str; 4] = ["improve", "prompts", "scripts", "skills"];

/// The paths a scan reads when none is named: the repository, when there is one, and each directory of
/// [`PROFILE_SCAN_DIRS`] that the profile home under `home` holds.
pub fn default_paths(repo: Option<&Path>, home: &Path) -> Vec<PathBuf> {
  let profile = PROFILE_SCAN_DIRS.iter().map(|dir| home.join(dir)).filter(|dir| dir.is_dir());
  repo.map(Path::to_path_buf).into_iter().chain(profile).collect()
}

/// Scan each path: a file as itself, and a directory by the text files under it. The hits come sorted by place, with
/// one line per site: a site that two classes match is reported once, as its default-value class when it has one.
pub fn scan(vocab: &Vocabulary, roots: &Roots, paths: &[PathBuf]) -> anyhow::Result<Vec<Hit>> {
  let mut files = Vec::new();
  for path in paths {
    let path = fs::canonicalize(path).with_context(|| format!("cannot read {}", path.display()))?;
    if path.is_dir() {
      files.extend(walk(&path)?);
    } else {
      files.push(path);
    }
  }
  files.retain(|file| !is_configuration(roots, file) && !is_skipped_path(file));
  let test_only = test_only_files(&files)?;
  let mut hits = Vec::new();
  for file in files.iter().filter(|file| !test_only.contains(*file)) {
    scan_file(vocab, roots, file, &mut hits)?;
  }
  hits.sort_by(|left, right| {
    (&left.path, left.line, &left.value, left.class != Class::DefaultValue, left.class).cmp(&(
      &right.path,
      right.line,
      &right.value,
      right.class != Class::DefaultValue,
      right.class,
    ))
  });
  hits.dedup_by(|later, kept| later.path == kept.path && later.line == kept.line && later.value == kept.value);
  Ok(hits)
}

/// The text files under `dir`, in walk order. Hidden files count; ignored files and the skipped directories do not.
fn walk(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
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
fn is_skipped_path(path: &Path) -> bool {
  is_skipped_file(path) || path.parent().is_some_and(|parent| parent.ancestors().any(is_skipped_dir))
}

/// Whether a file lies in the repository's `configs/` directory: the example and default configuration that a host copies
/// or overrides. Its values are the host's to choose, so it is no site.
fn is_configuration(roots: &Roots, path: &Path) -> bool {
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
fn scan_file(vocab: &Vocabulary, roots: &Roots, path: &Path, hits: &mut Vec<Hit>) -> anyhow::Result<()> {
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
fn test_only_files(files: &[PathBuf]) -> anyhow::Result<HashSet<PathBuf>> {
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

#[cfg(test)]
mod tests {
  use std::collections::BTreeSet;

  use super::*;

  fn vocab() -> Vocabulary {
    Vocabulary { options: BTreeSet::from(["claude-opus".to_string()]), ..Vocabulary::default() }
  }

  fn roots(home: &Path) -> Roots {
    Roots { repo: None, home: home.to_path_buf() }
  }

  /// A module declared under `#[cfg(test)]`, and every module under it, is test code, including one that a `#[path]`
  /// attribute places elsewhere. The production module beside it is scanned, and a fixture file carries its own values
  /// and is not.
  #[test]
  fn test_only_modules_and_fixtures_are_left_out_of_the_scan() {
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join("src");
    fs::create_dir_all(src.join("inline")).unwrap();
    fs::create_dir_all(src.join("elsewhere")).unwrap();
    fs::write(
      src.join("lib.rs"),
      "mod prod;\n#[cfg(test)]\nmod gated_tests;\n#[cfg(test)]\nmod inline { mod nested; }\n\
       #[cfg(test)]\n#[path = \"elsewhere/by_path.rs\"]\nmod by_path;\n",
    )
    .unwrap();
    fs::write(src.join("prod.rs"), "fn f() { let _ = \"claude-opus\"; }\n").unwrap();
    fs::write(src.join("gated_tests.rs"), "fn g() { let _ = \"claude-opus\"; }\n").unwrap();
    fs::write(src.join("inline").join("nested.rs"), "fn h() { let _ = \"claude-opus\"; }\n").unwrap();
    fs::write(src.join("elsewhere").join("by_path.rs"), "fn k() { let _ = \"claude-opus\"; }\n").unwrap();
    fs::write(src.join("prod_fixture.txt"), "model claude-opus\n").unwrap();

    let hits = scan(&vocab(), &roots(home.path()), &[src]).unwrap();
    let files: Vec<&str> = hits.iter().map(|hit| hit.path.file_name().unwrap().to_str().unwrap()).collect();
    assert_eq!(files, vec!["prod.rs"], "{hits:?}");
  }

  /// A site that two classes match is one hit, reported as its default-value class.
  #[test]
  fn one_site_is_one_hit_reported_as_its_default_value() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("run.sh");
    fs::write(&script, "model=\"${MODEL:-claude-opus}\"\n").unwrap();
    let hits = scan(&vocab(), &roots(home.path()), &[script]).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].class, Class::DefaultValue);
    assert_eq!(hits[0].line, 1);
  }

  /// A default scan reads the repository and the profile's machinery directories that exist, in the fixed order.
  #[test]
  fn default_paths_are_the_repo_and_the_profile_machinery_that_exists() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("skills")).unwrap();
    fs::create_dir(home.path().join("improve")).unwrap();
    let paths = default_paths(Some(repo.path()), home.path());
    assert_eq!(paths, vec![repo.path().to_path_buf(), home.path().join("improve"), home.path().join("skills")]);
  }

  /// A file under the repository's `configs/` directory is the example or default configuration that a host copies or
  /// overrides, so it is no site. The same file outside `configs/` is one.
  #[test]
  fn configs_directory_files_are_no_site() {
    let repo = tempfile::tempdir().unwrap();
    let repo_root = fs::canonicalize(repo.path()).unwrap();
    for dir in ["configs", "scripts"] {
      fs::create_dir(repo_root.join(dir)).unwrap();
      fs::write(repo_root.join(dir).join("cron.default.yaml"), "task: claude-opus\n").unwrap();
    }
    let roots = Roots { repo: Some(repo_root.clone()), home: repo_root.join("home") };
    let hits = scan(&vocab(), &roots, std::slice::from_ref(&repo_root)).unwrap();
    let files: Vec<PathBuf> = hits.iter().map(|hit| hit.path.strip_prefix(&repo_root).unwrap().to_path_buf()).collect();
    assert_eq!(files, vec![PathBuf::from("scripts/cron.default.yaml")], "{hits:?}");
  }

  /// Mock and fixture code is test code: a `mock-home` crate, a `mock` name and a `tests` tree carry their own values.
  /// The production file beside them is scanned.
  #[test]
  fn mock_and_fixture_code_is_test_code() {
    let repo = tempfile::tempdir().unwrap();
    let repo_root = fs::canonicalize(repo.path()).unwrap();
    for (file, body) in [
      ("app/mock-home/src/lib.rs", "fn f() { let _ = \"claude-opus\"; }\n"),
      ("app/cli/src/mock_client.rs", "fn g() { let _ = \"claude-opus\"; }\n"),
      ("app/cli/tests/cli.rs", "fn h() { let _ = \"claude-opus\"; }\n"),
      ("app/cli/src/lib.rs", "fn k() { let _ = \"claude-opus\"; }\n"),
    ] {
      let path = repo_root.join(file);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(path, body).unwrap();
    }
    let roots = Roots { repo: Some(repo_root.clone()), home: repo_root.join("home") };
    let hits = scan(&vocab(), &roots, std::slice::from_ref(&repo_root)).unwrap();
    let files: Vec<PathBuf> = hits.iter().map(|hit| hit.path.strip_prefix(&repo_root).unwrap().to_path_buf()).collect();
    assert_eq!(files, vec![PathBuf::from("app/cli/src/lib.rs")], "{hits:?}");

    let explicit =
      scan(&vocab(), &roots, &[repo_root.join("app/mock-home/src/lib.rs"), repo_root.join("app/cli/tests/cli.rs")])
        .unwrap();
    assert!(explicit.is_empty(), "{explicit:?}");
  }
}

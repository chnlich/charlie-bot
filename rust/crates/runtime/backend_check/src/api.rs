//! The scan's public surface: the paths a default scan reads, and the scan itself.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::internals::{is_configuration, is_skipped_path, scan_file, test_only_files, walk};
use crate::rules::Class;
use crate::types::{Hit, Roots};
use crate::vocabulary::Vocabulary;

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

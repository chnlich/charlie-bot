//! The check as its gates run it: the open sites of a scan, and the line that names each one.
//!
//! `charliebot backend check`, the land gate, the cron fire gate and the server-start report all read their sites here.
//! Each refusal therefore has one form, and no gate holds a pattern of its own.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_core::git;

use crate::{Hit, Roots, Vocabulary, scan};

/// The roots a scan reads its exemption paths under: the repository and the profile home, each resolved, so that the
/// paths a scan returns lie under them. A profile home that does not exist holds no file, and stays as given.
pub fn roots(repo: Option<&Path>, home: &Path) -> anyhow::Result<Roots> {
  let repo = repo
    .map(|repo| fs::canonicalize(repo).with_context(|| format!("cannot read the repository {}", repo.display())))
    .transpose()?;
  let home = if home.is_dir() {
    fs::canonicalize(home).with_context(|| format!("cannot read {}", home.display()))?
  } else {
    home.to_path_buf()
  };
  Ok(Roots { repo, home })
}

/// The git work tree that holds `dir`, or `None` when `dir` lies in no work tree.
pub fn work_tree(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
  let output = git::output(dir, &["rev-parse", "--show-toplevel"], &[])
    .with_context(|| format!("cannot run git in {}", dir.display()))?;
  if !output.status.success() {
    return Ok(None);
  }
  let top =
    String::from_utf8(output.stdout).with_context(|| format!("the work tree of {} is not UTF-8", dir.display()))?;
  Ok(Some(PathBuf::from(top.trim_end())))
}

/// The repository that a call over `paths` names: the work tree that holds each path, except a path under the profile
/// home `home`, which is host machinery and names no repository. The working directory plays no part, so a call gives
/// the same result from anywhere. One call scans one repository, so the paths that name one must name the same one.
pub fn named_repo(paths: &[PathBuf], home: &Path) -> anyhow::Result<Option<PathBuf>> {
  let home = roots(None, home)?.home;
  let mut repo: Option<PathBuf> = None;
  for path in paths {
    let path = fs::canonicalize(path).with_context(|| format!("cannot read {}", path.display()))?;
    if path.starts_with(&home) {
      continue;
    }
    let dir = if path.is_dir() { path.as_path() } else { path.parent().unwrap_or(&path) };
    let Some(tree) = work_tree(dir)? else { continue };
    if let Some(named) = repo.as_ref().filter(|named| **named != tree) {
      anyhow::bail!(
        "the paths name two repositories, {} and {}; one call scans one repository",
        named.display(),
        tree.display()
      );
    }
    repo = Some(tree);
  }
  Ok(repo)
}

/// The sites of `paths` that no exemption keeps, under `vocab`. A path that cannot be read is an error: no gate reads a
/// failed scan as a clean one.
pub fn open_hits(vocab: &Vocabulary, roots: &Roots, paths: &[PathBuf]) -> anyhow::Result<Vec<Hit>> {
  let hits = scan(vocab, roots, paths)?;
  Ok(hits.into_iter().filter(|hit| hit.exempt.is_none()).collect())
}

/// The line that names one open site: its file, its line, its class and its matched value, then the practice that
/// replaces it. The file is shown relative to `repo` when it lies there, and as scanned otherwise.
pub fn describe(hit: &Hit, repo: Option<&Path>) -> String {
  let place = repo.and_then(|repo| hit.path.strip_prefix(repo).ok()).unwrap_or(&hit.path);
  format!(
    "{}:{} {} {}  -> backends.calls.<class> via charliebot backend resolve",
    place.display(),
    hit.line,
    hit.class.label(),
    hit.value
  )
}

/// Each open site's line, one per line, in the order the scan sorts them.
pub fn report(hits: &[Hit], repo: Option<&Path>) -> String {
  hits.iter().map(|hit| describe(hit, repo)).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::rules::Class;
  use charliebot_registry::Registry;

  /// Scans two files that each hold a model id, one in `list_price.rs` and one in `scripts/run.sh`. Only
  /// `scripts/run.sh` stays an open hit because the exemption table keeps the price table entry.
  #[test]
  fn open_hits_keep_the_sites_no_exemption_keeps() {
    let root = tempfile::tempdir().unwrap();
    let repo = fs::canonicalize(root.path()).unwrap();
    let price_dir = repo.join("rust/crates/infra/core/src");
    fs::create_dir_all(&price_dir).unwrap();
    fs::write(price_dir.join("list_price.rs"), "static P: &str = \"claude-opus-5-5\";\n").unwrap();
    fs::create_dir_all(repo.join("scripts")).unwrap();
    fs::write(repo.join("scripts/run.sh"), "model=claude-opus-5-5\n").unwrap();

    let paths = [repo.join("rust/crates/infra/core/src"), repo.join("scripts/run.sh")];
    let vocab = Vocabulary::load(&Registry::new(), &repo.join("home"), None).unwrap();
    let hits = open_hits(&vocab, &roots(Some(&repo), &repo.join("home")).unwrap(), &paths).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].path, repo.join("scripts/run.sh"));
    assert_eq!((hits[0].line, hits[0].class), (1, Class::ModelId));
  }

  /// A path that cannot be read is an error that names it, not a scan with no hits.
  #[test]
  fn an_unreadable_path_is_an_error_not_a_clean_scan() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing.sh");
    let vocab = Vocabulary::default();
    let error = open_hits(&vocab, &roots(None, root.path()).unwrap(), &[missing]).unwrap_err();
    assert!(format!("{error:#}").contains("missing.sh"), "{error:#}");
  }

  /// A site's line names its file relative to the repository, its line, its class and its value, and the practice.
  #[test]
  fn a_site_line_names_its_file_line_class_and_value() {
    let hit = Hit {
      path: PathBuf::from("/repo/scripts/run.sh"),
      line: 3,
      class: Class::ModelId,
      value: "claude-opus-5-5".to_string(),
      exempt: None,
    };
    assert_eq!(
      describe(&hit, Some(Path::new("/repo"))),
      "scripts/run.sh:3 model-id claude-opus-5-5  -> backends.calls.<class> via charliebot backend resolve"
    );
    assert!(describe(&hit, None).starts_with("/repo/scripts/run.sh:3 "));
  }

  /// The `k8s/environ/fpt-k8s/helmfile.yaml` exemption passes the three Helm release and values file names at that
  /// path, refuses `muse-voice-transcribe-1.0` added to the same file, and refuses the three names at another path.
  #[test]
  fn fpt_k8s_helmfile_exemption_keeps_only_the_three_names_at_that_path() {
    let root = tempfile::tempdir().unwrap();
    let repo = fs::canonicalize(root.path()).unwrap();
    let fpt_dir = repo.join("k8s/environ/fpt-k8s");
    let other_dir = repo.join("k8s/environ/xinjiang");
    fs::create_dir_all(&fpt_dir).unwrap();
    fs::create_dir_all(&other_dir).unwrap();

    let three_names = "# muse-launcher\n- name: muse-bundle-eval\nvalues:\n- ./muse-bundle-eval.values.yaml\n";
    let helmfile = fpt_dir.join("helmfile.yaml");
    let other_file = other_dir.join("helmfile.yaml");
    fs::write(&helmfile, three_names).unwrap();
    fs::write(&other_file, three_names).unwrap();

    let vocab = Vocabulary::load(&Registry::new(), &repo.join("home"), None).unwrap();
    let scan_roots = roots(Some(&repo), &repo.join("home")).unwrap();

    let pass = open_hits(&vocab, &scan_roots, std::slice::from_ref(&helmfile)).unwrap();
    assert!(pass.is_empty(), "{pass:?}");

    let other_hits = open_hits(&vocab, &scan_roots, &[other_file]).unwrap();
    let other_values: Vec<&str> = other_hits.iter().map(|hit| hit.value.as_str()).collect();
    assert_eq!(other_values, vec!["muse-launcher", "muse-bundle-eval", "muse-bundle-eval.values.yaml"]);

    fs::write(&helmfile, format!("{three_names}model: muse-voice-transcribe-1.0\n")).unwrap();
    let model_hits = open_hits(&vocab, &scan_roots, &[helmfile]).unwrap();
    assert_eq!(model_hits.len(), 1, "{model_hits:?}");
    assert_eq!((model_hits[0].class, model_hits[0].value.as_str()), (Class::ModelId, "muse-voice-transcribe-1.0"));
  }

  fn git_init(dir: &Path) {
    let status = std::process::Command::new("git").args(["init", "-q"]).current_dir(dir).status().unwrap();
    assert!(status.success(), "git init in {}", dir.display());
  }

  /// A call names its repository by its paths. A repository path names the work tree that holds it, a path under the
  /// profile home names none, and two repositories in one call are refused. The working directory is not read.
  #[test]
  fn named_repo_is_the_work_tree_of_the_named_paths() {
    let root = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let repo = base.join("repo");
    let other = base.join("other");
    let home = base.join("home");
    for dir in [&repo, &other, &home] {
      fs::create_dir_all(dir).unwrap();
    }
    git_init(&repo);
    git_init(&other);
    fs::create_dir_all(home.join("improve")).unwrap();
    fs::write(home.join("improve/run.sh"), "echo\n").unwrap();
    fs::create_dir_all(repo.join("scripts")).unwrap();
    fs::write(repo.join("scripts/run.sh"), "echo\n").unwrap();

    let file = repo.join("scripts/run.sh");
    assert_eq!(named_repo(std::slice::from_ref(&file), &home).unwrap(), Some(repo.clone()));
    assert_eq!(named_repo(&[repo.clone(), home.join("improve")], &home).unwrap(), Some(repo.clone()));
    assert_eq!(named_repo(&[home.join("improve")], &home).unwrap(), None);
    let error = named_repo(&[repo.clone(), other], &home).unwrap_err();
    assert!(format!("{error:#}").contains("two repositories"), "{error:#}");
  }

  /// One check over a repository root and a host root keeps each root's rows. The repository's exempt file and the host
  /// file with no literal give no hit. A literal in the host file is the one hit, under that host file.
  #[test]
  fn one_check_over_a_repo_root_and_a_host_root_keeps_each_roots_rows() {
    let root = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let repo = base.join("repo");
    let home = base.join("home");
    let price_dir = repo.join("rust/crates/infra/core/src");
    fs::create_dir_all(&price_dir).unwrap();
    fs::write(price_dir.join("list_price.rs"), "static P: &str = \"claude-opus-5-5\";\n").unwrap();
    fs::create_dir_all(home.join("improve")).unwrap();
    let host_file = home.join("improve/run.sh");
    fs::write(&host_file, "echo ok\n").unwrap();

    let vocab = Vocabulary::load(&Registry::new(), &home, None).unwrap();
    let scan_roots = roots(Some(&repo), &home).unwrap();
    let paths = [repo.clone(), home.join("improve")];
    let hits = open_hits(&vocab, &scan_roots, &paths).unwrap();
    assert!(hits.is_empty(), "{hits:?}");

    fs::write(&host_file, "model=claude-opus-5-5\n").unwrap();
    let hits = open_hits(&vocab, &scan_roots, &paths).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].path, host_file);
    assert_eq!((hits[0].line, hits[0].class), (1, Class::ModelId));
  }
}

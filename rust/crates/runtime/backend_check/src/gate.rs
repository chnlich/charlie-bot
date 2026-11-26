//! The check as its gates run it: the open sites of a scan, and the line that names each one.
//!
//! `charliebot backend check`, the land gate, the cron fire gate and the server-start report all read their sites here.
//! Each refusal therefore has one form, and no gate holds a pattern of its own.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

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
}

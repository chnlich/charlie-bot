//! Every `skills/<name>/<rest>` reference in prompts/ resolves inside the repo.
//!
//! The prompts name genre and skill files at the rule that triggers writing (the worker commit step, the master
//! draft rule, the cron jobs). A renamed or moved file leaves such a pointer dangling and the rule silently
//! unwritten, so the test walks every prompts/ file and resolves each reference against skills/.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

/// A reference is `skills/<name>/<rest>`. The rest stops at the first character a repo path cannot contain
/// (whitespace, quote, paren, backtick), so the markdown punctuation around a reference never joins the path; a
/// trailing sentence period rides inside the rest and is stripped below.
const REFERENCE: &str = r"skills/(?P<name>[A-Za-z0-9_-]+)/(?P<rest>[A-Za-z0-9_./-]+)";

/// Sentence punctuation a reference may pick up: `)`, `.`, `,`, backtick, quotes.
const TRAILING_PUNCTUATION: [char; 6] = [')', '.', ',', '`', '"', '\''];

/// Every file under `dir`, in no order yet.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
  let mut entries: Vec<PathBuf> = fs::read_dir(dir)
    .unwrap_or_else(|error| panic!("read_dir {}: {error}", dir.display()))
    .map(|entry| entry.expect("dir entry").path())
    .collect();
  entries.sort();
  for path in entries {
    if path.is_dir() {
      collect_files(&path, out);
    } else {
      out.push(path);
    }
  }
}

#[test]
fn prompt_skill_references_resolve() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("repo root");
  let prompts = root.join("prompts");
  let skills = root.join("skills");
  let skill_names: BTreeSet<String> = fs::read_dir(&skills)
    .unwrap_or_else(|error| panic!("read_dir {}: {error}", skills.display()))
    .map(|entry| entry.expect("dir entry"))
    .filter(|entry| entry.file_type().expect("dir entry type").is_dir())
    .map(|entry| entry.file_name().to_string_lossy().into_owned())
    .collect();
  let reference = Regex::new(REFERENCE).expect("the reference regex parses");
  let mut paths = Vec::new();
  collect_files(&prompts, &mut paths);
  paths.sort();

  let mut checked = 0;
  let mut broken = Vec::new();
  for path in paths {
    let rel = path.strip_prefix(root).expect("prompts/ file under the repo root").to_string_lossy();
    let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    for (number, line) in text.lines().enumerate() {
      for capture in reference.captures_iter(line) {
        let name = capture.name("name").expect("the name group").as_str();
        if !skill_names.contains(name) {
          continue;
        }
        checked += 1;
        let rest = capture.name("rest").expect("the rest group").as_str().trim_end_matches(TRAILING_PUNCTUATION);
        let target = skills.join(name).join(rest);
        if !target.exists() {
          broken.push(format!(
            "{rel}:{}: {} -> {} does not exist",
            number + 1,
            capture.get(0).expect("the whole match").as_str(),
            target.strip_prefix(root).expect("target under the repo root").display()
          ));
        }
      }
    }
  }

  assert!(checked > 0, "no skills/<name>/<rest> reference found under prompts/ - the scan matched nothing");
  assert!(
    broken.is_empty(),
    "prompts/ reference(s) under skills/ do not resolve in the repo:\n  {}",
    broken.join("\n  ")
  );
}

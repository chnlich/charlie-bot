use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use charliebot_cli_common::exit::CliExit;
use charliebot_core::str_methods;
use unicode_width::UnicodeWidthChar;

use crate::memory::{STAGING_DIRNAME, TOPICS_FILENAME, Topic, parse_topics_text, validate_entry};

pub(super) const BODY_LINE_COLUMN_LIMIT: usize = 120;

pub(super) fn lint_store(root: &Path) -> Result<(Vec<String>, Vec<String>), CliExit> {
  let topics_path = root.join(TOPICS_FILENAME);
  let mut violations = Vec::new();
  let topics = if !topics_path.is_file() {
    violations.push(format!("{}: topics vocabulary file not found", topics_path.display()));
    BTreeMap::new()
  } else {
    match load_topics_for_lint(&topics_path)? {
      Ok(topics) => topics,
      Err(violation) => {
        violations.push(violation);
        BTreeMap::new()
      }
    }
  };
  let entry_files = entry_files(root)?;
  for path in &entry_files {
    match crate::memory::parse_entry(path) {
      Ok(entry) => violations.extend(validate_entry(&entry, &topics)),
      Err(error) if error.downcast_ref::<crate::memory::MemoryFormatError>().is_some() => {
        violations.push(error.to_string());
      }
      Err(error) => return Err(CliExit::error(error.to_string())),
    }
  }
  let warnings = entry_warnings(root, &entry_files)?;
  let staging_dir = root.join(STAGING_DIRNAME);
  if staging_dir.is_dir() {
    for path in sorted_markdown_files(&staging_dir)? {
      let text = read_text(&path)?;
      let first_line = text.split('\n').next().unwrap_or_default();
      if text.is_empty() {
        violations.push(format!("{}: empty capture file", path.display()));
      } else if !first_line.starts_with("# ") {
        violations.push(format!("{}: line 1: capture must start with '# <title>'", path.display()));
      } else if str_methods::strip(&first_line[2..]).is_empty() {
        violations.push(format!("{}: line 1: empty title after '# '", path.display()));
      }
    }
  }
  Ok((violations, warnings))
}

fn load_topics_for_lint(path: &Path) -> Result<Result<BTreeMap<String, Topic>, String>, CliExit> {
  let text = read_text(path)?;
  Ok(parse_topics_text(path, &text))
}

pub(super) fn entry_files(root: &Path) -> Result<Vec<PathBuf>, CliExit> {
  crate::memory::iter_entry_files(root).map_err(|error| CliExit::error(format!("{error:#}")))
}

pub(super) fn sorted_markdown_files(directory: &Path) -> Result<Vec<PathBuf>, CliExit> {
  crate::memory::sorted_markdown_files(directory).map_err(|error| CliExit::error(format!("{error:#}")))
}

fn entry_warnings(root: &Path, files: &[PathBuf]) -> Result<Vec<String>, CliExit> {
  let mut warnings = Vec::new();
  let mut longest = None;
  for path in files {
    let text = read_text(path)?;
    let relative = path.strip_prefix(root).expect("entry files are under their store root").to_string_lossy();
    let body = entry_body(&text);
    if longest.as_ref().is_none_or(|(count, _)| body.len() > *count) {
      longest = Some((body.len(), relative.to_string()));
    }
    for line in body {
      let width = display_width(line);
      if width > BODY_LINE_COLUMN_LIMIT {
        let preview: String = line.chars().take(60).collect();
        warnings.push(format!("{relative}: body line over {BODY_LINE_COLUMN_LIMIT} columns ({width}): {preview}"));
      }
    }
  }
  if let Some((count, relative)) = longest {
    warnings.push(format!("body_lines: {} entries, max {count} ({relative})", files.len()));
  }
  Ok(warnings)
}

pub(super) fn entry_body(text: &str) -> Vec<&str> {
  let lines: Vec<&str> = text.split('\n').collect();
  if str_methods::strip(lines[0]) != "---" {
    return lines.into_iter().filter(|line| !str_methods::strip(line).is_empty()).collect();
  }
  for (index, line) in lines.iter().enumerate().skip(1) {
    if str_methods::strip(line) == "---" {
      return lines[index + 1..].iter().copied().filter(|line| !str_methods::strip(line).is_empty()).collect();
    }
  }
  Vec::new()
}

pub(super) fn display_width(line: &str) -> usize {
  line.chars().map(|character| UnicodeWidthChar::width(character).unwrap_or(1).max(1)).sum()
}

pub(super) fn read_text(path: &Path) -> Result<String, CliExit> {
  crate::memory::read_text(path).map_err(|error| CliExit::error(format!("{error:#}")))
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;

  #[test]
  fn test_lint_reports_a_missing_topics_file_without_creating_the_scaffold() {
    // The lint refuses a missing topics file without creating the scaffold.
    let directory = tempfile::tempdir().unwrap();
    let memory_dir = directory.path().join("bare");
    fs::create_dir(&memory_dir).unwrap();

    let (violations, warnings) = lint_store(&memory_dir).unwrap();

    assert!(violations.iter().any(|violation| violation.contains("topics")), "{violations:?}");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!memory_dir.join(TOPICS_FILENAME).exists());
    assert!(!memory_dir.join(".git").exists());
  }
}

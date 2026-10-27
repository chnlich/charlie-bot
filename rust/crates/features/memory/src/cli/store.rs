use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use charliebot_cli_common::exit::CliExit;
use charliebot_core::pyfmt::str_repr;
use charliebot_core::pystr;
use unicode_width::UnicodeWidthChar;

pub(super) const TOPICS_FILENAME: &str = "topics";
const ENTRIES_DIRNAME: &str = "entries";
pub(super) const STAGING_DIRNAME: &str = "staging";
const BODY_LINE_COLUMN_LIMIT: usize = 120;

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
  let warnings = width_warnings(root, &entry_files)?;
  let staging_dir = root.join(STAGING_DIRNAME);
  if staging_dir.is_dir() {
    for path in sorted_markdown_files(&staging_dir)? {
      let text = read_text(&path)?;
      let first_line = text.split('\n').next().unwrap_or_default();
      if text.is_empty() {
        violations.push(format!("{}: empty capture file", path.display()));
      } else if !first_line.starts_with("# ") {
        violations.push(format!("{}: line 1: capture must start with '# <title>'", path.display()));
      } else if pystr::strip(&first_line[2..]).is_empty() {
        violations.push(format!("{}: line 1: empty title after '# '", path.display()));
      }
    }
  }
  Ok((violations, warnings))
}

fn load_topics_for_lint(path: &Path) -> Result<Result<BTreeMap<String, bool>, String>, CliExit> {
  let text = read_text(path)?;
  let mut topics = BTreeMap::new();
  for (index, raw) in text.split('\n').enumerate() {
    if raw.is_empty() {
      continue;
    }
    let line_number = index + 1;
    let Some((name, resident)) = parse_topic_line(raw) else {
      return Ok(Err(format!("{}: line {line_number}: malformed topic line: {}", path.display(), str_repr(raw))));
    };
    if topics.contains_key(name) {
      return Ok(Err(format!("{}: line {line_number}: duplicate topic {}", path.display(), str_repr(name))));
    }
    topics.insert(name.to_string(), resident);
  }
  Ok(Ok(topics))
}

fn parse_topic_line(raw: &str) -> Option<(&str, bool)> {
  let (name, resident) = match raw.strip_suffix(" resident") {
    Some(name) => (name, true),
    None => (raw, false),
  };
  let mut chars = name.chars();
  chars.next().filter(|character| character.is_ascii_lowercase() || character.is_ascii_digit())?;
  chars
    .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
    .then_some((name, resident))
}

fn validate_entry(entry: &crate::memory::Entry, topics: &BTreeMap<String, bool>) -> Vec<String> {
  let topic_label = entry.topic.as_deref().unwrap_or("?");
  let violation = |message: String| format!("entries/{topic_label}/{}.md: {message}", entry.slug);
  let mut violations = Vec::new();
  if !is_slug(&entry.slug) {
    violations
      .push(violation(format!("filename slug {} does not match slug charset [A-Za-z0-9._-]", str_repr(&entry.slug))));
  }
  match entry.topic.as_deref() {
    None => violations.push(violation("missing required header field 'topic'".to_string())),
    Some(topic) if !is_topic_name(topic) => {
      violations.push(violation(format!("topic {} is not a valid topic name", str_repr(topic))));
    }
    Some(_) => {}
  }
  if let Some(topic) = entry.topic.as_deref().filter(|topic| !topics.contains_key(*topic)) {
    violations.push(violation(format!("topic {} not in topics vocabulary", str_repr(topic))));
  }
  let parent_name = entry.path.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy();
  if let Some(topic) = entry.topic.as_deref().filter(|topic| *topic != parent_name) {
    violations.push(violation(format!("directory name {} != topic {}", str_repr(&parent_name), str_repr(topic))));
  }
  if entry.scope.is_none() {
    violations.push(violation("missing required header field 'scope'".to_string()));
  }
  if entry.audience.is_none() {
    violations.push(violation("missing required header field 'audience'".to_string()));
  }
  if let Some(scope) = entry.scope.as_deref().filter(|scope| !["user", "host"].contains(scope)) {
    violations.push(violation(format!("scope {} not in {{user, host}}", str_repr(scope))));
  }
  for element in entry.audience.iter().flatten().filter(|element| !["master", "worker"].contains(&element.as_str())) {
    violations.push(violation(format!("audience element {} not in {{master, worker}}", str_repr(element))));
  }
  violations
}

pub(super) fn entry_files(root: &Path) -> Result<Vec<PathBuf>, CliExit> {
  let entries_dir = root.join(ENTRIES_DIRNAME);
  if !entries_dir.is_dir() {
    return Ok(Vec::new());
  }
  let mut files = Vec::new();
  for topic_dir in sorted_children(&entries_dir)? {
    if topic_dir.is_dir() {
      files.extend(sorted_markdown_files(&topic_dir)?);
    }
  }
  Ok(files)
}

pub(super) fn sorted_markdown_files(directory: &Path) -> Result<Vec<PathBuf>, CliExit> {
  Ok(
    sorted_children(directory)?
      .into_iter()
      .filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().ends_with(".md")))
      .collect(),
  )
}

fn sorted_children(directory: &Path) -> Result<Vec<PathBuf>, CliExit> {
  let mut paths = fs::read_dir(directory)
    .map_err(|error| CliExit::error(format!("list {}: {error}", directory.display())))?
    .map(|entry| entry.map(|entry| entry.path()).map_err(|error| CliExit::error(error.to_string())))
    .collect::<Result<Vec<_>, _>>()?;
  paths.sort();
  Ok(paths)
}

fn width_warnings(root: &Path, files: &[PathBuf]) -> Result<Vec<String>, CliExit> {
  let mut warnings = Vec::new();
  for path in files {
    let text = read_text(path)?;
    let relative = path.strip_prefix(root).expect("entry files are under their store root").to_string_lossy();
    for line in entry_body(&text) {
      let width = display_width(line);
      if width > BODY_LINE_COLUMN_LIMIT {
        let preview: String = line.chars().take(60).collect();
        warnings.push(format!("{relative}: body line over {BODY_LINE_COLUMN_LIMIT} columns ({width}): {preview}"));
      }
    }
  }
  Ok(warnings)
}

fn entry_body(text: &str) -> Vec<&str> {
  let lines: Vec<&str> = text.split('\n').collect();
  if pystr::strip(lines[0]) != "---" {
    return lines.into_iter().filter(|line| !pystr::strip(line).is_empty()).collect();
  }
  for (index, line) in lines.iter().enumerate().skip(1) {
    if pystr::strip(line) == "---" {
      return lines[index + 1..].iter().copied().filter(|line| !pystr::strip(line).is_empty()).collect();
    }
  }
  Vec::new()
}

fn display_width(line: &str) -> usize {
  line.chars().map(|character| UnicodeWidthChar::width(character).unwrap_or(1).max(1)).sum()
}

fn is_slug(value: &str) -> bool {
  !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_topic_name(value: &str) -> bool {
  let mut chars = value.chars();
  chars.next().is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
    && chars.all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
}

pub(super) fn read_text(path: &Path) -> Result<String, CliExit> {
  let text = fs::read_to_string(path).map_err(|error| CliExit::error(format!("read {}: {error}", path.display())))?;
  Ok(normalize_newlines(&text))
}

pub(super) fn normalize_newlines(text: &str) -> String {
  text.replace("\r\n", "\n").replace('\r', "\n")
}

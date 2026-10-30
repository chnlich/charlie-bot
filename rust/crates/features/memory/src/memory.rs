//! The labeled-entry memory store's server half (`src/features/memory/memory.py`): the entry grammar (format v2), the
//! topics vocabulary, `ensure_store`, `load_store`, and the audience selections a run's instructions take. The `memory`
//! CLI, its lint and `memory_proposal.py` stay in Python.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, bail};
use charliebot_core::text_repr::str_repr;
use charliebot_core::str_methods;

const TOPICS_FILENAME: &str = "topics";
const ENTRIES_DIRNAME: &str = "entries";
const STAGING_DIRNAME: &str = "staging";
const GITIGNORE_FILENAME: &str = ".gitignore";

/// The vocabulary seeded when a store has no `topics` file (`DEFAULT_MEMORY_TOPICS`).
pub const DEFAULT_MEMORY_TOPICS: &str =
  "profile resident\ncommunication resident\nworkflow resident\nrulings resident\nhost resident\ncharliebot\n";

/// The `.gitignore` seeded when a store has none (`DEFAULT_MEMORY_GITIGNORE`).
pub const DEFAULT_MEMORY_GITIGNORE: &str = "staging/\n";

/// The header line of every selection's index (`INDEX_HEADER`).
pub const INDEX_HEADER: &str = concat!(
  "# Memory index — full text via `charliebot memory query --topic <topic>` ",
  "(topic = segment before \"/\", e.g. `--topic integrations`)",
);

/// The usage line that ends a worker selection (`WORKER_USAGE_LINE`).
pub const WORKER_USAGE_LINE: &str = concat!(
  "On-demand knowledge: `charliebot memory query --topic <topic>` (full text) or `--index` ",
  "for the index only. Stage a capture with `charliebot memory add [--file F]`: a capture ",
  "is one file, first line `# <title>`, stating one fact to record or one change to ",
  "propose, naming the target entry in the body when proposing a change (writes staging/, ",
  "never entries/).",
);

const KNOWN_FIELDS: &[&str] = &["scope", "topic", "audience", "title"];
const SCOPES: &[&str] = &["user", "host"];
const AUDIENCE_ELEMENTS: &[&str] = &["master", "worker"];

/// A store file violates the entry or topic grammar. The text is the message of Python's `MemoryFormatError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryFormatError(pub String);

impl std::fmt::Display for MemoryFormatError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::error::Error for MemoryFormatError {}

fn format_error(message: String) -> anyhow::Error {
  MemoryFormatError(message).into()
}

/// One vocabulary line: its topic name and whether the topic is resident (`memory.Topic`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topic {
  pub name: String,
  pub resident: bool,
}

/// One parsed entry file (`memory.Entry`). A header field the file leaves out is `None`; validation rejects those.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
  pub path: PathBuf,
  pub topic: Option<String>,
  pub slug: String,
  pub scope: Option<String>,
  pub audience: Option<Vec<String>>,
  pub title: String,
  pub body: String,
}

impl Entry {
  /// `<topic>/<slug>`, the entry's identity. Only a validated entry names its topic.
  pub fn id(&self) -> String {
    format!("{}/{}", self.topic(), self.slug)
  }

  fn topic(&self) -> &str {
    self.topic.as_deref().expect("a validated entry names its topic")
  }
}

/// One loaded store: the memory dir, the topics vocabulary and the parsed entries (`memory.Store`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
  pub memory_dir: PathBuf,
  pub topics: BTreeMap<String, Topic>,
  pub entries: Vec<Entry>,
}

/// One selected entry's provenance and contribution (`memory.MemoryEntrySource`). `delivery` is `full` or `index`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryEntrySource {
  pub source_ref: String,
  pub delivery: &'static str,
  pub text: String,
}

/// One ordered segment of a selection (`memory.MemorySelection.segments`): its delivery, text and sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemorySegment {
  pub delivery: &'static str,
  pub text: String,
  pub sources: Vec<MemoryEntrySource>,
}

/// The memory that one audience's selection produced (`memory.MemorySelection`). `usage_line` rides only on worker
/// selections, and `text` is the segments joined, or `None` when there are none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemorySelection {
  pub audience: &'static str,
  pub repo_basename: Option<String>,
  pub segments: Vec<MemorySegment>,
  pub usage_line: Option<&'static str>,
  pub text: Option<String>,
}

/// Serializes `ensure_store` in the process (`_ENSURE_STORE_LOCK`): a `git init` that starts while another runs in the
/// same directory exits 128.
static ENSURE_STORE_LOCK: Mutex<()> = Mutex::new(());

/// Creates the store scaffold under `memory_dir`, idempotently (`memory.ensure_store`). It runs `git init` when `.git`
/// is absent, seeds `topics` and `.gitignore` only when missing, and creates `entries/` and `staging/`.
pub fn ensure_store(memory_dir: &Path) -> anyhow::Result<()> {
  let _guard = ENSURE_STORE_LOCK.lock().expect("the memory store lock was poisoned by an earlier ensure_store");
  fs::create_dir_all(memory_dir).with_context(|| format!("create {}", memory_dir.display()))?;
  if !memory_dir.join(".git").exists() {
    git_init(memory_dir)?;
  }
  seed_if_missing(&memory_dir.join(TOPICS_FILENAME), DEFAULT_MEMORY_TOPICS)?;
  seed_if_missing(&memory_dir.join(GITIGNORE_FILENAME), DEFAULT_MEMORY_GITIGNORE)?;
  make_dir(&memory_dir.join(ENTRIES_DIRNAME))?;
  make_dir(&memory_dir.join(STAGING_DIRNAME))?;
  Ok(())
}

fn git_init(memory_dir: &Path) -> anyhow::Result<()> {
  let output = Command::new("git")
    .arg("init")
    .current_dir(memory_dir)
    .output()
    .with_context(|| format!("run git init in {}", memory_dir.display()))?;
  if !output.status.success() {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    bail!("git init in {} failed with {}: {}", memory_dir.display(), output.status, text.trim_end());
  }
  Ok(())
}

fn seed_if_missing(path: &Path, content: &str) -> anyhow::Result<()> {
  if path.exists() {
    return Ok(());
  }
  fs::write(path, content).with_context(|| format!("write {}", path.display()))
}

/// `mkdir(exist_ok=True)`: an existing directory is fine, and an existing file at the path is an error.
fn make_dir(path: &Path) -> anyhow::Result<()> {
  match fs::create_dir(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
    Err(error) => Err(error).with_context(|| format!("create {}", path.display())),
  }
}

/// Loads the topics vocabulary and every entry, failing on the first violation (`memory.load_store`). It runs
/// [`ensure_store`] first, so a missing store loads as the empty store with the seeded vocabulary.
pub fn load_store(memory_dir: &Path) -> anyhow::Result<Store> {
  ensure_store(memory_dir)?;
  let topics = load_topics(memory_dir)?;
  let mut entries = Vec::new();
  for md_file in iter_entry_files(memory_dir)? {
    let entry = parse_entry(&md_file)?;
    if let Some(violation) = validate_entry(&entry, &topics).into_iter().next() {
      return Err(format_error(violation));
    }
    entries.push(entry);
  }
  Ok(Store { memory_dir: memory_dir.to_path_buf(), topics, entries })
}

/// Reads a text file the way Python's `Path.read_text` does: universal newlines turn `\r\n` and a lone `\r` into `\n`.
fn read_text(path: &Path) -> anyhow::Result<String> {
  let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Parses one entry file (`memory.parse_entry`).
pub fn parse_entry(path: &Path) -> anyhow::Result<Entry> {
  let text = read_text(path)?;
  Ok(parse_entry_text(&text, path)?)
}

/// Parses one entry file's text (`memory.parse_entry_text`). Only the first header block is parsed; the body is the rest,
/// so it may hold `---` lines. Structure only: `load_store` checks the semantic rules.
pub fn parse_entry_text(text: &str, entry_path: &Path) -> Result<Entry, MemoryFormatError> {
  let location = entry_path.display();
  let malformed = |line_no: usize, line: &str| {
    MemoryFormatError(format!("{location}: line {line_no}: malformed header line: {}", str_repr(line)))
  };
  let lines: Vec<&str> = text.split('\n').collect();
  if lines[0] != "---" {
    return Err(MemoryFormatError(format!("{location}: line 1: expected '---' front matter opener")));
  }
  let mut header: BTreeMap<&str, String> = BTreeMap::new();
  let mut i = 1;
  while i < lines.len() && lines[i] != "---" {
    let line = lines[i];
    let line_no = i + 1;
    let Some((key, value)) = split_header_line(line) else {
      return Err(malformed(line_no, line));
    };
    if !KNOWN_FIELDS.contains(&key) {
      return Err(MemoryFormatError(format!("{location}: line {line_no}: unknown header field {}", str_repr(key))));
    }
    if header.contains_key(key) {
      return Err(MemoryFormatError(format!("{location}: line {line_no}: duplicate header field {}", str_repr(key))));
    }
    let value = match key {
      "title" => {
        let title = str_methods::strip(value);
        if title.is_empty() {
          return Err(MemoryFormatError(format!("{location}: line {line_no}: empty 'title' header value")));
        }
        title.to_string()
      }
      "audience" if is_audience_value(value) => value.to_string(),
      "audience" => return Err(malformed(line_no, line)),
      _ if is_slug(value) => value.to_string(),
      _ => return Err(malformed(line_no, line)),
    };
    header.insert(key, value);
    i += 1;
  }
  if i >= lines.len() {
    return Err(MemoryFormatError(format!("{location}: missing closing '---' after header")));
  }
  // lines[i] is the closing `---`; the body is everything after it.
  let body_lines = &lines[i + 1..];
  if body_lines.is_empty() || (body_lines.len() == 1 && body_lines[0].is_empty()) {
    return Err(MemoryFormatError(format!("{location}: line {}: empty body", i + 2)));
  }
  let Some(title) = header.get("title") else {
    return Err(MemoryFormatError(format!("{location}: missing header field 'title'")));
  };
  Ok(Entry {
    path: entry_path.to_path_buf(),
    topic: header.get("topic").cloned(),
    slug: entry_path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default(),
    scope: header.get("scope").cloned(),
    audience: header.get("audience").map(|raw| raw.split(',').map(|part| str_methods::strip(part).to_string()).collect()),
    title: title.clone(),
    body: body_lines.join("\n"),
  })
}

/// `^([a-z_]+): (.+)$` (`_HEADER_RE`): the field name and the value of one header line.
fn split_header_line(line: &str) -> Option<(&str, &str)> {
  let key_len = line.bytes().take_while(|b| b.is_ascii_lowercase() || *b == b'_').count();
  if key_len == 0 {
    return None;
  }
  let value = line[key_len..].strip_prefix(": ")?;
  (!value.is_empty()).then(|| (&line[..key_len], value))
}

/// `^[A-Za-z0-9._-]+$` (`_SLUG_RE`).
fn is_slug(value: &str) -> bool {
  !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `^[A-Za-z0-9._-]+( *, *[A-Za-z0-9._-]+)*$` (`_AUDIENCE_VALUE_RE`): slug elements separated by commas, with spaces
/// allowed around a comma but not at either end of the value.
fn is_audience_value(value: &str) -> bool {
  let elements: Vec<&str> = value.split(',').collect();
  let last = elements.len() - 1;
  elements.iter().enumerate().all(|(index, element)| {
    let element = if index > 0 { element.trim_start_matches(' ') } else { element };
    let element = if index < last { element.trim_end_matches(' ') } else { element };
    is_slug(element)
  })
}

/// `^[a-z0-9][a-z0-9-]*$` (`TOPIC_NAME_RE`).
fn is_topic_name(name: &str) -> bool {
  let mut chars = name.chars();
  chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// One vocabulary line, `name` or `name resident` (`_TOPIC_LINE_RE`).
fn parse_topic_line(raw: &str) -> Option<(&str, bool)> {
  let (name, resident) = match raw.strip_suffix(" resident") {
    Some(name) => (name, true),
    None => (raw, false),
  };
  is_topic_name(name).then_some((name, resident))
}

fn load_topics(memory_dir: &Path) -> anyhow::Result<BTreeMap<String, Topic>> {
  let topics_path = memory_dir.join(TOPICS_FILENAME);
  if !topics_path.is_file() {
    return Err(format_error(format!("{}: topics vocabulary file not found", topics_path.display())));
  }
  let text = read_text(&topics_path)?;
  let mut topics = BTreeMap::new();
  for (index, raw) in text.split('\n').enumerate() {
    if raw.is_empty() {
      continue;
    }
    let lineno = index + 1;
    let Some((name, resident)) = parse_topic_line(raw) else {
      return Err(format_error(format!(
        "{}: line {lineno}: malformed topic line: {}",
        topics_path.display(),
        str_repr(raw)
      )));
    };
    if topics.contains_key(name) {
      return Err(format_error(format!(
        "{}: line {lineno}: duplicate topic {}",
        topics_path.display(),
        str_repr(name)
      )));
    }
    topics.insert(name.to_string(), Topic { name: name.to_string(), resident });
  }
  Ok(topics)
}

/// The semantic violations of one entry against the vocabulary, in the order Python reports them (`_validate_entry`).
/// Empty when the entry is valid.
fn validate_entry(entry: &Entry, topics: &BTreeMap<String, Topic>) -> Vec<String> {
  let topic_label = entry.topic.as_deref().unwrap_or("?");
  let violation = |message: String| format!("{ENTRIES_DIRNAME}/{topic_label}/{}.md: {message}", entry.slug);
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
  let parent_name = entry.path.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy().into_owned();
  if let Some(topic) = entry.topic.as_deref().filter(|topic| *topic != parent_name) {
    violations.push(violation(format!("directory name {} != topic {}", str_repr(&parent_name), str_repr(topic))));
  }
  if entry.scope.is_none() {
    violations.push(violation("missing required header field 'scope'".to_string()));
  }
  if entry.audience.is_none() {
    violations.push(violation("missing required header field 'audience'".to_string()));
  }
  if let Some(scope) = entry.scope.as_deref().filter(|scope| !SCOPES.contains(scope)) {
    violations.push(violation(format!("scope {} not in {{user, host}}", str_repr(scope))));
  }
  for element in entry.audience.iter().flatten().filter(|element| !AUDIENCE_ELEMENTS.contains(&element.as_str())) {
    violations.push(violation(format!("audience element {} not in {{master, worker}}", str_repr(element))));
  }
  violations
}

/// The sorted entry files: each `*.md` directly under a directory of `entries/` (`memory._iter_entry_files`).
fn iter_entry_files(memory_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
  let entries_dir = memory_dir.join(ENTRIES_DIRNAME);
  if !entries_dir.is_dir() {
    return Ok(Vec::new());
  }
  let mut files = Vec::new();
  for topic_dir in sorted_children(&entries_dir)? {
    if !topic_dir.is_dir() {
      continue;
    }
    for file in sorted_children(&topic_dir)? {
      if file.file_name().is_some_and(|name| name.to_string_lossy().ends_with(".md")) {
        files.push(file);
      }
    }
  }
  Ok(files)
}

fn sorted_children(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
  let children = fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))?;
  let mut paths = children.map(|child| child.map(|child| child.path())).collect::<io::Result<Vec<_>>>()?;
  paths.sort();
  Ok(paths)
}

/// `# <title>` followed by a blank line and the body, or the body alone when it opens with its own `# ` heading
/// (`memory.full_text`).
pub fn full_text(entry: &Entry) -> String {
  let body = entry.body.trim_end_matches('\n');
  if body.starts_with("# ") { body.to_string() } else { format!("# {}\n\n{body}", entry.title) }
}

/// The index line of one entry: `<topic>/<slug> · <title>`.
fn index_line(entry: &Entry) -> String {
  format!("{} · {}", entry.id(), entry.title)
}

/// Whether the entry's audience header names `audience` (`memory.audience_allows`). An entry without an audience
/// header admits nobody.
pub fn audience_allows(entry: &Entry, audience: &str) -> bool {
  entry.audience.as_ref().is_some_and(|audiences| audiences.iter().any(|element| element == audience))
}

/// The store's canonical entry order, `(topic, slug)` (`memory.entry_order_key`).
pub fn entry_order_key(entry: &Entry) -> (Option<&str>, &str) {
  (entry.topic.as_deref(), entry.slug.as_str())
}

/// The names of the store's resident topics (`memory.resident_topic_names`).
pub fn resident_topic_names(store: &Store) -> BTreeSet<&str> {
  store.topics.values().filter(|topic| topic.resident).map(|topic| topic.name.as_str()).collect()
}

/// The master selection: the full text of entries in resident topics, then the index lines of the other master
/// entries. `None` when the audience has no entry (`memory.select_master_memory`).
pub fn select_master_memory(memory_dir: &Path) -> anyhow::Result<Option<MemorySelection>> {
  let store = load_store(memory_dir)?;
  let residents = resident_topic_names(&store);
  let mut full = Vec::new();
  let mut index = Vec::new();
  for entry in store.entries.iter().filter(|entry| audience_allows(entry, "master")) {
    if entry.topic.as_deref().is_some_and(|topic| residents.contains(topic)) {
      full.push(entry);
    } else {
      index.push(entry);
    }
  }
  if full.is_empty() && index.is_empty() {
    return Ok(None);
  }
  Ok(Some(selection_from_parts("master", None, full, index, None)))
}

/// The worker selection for `repo_basename`: the full text of worker entries whose topic is that repo, then the index
/// lines of the other worker entries, then the usage line (`memory.select_worker_memory`).
pub fn select_worker_memory(memory_dir: &Path, repo_basename: &str) -> anyhow::Result<MemorySelection> {
  let store = load_store(memory_dir)?;
  let mut full = Vec::new();
  let mut index = Vec::new();
  for entry in store.entries.iter().filter(|entry| audience_allows(entry, "worker")) {
    if entry.topic.as_deref() == Some(repo_basename) {
      full.push(entry);
    } else {
      index.push(entry);
    }
  }
  Ok(selection_from_parts("worker", Some(repo_basename.to_string()), full, index, Some(WORKER_USAGE_LINE)))
}

/// The selection's segments, full then index, each sorted by [`entry_order_key`] (`memory._selection_from_parts`).
fn selection_from_parts(
  audience: &'static str,
  repo_basename: Option<String>,
  mut full: Vec<&Entry>,
  mut index: Vec<&Entry>,
  usage_line: Option<&'static str>,
) -> MemorySelection {
  full.sort_by(|a, b| entry_order_key(a).cmp(&entry_order_key(b)));
  index.sort_by(|a, b| entry_order_key(a).cmp(&entry_order_key(b)));
  let mut segments = Vec::new();
  if !full.is_empty() {
    let texts: Vec<String> = full.iter().map(|entry| full_text(entry)).collect();
    segments.push(MemorySegment {
      delivery: "full",
      text: texts.join("\n\n"),
      sources: full
        .iter()
        .zip(texts)
        .map(|(entry, text)| MemoryEntrySource { source_ref: format!("memory:{}", entry.id()), delivery: "full", text })
        .collect(),
    });
  }
  if !index.is_empty() {
    let mut lines = vec![INDEX_HEADER.to_string()];
    lines.extend(index.iter().map(|entry| index_line(entry)));
    segments.push(MemorySegment {
      delivery: "index",
      text: lines.join("\n"),
      sources: index
        .iter()
        .map(|entry| MemoryEntrySource {
          source_ref: format!("memory:{}", entry.id()),
          delivery: "index",
          text: index_line(entry),
        })
        .collect(),
    });
  }
  if let Some(usage) = usage_line {
    match segments.last_mut() {
      // The usage line rides the index segment; a selection with no index entries gets a usage-only one.
      Some(last) if last.delivery == "index" => last.text = format!("{}\n\n{usage}", last.text),
      _ => segments.push(MemorySegment { delivery: "index", text: usage.to_string(), sources: Vec::new() }),
    }
  }
  let text = (!segments.is_empty()).then(|| {
    let texts: Vec<&str> = segments.iter().map(|segment| segment.text.as_str()).collect();
    texts.join("\n\n")
  });
  MemorySelection { audience, repo_basename, segments, usage_line, text }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Barrier};

  use super::*;

  const MASTER_ENTRY: &str =
    "---\nscope: user\ntopic: profile\naudience: master, worker\ntitle: Dark Mode\n---\nbody\n";

  /// Writes each `(relative path, text)` under `root`, creating the parent directories.
  fn write_files(root: &Path, files: &[(impl AsRef<str>, impl AsRef<str>)]) {
    for (relative, text) in files {
      let path = root.join(relative.as_ref());
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(path, text.as_ref()).unwrap();
    }
  }

  /// Every path under `root` (except `.git`) with its bytes, `None` for a directory.
  fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
      for child in fs::read_dir(&dir).unwrap() {
        let path = child.unwrap().path();
        let relative = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
        if relative == ".git" {
          continue;
        }
        if path.is_dir() {
          out.insert(relative, None);
          pending.push(path);
        } else {
          out.insert(relative, Some(fs::read(&path).unwrap()));
        }
      }
    }
    out
  }

  #[test]
  fn a_v2_entry_parses_into_its_header_and_body() {
    let entry = parse_entry_text(MASTER_ENTRY, Path::new("entries/profile/dark-mode.md")).unwrap();
    assert_eq!(entry.topic.as_deref(), Some("profile"));
    assert_eq!(entry.slug, "dark-mode");
    assert_eq!(entry.scope.as_deref(), Some("user"));
    assert_eq!(entry.audience, Some(vec!["master".to_string(), "worker".to_string()]));
    assert_eq!(entry.title, "Dark Mode");
    assert_eq!(entry.body, "body\n");
    assert_eq!(entry.id(), "profile/dark-mode");
  }

  #[test]
  fn the_body_may_hold_rule_lines_and_only_the_first_header_block_is_parsed() {
    let text = "---\nscope: user\ntopic: profile\naudience: worker\ntitle: T\n---\nfirst\n---\nscope: host\n";
    let entry = parse_entry_text(text, Path::new("entries/profile/x.md")).unwrap();
    assert_eq!(entry.scope.as_deref(), Some("user"));
    assert_eq!(entry.body, "first\n---\nscope: host\n");
  }

  #[test]
  fn a_title_loses_python_whitespace_and_a_carriage_return_is_read_as_a_newline() {
    let text = "---\nscope: user\ntopic: profile\naudience: worker\ntitle: \u{3000}Café 记忆\u{1c}\n---\nbody\n";
    let entry = parse_entry_text(text, Path::new("entries/profile/x.md")).unwrap();
    assert_eq!(entry.title, "Café 记忆");
  }

  #[test]
  fn a_malformed_header_line_is_quoted_as_python_repr() {
    let text = "---\nTitle: Note\n---\nbody\n";
    let error = parse_entry_text(text, Path::new("entries/profile/x.md")).unwrap_err();
    assert_eq!(error.0, "entries/profile/x.md: line 2: malformed header line: 'Title: Note'");
  }

  #[test]
  fn the_store_load_rejects_each_placement_with_the_violation_it_names() {
    let cases = [
      ("entries/nonexistent/x.md", entry_text("nonexistent", "master, worker"), "not in topics vocabulary"),
      (
        "entries/wrongdir/slug.md",
        entry_text("profile", "master, worker"),
        "directory name 'wrongdir' != topic 'profile'",
      ),
      ("entries/profile/bad slug.md", entry_text("profile", "master, worker"), "does not match slug charset"),
      (
        "entries/profile/rev.md",
        entry_text("profile", "master, worker").replacen("---\n", "---\nrevises: old-entry\n", 1),
        "unknown header field 'revises'",
      ),
      ("entries/profile/a.md", entry_text("profile", "master,all"), "audience element 'all' not in {master, worker}"),
    ];
    for (relative, text, fragment) in cases {
      let dir = tempfile::tempdir().unwrap();
      write_files(dir.path(), &[("topics", "profile resident\n")]);
      write_files(dir.path(), &[(relative, text)]);
      let error = load_store(dir.path()).unwrap_err();
      assert!(error.to_string().contains(fragment), "{error} does not name {fragment:?}");
    }
  }

  /// A v2 entry in `topic` with the given audience header value.
  fn entry_text(topic: &str, audience: &str) -> String {
    format!("---\nscope: user\ntopic: {topic}\naudience: {audience}\ntitle: Note\n---\nbody\n")
  }

  #[test]
  fn selecting_on_a_missing_dir_creates_the_scaffold_and_selects_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let memory_dir = dir.path().join("nope");
    assert!(select_master_memory(&memory_dir).unwrap().is_none());
    assert_fresh_scaffold(&memory_dir);
  }

  fn assert_fresh_scaffold(memory_dir: &Path) {
    for name in [".git", "topics", ".gitignore", "entries", "staging"] {
      assert!(memory_dir.join(name).exists(), "{name} is missing");
    }
    assert_eq!(fs::read_to_string(memory_dir.join("topics")).unwrap(), DEFAULT_MEMORY_TOPICS);
    assert_eq!(fs::read_to_string(memory_dir.join(".gitignore")).unwrap(), DEFAULT_MEMORY_GITIGNORE);
  }

  #[test]
  fn ensure_store_twice_keeps_a_curated_vocabulary() {
    let dir = tempfile::tempdir().unwrap();
    let memory_dir = dir.path().join("store");
    ensure_store(&memory_dir).unwrap();
    fs::write(memory_dir.join("topics"), "alpha resident\n").unwrap();
    let before = snapshot(&memory_dir);
    ensure_store(&memory_dir).unwrap();
    assert_eq!(snapshot(&memory_dir), before);
    assert_eq!(fs::read_to_string(memory_dir.join("topics")).unwrap(), "alpha resident\n");
  }

  #[test]
  fn concurrent_first_use_leaves_one_whole_scaffold() {
    let dir = tempfile::tempdir().unwrap();
    let memory_dir = Arc::new(dir.path().join("store"));
    let callers = 8;
    let start = Arc::new(Barrier::new(callers));
    let handles: Vec<_> = (0..callers)
      .map(|_| {
        let (memory_dir, start) = (Arc::clone(&memory_dir), Arc::clone(&start));
        std::thread::spawn(move || {
          start.wait();
          ensure_store(&memory_dir).unwrap();
        })
      })
      .collect();
    for handle in handles {
      handle.join().unwrap();
    }
    assert_fresh_scaffold(&memory_dir);
  }

  #[test]
  fn a_staged_capture_never_enters_a_selection() {
    let dir = tempfile::tempdir().unwrap();
    write_files(
      dir.path(),
      &[("staging/candidate.md", "---\nscope: user\ntopic: staged\ntitle: C\n---\nstaged body\n")],
    );
    write_files(dir.path(), &[("topics", "")]);
    assert!(select_master_memory(dir.path()).unwrap().is_none());
    let worker = select_worker_memory(dir.path(), "").unwrap();
    assert!(worker.segments.iter().all(|segment| segment.sources.is_empty()));
  }

  #[test]
  fn a_worker_selection_without_entries_is_the_usage_line_alone() {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), &[("topics", "profile resident\n")]);
    let worker = select_worker_memory(dir.path(), "profile").unwrap();
    assert_eq!(worker.segments.len(), 1);
    assert_eq!(worker.segments[0].delivery, "index");
    assert_eq!(worker.segments[0].text, WORKER_USAGE_LINE);
    assert_eq!(worker.text.as_deref(), Some(WORKER_USAGE_LINE));
  }

  #[test]
  fn residents_take_full_text_and_the_rest_of_the_master_audience_takes_index_lines() {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), &[("topics", "profile resident\nbeta\n")]);
    write_files(dir.path(), &[("entries/profile/dark-mode.md", MASTER_ENTRY)]);
    write_files(
      dir.path(),
      &[("entries/beta/worker-only.md", "---\nscope: user\ntopic: beta\naudience: worker\ntitle: W\n---\nworker\n")],
    );
    write_files(
      dir.path(),
      &[("entries/beta/master-note.md", "---\nscope: host\ntopic: beta\naudience: master\ntitle: M\n---\nmaster\n")],
    );
    let selection = select_master_memory(dir.path()).unwrap().unwrap();
    let deliveries: Vec<&str> = selection.segments.iter().map(|segment| segment.delivery).collect();
    assert_eq!(deliveries, ["full", "index"]);
    assert_eq!(selection.segments[0].sources[0].source_ref, "memory:profile/dark-mode");
    assert_eq!(selection.segments[0].text, "# Dark Mode\n\nbody");
    assert_eq!(selection.segments[1].sources[0].source_ref, "memory:beta/master-note");
    assert_eq!(selection.segments[1].sources[0].text, "beta/master-note · M");
  }

  #[test]
  fn a_worker_selection_puts_its_repo_topic_in_full_and_the_rest_in_the_index() {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), &[("topics", "profile resident\ngamma-service\n")]);
    write_files(dir.path(), &[("entries/profile/dark-mode.md", MASTER_ENTRY)]);
    write_files(
      dir.path(),
      &[(
        "entries/gamma-service/build.md",
        "---\nscope: user\ntopic: gamma-service\naudience: worker\ntitle: B\n---\nbuild\n",
      )],
    );
    let selection = select_worker_memory(dir.path(), "gamma-service").unwrap();
    assert_eq!(selection.segments[0].delivery, "full");
    assert_eq!(selection.segments[0].sources[0].source_ref, "memory:gamma-service/build");
    assert_eq!(selection.segments[1].delivery, "index");
    assert_eq!(selection.segments[1].sources[0].source_ref, "memory:profile/dark-mode");
    assert_eq!(selection.usage_line, Some(WORKER_USAGE_LINE));
  }
}

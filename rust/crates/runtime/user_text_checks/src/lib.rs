//! The shared checks for user-facing page and reply text, with their compiled pattern config.

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context as AnyhowContext, anyhow};
use charliebot_compat::file_urls::{self, FileLinkForm};
use charliebot_core::config::{ConfigSection, SectionValue, sessions_dir_in};
use charliebot_html::{EventKind, VOID_TAGS, parse_with};
use charliebot_registry::{Registry, RegistryError};
use fancy_regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};
use serde_yaml::Value as YamlValue;
use url::Url;

/// The internal-id check name shared by artifact assertions and web-reply refusals.
pub const TRACKER_ID_NEARBY: &str = "tracker-id-nearby";
/// The session-link check name shared by artifact assertions and web-reply refusals.
pub const SESSION_LINKED: &str = "session-linked";
/// The renderable-link check name shared by artifact assertions and web-reply refusals.
pub const LINK_RENDERABLE: &str = "link-renderable";
/// The commit-hash check name: an identifier must stand beside a readable name that says what it identifies.
pub const COMMIT_NAMED: &str = "commit-named";
/// The vocabulary check name: words must be the user's own or everyday words, with rule-defined agent terms kept out.
pub const VOCABULARY_RULE_TERM: &str = "vocabulary-rule-term";
/// The three checks, in the order each entry point reports them.
pub const CHECK_NAMES: [&str; 3] = [TRACKER_ID_NEARBY, SESSION_LINKED, LINK_RENDERABLE];

/// The key of the config section this package registers and its callers look up.
pub const CONFIG_SECTION: &str = "user_text_checks";
const TRACKER_FIX: &str = "write the Linear id right next to the internal id";
const TRACKER_PURPOSE: &str = "This lets the reader identify the work item.";
const SESSION_FIX: &str =
  "write the id as [<readable alias> · <first 8 characters>](<CharlieBot URL>/?session=<full id>)";
const SESSION_PURPOSE: &str = "This lets the reader open the correct session.";
const LINK_FIX: &str = "link an HTML page, or the directory that holds the file";
const LINK_PURPOSE: &str = "This gives the reader a rendered result.";
const COMMIT_FIX: &str = "name the commit by what it changes, with its hash in the link";
const COMMIT_PURPOSE: &str = "This lets the reader know what changed.";
const VOCABULARY_FIX: &str = "use everyday words instead of rule-defined agent terms";
const VOCABULARY_PURPOSE: &str = "This keeps agent jargon out of text for the user.";
const LINEAR_ID_PATTERN: &str = r"[A-Z]{2,}-\d+";
const SESSION_ID_PATTERN: &str = concat!(
  r"(?<![\p{L}\p{N}_])(?:",
  r"(?P<full>[A-Fa-f0-9]{8}-[A-Fa-f0-9]{4}-[A-Fa-f0-9]{4}-[A-Fa-f0-9]{4}-[A-Fa-f0-9]{12})",
  r"|(?P<short>[0-9a-f]{8}))",
  r"(?![\p{L}\p{N}_])",
);
const COMMIT_HASH_PATTERN: &str = r"(?<![\p{L}\p{N}_])(?P<hash>[0-9a-fA-F]{7,40})(?![\p{L}\p{N}_])";
const MARKDOWN_LINK_PATTERN: &str = concat!(
  r"\[(?P<label>[^\]\n]*)\]\([ \t]*(?:",
  r"<(?P<angle>[^>\n]*)>|(?P<bare>[^)\s]+)",
  r")(?:[ \t]+[^)\n]*)?[ \t]*\)",
);

static LINEAR_ID_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(LINEAR_ID_PATTERN).expect("the Linear id pattern is valid"));
static SESSION_ID_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(SESSION_ID_PATTERN).expect("the session id pattern is valid"));
static COMMIT_HASH_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(COMMIT_HASH_PATTERN).expect("the commit hash pattern is valid"));
static MARKDOWN_LINK_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(MARKDOWN_LINK_PATTERN).expect("the Markdown link pattern is valid"));

/// Rule-defined agent terms that must stay out of text for the user (Vocabulary).
pub const RULE_DEFINED_TERMS: &[&str] = &[
  "读回",
  "主线",
  "子线",
  "本线",
  "这条线",
  "上级会话",
  "父节点",
  "子会话",
  "子节点",
  "冷读",
  "分歧",
  "取舍",
  "接手",
  "起飞",
  "Waiting on you",
  "审批人",
  "放行",
];

/// A page link's route after it passes `check_page_link`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageLinkTarget {
  NoLink,
  ApplicationRoute,
  External,
  HostFile(PathBuf),
}

/// The route a status, reply or rendered page can open as a browser page. An error carries the refusal reason.
pub fn check_page_link(link: Option<&str>) -> Result<PageLinkTarget, String> {
  let Some(link) = link else {
    return Ok(PageLinkTarget::NoLink);
  };
  if is_application_route(link) {
    return Ok(PageLinkTarget::ApplicationRoute);
  }

  let file_link =
    file_urls::scan_file_link_target(link, None).map_err(|error| format!("link {link:?} cannot be parsed: {error}"))?;
  if file_link.as_ref().is_some_and(|file| file.form == FileLinkForm::RetiredPrefix) {
    return Err(format!("link {link:?} uses a retired file-server prefix"));
  }

  let url = Url::parse(link).map_err(|error| format!("link {link:?} is not a valid absolute URL: {error}"))?;
  if !matches!(url.scheme(), "http" | "https") {
    return Err(format!("link {link:?} uses unsupported scheme {:?}; only http and https URLs render", url.scheme()));
  }

  let Some(path) =
    file_urls::host_file(Some(link)).map_err(|error| format!("link {link:?} cannot be parsed: {error}"))?
  else {
    return Ok(PageLinkTarget::External);
  };
  let metadata =
    fs::metadata(&path).map_err(|error| format!("mapped link {link:?} cannot read {}: {error}", path.display()))?;
  if ["md", "diff", "patch"]
    .iter()
    .any(|suffix| path.extension().is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case(suffix)))
  {
    return Err(format!("mapped link {link:?} targets a non-page file at {}", path.display()));
  }
  if metadata.is_file() {
    fs::File::open(&path).map_err(|error| format!("mapped link {link:?} cannot read {}: {error}", path.display()))?;
  } else if metadata.is_dir() {
    for entry in
      fs::read_dir(&path).map_err(|error| format!("mapped link {link:?} cannot read {}: {error}", path.display()))?
    {
      entry.map_err(|error| format!("mapped link {link:?} cannot read {}: {error}", path.display()))?;
    }
  } else {
    return Err(format!("mapped link {link:?} does not name a rendered file or directory at {}", path.display()));
  }
  Ok(PageLinkTarget::HostFile(path))
}

/// Whether a local link names one of the application routes that does not map to a host file.
fn is_application_route(link: &str) -> bool {
  if link.starts_with("/?") {
    return true;
  }
  let path = link.split(['?', '#']).next().expect("split always yields the full string");
  path == "/diff" || path == "/api" || path.starts_with("/diff/") || path.starts_with("/api/")
}

const PAGE_BLOCK_TAGS: &[&str] =
  &["p", "li", "tr", "dd", "dt", "h1", "h2", "h3", "h4", "blockquote", "figcaption", "summary", "pre"];

#[derive(Clone)]
struct InternalIdPattern {
  source: String,
  compiled: Regex,
}

/// The compiled user_text_checks.internal_id_patterns config section.
#[derive(Clone, Default)]
pub struct UserTextChecksConfig {
  patterns: Vec<InternalIdPattern>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUserTextChecksConfig {
  #[serde(default)]
  internal_id_patterns: Vec<String>,
}

/// One user-text violation and the fixed correction shown at the writing entry point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextViolation {
  pub check: &'static str,
  pub snippet: String,
  pub block: Option<String>,
  fix: &'static str,
  purpose: &'static str,
}

/// The three checks and the session directory names read once for one input batch.
#[derive(Clone)]
pub struct UserTextChecks {
  config: UserTextChecksConfig,
  sessions: SessionIndex,
}

#[derive(Clone)]
struct SessionIndex {
  names: Vec<String>,
}

struct PageScan {
  blocks: Vec<String>,
  text: Vec<PageText>,
  code: Vec<PageCode>,
  links: Vec<PageLink>,
}

struct PageText {
  text: String,
  block: Option<usize>,
  anchor_href: Option<String>,
  in_pre: bool,
}

struct PageCode {
  text: String,
  block: Option<usize>,
  anchor_href: Option<String>,
  in_pre: bool,
}

struct PageLink {
  href: String,
  block: Option<usize>,
  label: String,
}

struct ElementState {
  tag: String,
  block: Option<usize>,
  anchor: Option<usize>,
  code: Option<usize>,
  in_pre: bool,
  hidden: bool,
}

struct MarkdownLink {
  label: Range<usize>,
  target: Range<usize>,
  target_text: String,
}

struct InlineCode {
  content: Range<usize>,
}

/// The `user_text_checks` section's registration, which makes the loader accept and compile its patterns.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(config_section())
}

/// The config section definition owned by this package.
pub fn config_section() -> ConfigSection {
  ConfigSection { key: CONFIG_SECTION, legacy_keys: &[], parse: parse_config, dump: dump_config }
}

fn parse_config(value: &YamlValue) -> Result<SectionValue, String> {
  let raw: RawUserTextChecksConfig =
    serde_yaml::from_value(value.clone()).map_err(|error| format!("user_text_checks: {error}"))?;
  let patterns = raw
    .internal_id_patterns
    .into_iter()
    .map(|source| {
      Regex::new(&source)
        .map(|compiled| InternalIdPattern { source: source.clone(), compiled })
        .map_err(|error| format!("internal_id_patterns pattern {source:?} is invalid: {error}"))
    })
    .collect::<Result<Vec<_>, _>>()?;
  Ok(std::sync::Arc::new(UserTextChecksConfig { patterns }))
}

fn dump_config(value: &SectionValue) -> Result<Value, String> {
  let config = value.downcast_ref::<UserTextChecksConfig>().ok_or("user_text_checks holds an unexpected value")?;
  let patterns = config.patterns.iter().map(|pattern| Value::String(pattern.source.clone())).collect();
  let mut object = Map::new();
  object.insert("internal_id_patterns".to_string(), Value::Array(patterns));
  Ok(Value::Object(object))
}

impl UserTextChecks {
  /// The checks for `home`: the configured patterns, and the session directory names that resolve session ids.
  pub fn new(config: &UserTextChecksConfig, home: &Path) -> anyhow::Result<UserTextChecks> {
    let sessions_path = sessions_dir_in(home);
    let mut names = Vec::new();
    match fs::read_dir(&sessions_path) {
      Ok(entries) => {
        for entry in entries {
          let entry = entry.with_context(|| format!("could not read an entry in {}", sessions_path.display()))?;
          if entry.file_type().with_context(|| format!("could not inspect {}", entry.path().display()))?.is_dir() {
            names.push(
              entry
                .file_name()
                .into_string()
                .map_err(|name| anyhow!("session directory name is not Unicode: {name:?}"))?,
            );
          }
        }
      }
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
      Err(error) => {
        return Err(error)
          .with_context(|| format!("could not read session directories at {}", sessions_path.display()));
      }
    }
    names.sort();
    Ok(UserTextChecks { config: config.clone(), sessions: SessionIndex { names } })
  }

  /// The violations in one web reply, found in its one-line blocks and at its link targets.
  pub fn check_reply(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let mut violations = self.tracker_violations(line_blocks(text))?;
    violations.extend(self.reply_session_violations(text)?);
    violations.extend(self.reply_link_violations(text)?);
    violations.extend(self.reply_commit_violations(text)?);
    violations.extend(self.reply_vocabulary_violations(text)?);
    Ok(violations)
  }

  /// The violations in one rendered HTML page, found in its configured text blocks and at its anchor targets.
  pub fn check_page(&self, html: &str) -> anyhow::Result<Vec<TextViolation>> {
    let page = parse_page(html)?;
    let blocks = page.blocks.iter().enumerate().map(|(index, text)| (index, text.as_str())).collect::<Vec<_>>();
    let mut violations = self.tracker_violations(blocks)?;
    violations.extend(self.page_session_violations(&page)?);
    violations.extend(self.page_link_violations(&page)?);
    Ok(violations)
  }

  fn tracker_violations<'a>(
    &self,
    blocks: impl IntoIterator<Item = (usize, &'a str)>,
  ) -> anyhow::Result<Vec<TextViolation>> {
    let mut violations = Vec::new();
    for (_, block) in blocks {
      let mut linear = Vec::new();
      for found in LINEAR_ID_RE.find_iter(block) {
        linear.push(found?.range());
      }
      let internal = self.internal_matches(block)?;
      for (range, snippet) in internal {
        if !linear.iter().any(|linear| within_linear_window(block, &range, linear)) {
          violations.push(TextViolation::new(TRACKER_ID_NEARBY, snippet, Some(display_block(block))));
        }
      }
    }
    Ok(violations)
  }

  fn internal_matches(&self, text: &str) -> anyhow::Result<Vec<(Range<usize>, String)>> {
    let mut matches = BTreeSet::new();
    for pattern in &self.config.patterns {
      for found in pattern.compiled.find_iter(text) {
        let found = found?;
        matches.insert((found.start(), found.end()));
      }
    }
    Ok(matches.into_iter().map(|(start, end)| ((start..end), text[start..end].to_string())).collect())
  }

  fn reply_session_violations(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let links = markdown_links(text)?;
    let fenced = fenced_code_ranges(text);
    let inline_code = inline_code_spans(text, &fenced);
    let mut violations = Vec::new();
    for matched in self.session_matches(text)? {
      if is_inside(&fenced, &matched.range)
        || is_url_or_path(text, &matched.range, &links)
        || inline_code.iter().any(|code| {
          code.content.start <= matched.range.start
            && matched.range.end <= code.content.end
            && text[code.content.clone()].trim() != matched.snippet
        })
      {
        continue;
      }
      if links.iter().any(|link| {
        link.label.start <= matched.range.start
          && matched.range.end <= link.label.end
          && session_target_matches(&link.target_text, &matched.full_id)
      }) {
        continue;
      }
      violations.push(TextViolation::new(SESSION_LINKED, matched.snippet, None));
    }
    Ok(violations)
  }

  fn page_session_violations(&self, page: &PageScan) -> anyhow::Result<Vec<TextViolation>> {
    let mut violations = Vec::new();
    for text in &page.text {
      if text.in_pre {
        continue;
      }
      for matched in self.session_matches(&text.text)? {
        if is_url_or_path(&text.text, &matched.range, &[]) {
          continue;
        }
        if text.anchor_href.as_deref().is_some_and(|href| session_target_matches(href, &matched.full_id)) {
          continue;
        }
        violations.push(TextViolation::new(
          SESSION_LINKED,
          matched.snippet,
          violation_block(page, text.block, &text.text),
        ));
      }
    }
    for code in &page.code {
      if code.in_pre || code.text.trim().is_empty() {
        continue;
      }
      for matched in self.session_matches(&code.text)? {
        if code.text.trim() != matched.snippet || is_url_or_path(&code.text, &matched.range, &[]) {
          continue;
        }
        if code.anchor_href.as_deref().is_some_and(|href| session_target_matches(href, &matched.full_id)) {
          continue;
        }
        violations.push(TextViolation::new(
          SESSION_LINKED,
          matched.snippet,
          violation_block(page, code.block, &code.text),
        ));
      }
    }
    Ok(violations)
  }

  fn reply_link_violations(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let targets = file_urls::scan_link_targets(text);
    let fenced = fenced_code_ranges(text);
    let inline_code = inline_code_spans(text, &fenced);
    let mut violations = Vec::new();
    for target in targets {
      if is_inside(&fenced, &target.span)
        || inline_code.iter().any(|code| {
          let closing_delimiters = text.as_bytes()[code.content.end..].iter().take_while(|byte| **byte == b'`').count();
          code.content.start <= target.span.start && target.span.end <= code.content.end + closing_delimiters
        })
      {
        continue;
      }
      if check_page_link(Some(&target.target)).is_err() {
        violations.push(TextViolation::new(
          LINK_RENDERABLE,
          target.target,
          Some(display_block(line_for_span(text, &target.span))),
        ));
      }
    }
    Ok(violations)
  }

  fn page_link_violations(&self, page: &PageScan) -> anyhow::Result<Vec<TextViolation>> {
    let mut violations = Vec::new();
    for link in &page.links {
      if check_page_link(Some(&link.href)).is_err() {
        violations.push(TextViolation::new(
          LINK_RENDERABLE,
          link.href.clone(),
          violation_block(page, link.block, &link.label),
        ));
      }
    }
    Ok(violations)
  }

  fn session_matches(&self, text: &str) -> anyhow::Result<Vec<SessionMatch>> {
    let mut matches = Vec::new();
    for captures in SESSION_ID_RE.captures_iter(text) {
      let captures = captures?;
      let id = captures.name("full").or_else(|| captures.name("short")).expect("the pattern captures one id");
      let Some(full_id) = self.sessions.resolve(id.as_str(), captures.name("full").is_some()) else {
        continue;
      };
      matches.push(SessionMatch { range: id.range(), snippet: id.as_str().to_string(), full_id: full_id.to_string() });
    }
    Ok(matches)
  }

  fn reply_commit_violations(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let links = markdown_links(text)?;
    let fenced = fenced_code_ranges(text);
    let inline_code = inline_code_spans(text, &fenced);
    let session_matches = self.session_matches(text)?;
    let mut violations = Vec::new();
    for captures in COMMIT_HASH_RE.captures_iter(text) {
      let captures = captures?;
      let matched = captures.name("hash").expect("the pattern captures hash");
      let s = matched.as_str();
      if !s.chars().any(|c| c.is_ascii_digit()) || !s.chars().any(|c| c.is_ascii_alphabetic()) {
        continue;
      }
      let range = matched.range();
      let before = text[..range.start].chars().next_back();
      let after = text[range.end..].chars().next();
      if before == Some('-') || after == Some('-') {
        continue;
      }
      if session_matches.iter().any(|sm| sm.range == range) {
        continue;
      }
      if is_inside(&fenced, &range) || is_url_or_path(text, &range, &links) {
        continue;
      }
      if inline_code.iter().any(|code| {
        code.content.start <= range.start && range.end <= code.content.end && text[code.content.clone()].trim() != s
      }) {
        continue;
      }
      if let Some(link) = links.iter().find(|link| link.label.start <= range.start && range.end <= link.label.end) {
        let label = &text[link.label.clone()];
        let label_stripped = label.trim().trim_matches('`').trim();
        if label_stripped != s && label.chars().any(|c| c.is_alphabetic() && !c.is_ascii_hexdigit()) {
          continue;
        }
      }
      violations.push(TextViolation::new(COMMIT_NAMED, s.to_string(), None));
    }
    Ok(violations)
  }

  fn reply_vocabulary_violations(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let fenced = fenced_code_ranges(text);
    let mut violations = Vec::new();
    for &term in RULE_DEFINED_TERMS {
      let mut cursor = 0;
      while let Some(index) = text[cursor..].find(term) {
        let start = cursor + index;
        let end = start + term.len();
        cursor = end;
        let range = start..end;
        if is_inside(&fenced, &range) {
          continue;
        }
        violations.push(TextViolation::new(VOCABULARY_RULE_TERM, term.to_string(), None));
      }
    }
    Ok(violations)
  }
}

#[derive(Clone)]
struct SessionMatch {
  range: Range<usize>,
  snippet: String,
  full_id: String,
}

impl SessionIndex {
  fn resolve(&self, candidate: &str, full_uuid: bool) -> Option<&str> {
    if full_uuid {
      return self.names.iter().find(|name| name.as_str() == candidate).map(String::as_str);
    }
    if let Some(exact) = self.names.iter().find(|name| name.as_str() == candidate) {
      return Some(exact);
    }
    let mut matches = self.names.iter().filter(|name| name.starts_with(candidate));
    let only = matches.next()?;
    matches.next().is_none().then_some(only)
  }
}

impl TextViolation {
  fn new(check: &'static str, snippet: String, block: Option<String>) -> TextViolation {
    let (fix, purpose) = match check {
      TRACKER_ID_NEARBY => (TRACKER_FIX, TRACKER_PURPOSE),
      SESSION_LINKED => (SESSION_FIX, SESSION_PURPOSE),
      LINK_RENDERABLE => (LINK_FIX, LINK_PURPOSE),
      COMMIT_NAMED => (COMMIT_FIX, COMMIT_PURPOSE),
      VOCABULARY_RULE_TERM => (VOCABULARY_FIX, VOCABULARY_PURPOSE),
      other => unreachable!("unknown user-text check {other:?}"),
    };
    TextViolation { check, snippet, block, fix, purpose }
  }
}

/// One page failure as a line of text, with its snippet, containing block, fix and reason.
pub fn page_failure_detail(violation: &TextViolation) -> String {
  format!(
    "snippet {:?}; block {:?}; fix: {}. {}",
    violation.snippet,
    violation.block.as_deref().unwrap_or(""),
    violation.fix,
    violation.purpose
  )
}

/// Web-reply refusals as one line per hit, with each correction's purpose.
pub fn reply_refusal_detail(violations: &[TextViolation]) -> String {
  violations
    .iter()
    .map(|violation| {
      format!("{}: snippet {:?}; fix: {}. {}", violation.check, violation.snippet, violation.fix, violation.purpose)
    })
    .collect::<Vec<_>>()
    .join("\n")
}

fn within_linear_window(block: &str, internal: &Range<usize>, linear: &Range<usize>) -> bool {
  let gap = if linear.end <= internal.start {
    block[linear.end..internal.start].chars().count()
  } else if internal.end <= linear.start {
    block[internal.end..linear.start].chars().count()
  } else {
    0
  };
  gap <= 40
}

fn line_blocks(text: &str) -> Vec<(usize, &str)> {
  let mut offset = 0;
  text
    .split_inclusive('\n')
    .map(|line| {
      let without_newline = line.strip_suffix('\n').unwrap_or(line);
      let content = without_newline.strip_suffix('\r').unwrap_or(without_newline);
      let block = (offset, content);
      offset += line.len();
      block
    })
    .collect()
}

fn line_for_span<'a>(text: &'a str, span: &Range<usize>) -> &'a str {
  let start = text[..span.start].rfind('\n').map_or(0, |index| index + '\n'.len_utf8());
  let end = text[span.end..].find('\n').map_or(text.len(), |index| span.end + index);
  &text[start..end]
}

fn display_block(block: &str) -> String {
  block.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The violation's reported block: the text of the block that holds it, or the display form of `fallback`.
fn violation_block(page: &PageScan, block: Option<usize>, fallback: &str) -> Option<String> {
  block.map(|index| display_block(&page.blocks[index])).or_else(|| Some(display_block(fallback)))
}

fn markdown_links(text: &str) -> anyhow::Result<Vec<MarkdownLink>> {
  let mut links = Vec::new();
  for captures in MARKDOWN_LINK_RE.captures_iter(text) {
    let captures = captures?;
    let label = captures.name("label").expect("the pattern captures the link label");
    let target =
      captures.name("angle").or_else(|| captures.name("bare")).expect("the pattern captures the link target");
    links.push(MarkdownLink { label: label.range(), target: target.range(), target_text: target.as_str().to_string() });
  }
  Ok(links)
}

fn session_target_matches(target: &str, full_id: &str) -> bool {
  let target = target.split_once('#').map_or(target, |(before, _)| before);
  let Some((before_query, query)) = target.split_once('?') else {
    return false;
  };
  link_path(before_query) == "/"
    && query
      .split('&')
      .any(|parameter| parameter.split_once('=').is_some_and(|(name, value)| name == "session" && value == full_id))
}

fn is_url_or_path(text: &str, span: &Range<usize>, links: &[MarkdownLink]) -> bool {
  if file_urls::url_regex().find_iter(text).any(|url| url.start() <= span.start && span.end <= url.end())
    || links.iter().any(|link| link.target.start <= span.start && span.end <= link.target.end)
  {
    return true;
  }
  let previous = text[..span.start].chars().next_back();
  let next = text[span.end..].chars().next();
  if previous.is_some_and(|character| matches!(character, '/' | '\\'))
    || next.is_some_and(|character| matches!(character, '/' | '\\'))
  {
    return true;
  }
  // The matched whitespace can be multi-byte, such as U+3000, so the token starts after all of its bytes.
  let start = text[..span.start]
    .char_indices()
    .rev()
    .find(|(_, character)| character.is_whitespace())
    .map_or(0, |(index, character)| index + character.len_utf8());
  let end = text[span.end..].find(char::is_whitespace).map_or(text.len(), |index| span.end + index);
  let token = &text[start..end];
  let uri = token.trim_start_matches(['(', '[', '{', '<']);
  if uri.split_once(':').is_some_and(|(scheme, _)| {
    scheme.chars().next().is_some_and(|first| first.is_ascii_alphabetic())
      && scheme.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '.' | '-'))
  }) {
    return true;
  }
  token.starts_with('/') && token.contains('?')
}

fn is_inside(ranges: &[Range<usize>], span: &Range<usize>) -> bool {
  ranges.iter().any(|range| range.start <= span.start && span.end <= range.end)
}

fn fenced_code_ranges(text: &str) -> Vec<Range<usize>> {
  let mut ranges = Vec::new();
  let mut fence: Option<(u8, usize, usize)> = None;
  let mut offset = 0;
  for line in text.split_inclusive('\n') {
    let without_newline = line.strip_suffix('\n').unwrap_or(line);
    let content = without_newline.strip_suffix('\r').unwrap_or(without_newline);
    let indent_len = content.bytes().take_while(|byte| *byte == b' ').count();
    let rest = &content[indent_len..];
    let marker = rest.as_bytes().first().copied();
    let marker_count = marker.map_or(0, |marker| rest.bytes().take_while(|byte| *byte == marker).count());
    if let Some((active, minimum, start)) = fence {
      if indent_len <= 3 && marker == Some(active) && marker_count >= minimum && rest[marker_count..].trim().is_empty()
      {
        ranges.push(start..offset + line.len());
        fence = None;
      }
    } else if indent_len <= 3 && marker.is_some_and(|byte| matches!(byte, 0x60 | b'~')) && marker_count >= 3 {
      fence = Some((marker.expect("a fence has a marker"), marker_count, offset));
    }
    offset += line.len();
  }
  if let Some((_, _, start)) = fence {
    ranges.push(start..text.len());
  }
  ranges
}

fn inline_code_spans(text: &str, fenced: &[Range<usize>]) -> Vec<InlineCode> {
  let bytes = text.as_bytes();
  let mut spans = Vec::new();
  let mut cursor = 0;
  while cursor < bytes.len() {
    if let Some(fence) = fenced.iter().find(|fence| fence.start <= cursor && cursor < fence.end) {
      cursor = fence.end;
      continue;
    }
    if bytes[cursor] != 0x60 {
      cursor += 1;
      continue;
    }
    let start = cursor;
    while cursor < bytes.len() && bytes[cursor] == 0x60 {
      cursor += 1;
    }
    let count = cursor - start;
    if escaped_backtick(bytes, start) {
      continue;
    }
    if let Some(close) = matching_backtick_run(bytes, cursor, count, fenced) {
      spans.push(InlineCode { content: cursor..close });
      cursor = close + count;
    }
  }
  spans
}

fn escaped_backtick(bytes: &[u8], start: usize) -> bool {
  let mut slashes = 0;
  let mut cursor = start;
  while cursor > 0 && bytes[cursor - 1] == b'\\' {
    slashes += 1;
    cursor -= 1;
  }
  slashes % 2 == 1
}

fn matching_backtick_run(bytes: &[u8], mut cursor: usize, count: usize, fenced: &[Range<usize>]) -> Option<usize> {
  while cursor < bytes.len() {
    if let Some(fence) = fenced.iter().find(|fence| fence.start <= cursor && cursor < fence.end) {
      cursor = fence.end;
      continue;
    }
    if bytes[cursor] != 0x60 {
      cursor += 1;
      continue;
    }
    let start = cursor;
    while cursor < bytes.len() && bytes[cursor] == 0x60 {
      cursor += 1;
    }
    if cursor - start == count {
      return Some(start);
    }
  }
  None
}

/// The path part of a link: its text before any query or fragment, with the scheme and authority removed.
fn link_path(link: &str) -> &str {
  let no_fragment = link.split_once('#').map_or(link, |(before, _)| before);
  let no_query = no_fragment.split_once('?').map_or(no_fragment, |(before, _)| before);
  if let Some((_, after_scheme)) = no_query.split_once("://") {
    after_scheme.find('/').map_or("", |slash| &after_scheme[slash..])
  } else if let Some(authority) = no_query.strip_prefix("//") {
    authority.find('/').map_or("", |slash| &authority[slash..])
  } else {
    no_query
  }
}

fn parse_page(html: &str) -> anyhow::Result<PageScan> {
  let mut page = PageScan { blocks: Vec::new(), text: Vec::new(), code: Vec::new(), links: Vec::new() };
  let mut stack = vec![ElementState {
    tag: "#root".to_string(),
    block: None,
    anchor: None,
    code: None,
    in_pre: false,
    hidden: false,
  }];
  parse_with(html, true, |event| match event.kind {
    EventKind::StartTag | EventKind::StartEndTag => {
      let arguments = event.args.as_array().expect("a start-tag event has arguments");
      let tag = arguments[0].as_str().expect("a start-tag event has a tag").to_string();
      let parent = stack.last().expect("the root remains on the parser stack");
      let block = if PAGE_BLOCK_TAGS.contains(&tag.as_str()) {
        let index = page.blocks.len();
        page.blocks.push(String::new());
        Some(index)
      } else {
        parent.block
      };
      let attributes = arguments[1].as_array().expect("a start-tag event has attributes");
      let href = attributes.iter().find_map(|attribute| {
        let pair = attribute.as_array().expect("an attribute has a pair");
        (pair[0].as_str() == Some("href")).then(|| pair[1].as_str().map(str::to_string)).flatten()
      });
      let anchor = if tag == "a" {
        if let Some(href) = href {
          let index = page.links.len();
          page.links.push(PageLink { href, block, label: String::new() });
          Some(index)
        } else {
          parent.anchor
        }
      } else {
        parent.anchor
      };
      let code = if tag == "code" {
        let index = page.code.len();
        page.code.push(PageCode {
          text: String::new(),
          block,
          anchor_href: parent.anchor.map(|anchor| page.links[anchor].href.clone()),
          in_pre: parent.in_pre || tag == "pre",
        });
        Some(index)
      } else {
        parent.code
      };
      let in_pre = parent.in_pre || tag == "pre";
      let hidden = parent.hidden || matches!(tag.as_str(), "script" | "style");
      if event.kind == EventKind::StartTag && !VOID_TAGS.contains(&tag.as_str()) {
        stack.push(ElementState { tag, block, anchor, code, in_pre, hidden });
      }
    }
    EventKind::EndTag => {
      let tag = event.args[0].as_str().expect("an end-tag event has a tag");
      if let Some(position) = (1..stack.len()).rev().find(|&position| stack[position].tag == tag) {
        stack.truncate(position);
      }
    }
    EventKind::Data => {
      let text = event.args[0].as_str().expect("a data event has text");
      let current = stack.last().expect("the root remains on the parser stack");
      if current.hidden {
        return;
      }
      if let Some(block) = current.block {
        page.blocks[block].push_str(text);
      }
      if let Some(anchor) = current.anchor {
        page.links[anchor].label.push_str(text);
      }
      if let Some(code) = current.code {
        page.code[code].text.push_str(text);
      } else {
        page.text.push(PageText {
          text: text.to_string(),
          block: current.block,
          anchor_href: current.anchor.map(|anchor| page.links[anchor].href.clone()),
          in_pre: current.in_pre,
        });
      }
    }
    EventKind::EntityReference
    | EventKind::CharacterReference
    | EventKind::Comment
    | EventKind::Declaration
    | EventKind::ProcessingInstruction
    | EventKind::UnknownDeclaration => {}
  })
  .map_err(|error| anyhow!("{}: {}", error.exception, error.message))?;
  Ok(page)
}

#[cfg(test)]
mod tests;

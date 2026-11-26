//! The cold-read run's shared contract: the prompt texts, the probe input, and the report body.
//!
//! `prompts/cold_read.md` is the only home of the cold-reader instructions and questions. The artifact check
//! and the cold-read child session both build from here, so the foreground output and the completion report carry the
//! same lines.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use anyhow::bail;
use charliebot_agents::backend_pool;
use charliebot_agents::cli_host::CliHost;
use charliebot_agents::{backend_types, one_shot_text};
use charliebot_core::config::CallClass;
use charliebot_core::text_repr::{self, JsonStyle};
use charliebot_html::{BLOCK_TAGS, EventKind, folds_at_open, parse_with};
use charliebot_registry::backend::LaunchArgs;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::spawner_prompt;

/// The cold-reader template under `prompts/`.
pub const PROMPT_FILENAME: &str = "cold_read.md";
const SYSTEM_SECTION: &str = "system_prompt";
const QUESTIONS_SECTION: &str = "questions";
const ASK_SYSTEM_SECTION: &str = "ask_system_prompt";
const ASK_QUESTIONS_SECTION: &str = "ask_questions";
const REPLY_SYSTEM_SECTION: &str = "reply_system_prompt";
const REPLY_QUESTIONS_SECTION: &str = "reply_questions";
/// The Now-status prompt pair under `prompts/cold_read.md`.
pub const NOW_SYSTEM_SECTION: &str = "now_system_prompt";
pub const NOW_QUESTIONS_SECTION: &str = "now_questions";
/// The settle prompt pair under `prompts/cold_read.md`.
pub const SETTLE_SYSTEM_SECTION: &str = "settle_system_prompt";
pub const SETTLE_QUESTIONS_SECTION: &str = "settle_questions";
/// The question counts of the Now-status and web-reply prompt pairs.
pub const NOW_QUESTION_COUNT: usize = 1;
pub const REPLY_QUESTION_COUNT: usize = 3;
/// The bounded timeout for one cold read probe attempt.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The name of each registered cold read. A `cold_read_check` event carries its check's name.
pub const PAGE: &str = "page";
pub const REPLY: &str = "reply";
pub const ASK: &str = "ask";
pub const SETTLE: &str = "settle";
pub const NOW: &str = "now";

/// What a caller does with a cold read's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalRule {
  /// The answer is a report for the caller: nothing is recorded and nothing is refused.
  None,
  /// Findings hold the first submission; a second submission posts without a server-side refusal.
  HoldOnFindings,
  /// The answer is recorded and the command refuses to store until the record exists.
  RequireRecord,
  /// The answer is recorded for each kept item and the store is refused until a record follows the turn's last input.
  RequireRecordSinceInput,
  /// Findings refuse the command; nothing is recorded.
  RefuseOnFindings,
}

/// One row of the cold-read registry: the check's name, its prompt pair under `prompts/cold_read.md`, and what a
/// caller does with its answer.
pub struct ColdReadRow {
  pub name: &'static str,
  pub system_section: &'static str,
  pub questions_section: &'static str,
  pub refusal: RefusalRule,
}

/// The registry: every command-side cold read, one row each.
pub const REGISTRY: [ColdReadRow; 5] = [
  ColdReadRow {
    name: PAGE,
    system_section: SYSTEM_SECTION,
    questions_section: QUESTIONS_SECTION,
    refusal: RefusalRule::None,
  },
  ColdReadRow {
    name: REPLY,
    system_section: REPLY_SYSTEM_SECTION,
    questions_section: REPLY_QUESTIONS_SECTION,
    refusal: RefusalRule::HoldOnFindings,
  },
  ColdReadRow {
    name: ASK,
    system_section: ASK_SYSTEM_SECTION,
    questions_section: ASK_QUESTIONS_SECTION,
    refusal: RefusalRule::RequireRecord,
  },
  ColdReadRow {
    name: SETTLE,
    system_section: SETTLE_SYSTEM_SECTION,
    questions_section: SETTLE_QUESTIONS_SECTION,
    refusal: RefusalRule::RequireRecordSinceInput,
  },
  ColdReadRow {
    name: NOW,
    system_section: NOW_SYSTEM_SECTION,
    questions_section: NOW_QUESTIONS_SECTION,
    refusal: RefusalRule::RefuseOnFindings,
  },
];

/// The registry row named `name`, or `None` for a name no row carries.
pub fn row(name: &str) -> Option<&'static ColdReadRow> {
  REGISTRY.iter().find(|row| row.name == name)
}

impl ColdReadRow {
  /// The system prompt and questions of this row, read fresh from `prompts/cold_read.md`.
  pub fn prompt_pair(&self, repo: &Path) -> anyhow::Result<(String, String)> {
    load_prompt_pair(repo, self.system_section, self.questions_section)
  }
}

/// The names a `cold_read_check` event may carry: the registry's recorded rows, in registry order.
pub const RECORDED_NAMES: &[&str] = &[REPLY, ASK, SETTLE];
/// The item JSON an ask probe carries: non-ASCII characters unescaped, two-space indentation.
const ITEM_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ": ", indent: Some(2) };
/// The placeholder in the questions that the trigger message replaces.
pub const TRIGGER_PLACEHOLDER: &str = "<trigger message verbatim>";

/// The run-directory file one cold-read run writes at launch: the page path, the page's sha256, and the error its
/// failure judgment recorded. The completion report reads each attempt back from it.
pub const EVIDENCE_FILENAME: &str = "cold_read.json";

/// The outcome the report states: the model that answered, or the trailer for every model failing.
pub enum ColdReadOutcome<'a> {
  Answered { backend_id: &'a str, answer: &'a str },
  AllFailed,
}

/// The SHA-256 digest of the page bytes a cold read read, as lowercase hexadecimal (`hashlib.sha256(...).hexdigest()`).
pub fn page_sha256(page_bytes: &[u8]) -> String {
  format!("{:x}", Sha256::digest(page_bytes))
}

/// The system prompt and questions named by one pair of sections, read fresh from `prompts/cold_read.md`. Trailing
/// newlines stay out of both, so the probe input is byte-identical to the file's section text.
pub fn load_prompt_pair(
  repo: &Path,
  system_section: &str,
  questions_section: &str,
) -> anyhow::Result<(String, String)> {
  let sections = spawner_prompt::load_marker_sections(
    &repo.join("prompts").join(PROMPT_FILENAME),
    &[system_section, questions_section],
    "cold-read",
  )?;
  Ok((
    sections[system_section].trim_end_matches('\n').to_string(),
    sections[questions_section].trim_end_matches('\n').to_string(),
  ))
}

/// The system prompt and questions for the artifact-page check, read fresh from `prompts/cold_read.md`.
pub fn load_texts(repo: &Path) -> anyhow::Result<(String, String)> {
  let row = row(PAGE).expect("the page row is registered");
  load_prompt_pair(repo, row.system_section, row.questions_section)
}

/// The block a probe adds after the full source: the text the reader sees at page open, folded blocks closed.
pub const VISIBLE_TEXT_HEADER: &str = "VISIBLE TEXT";

/// The non-block elements that also end a line of visible text: a line break, and the page head.
const LINE_END_TAGS: &[&str] = &["br", "head"];

/// Whether a visible-text line ends at this element. Each block element and each line-end element begins and ends its
/// own line, so text joined across inline elements stays on one line.
fn ends_line(tag: &str) -> bool {
  BLOCK_TAGS.contains(&tag) || LINE_END_TAGS.contains(&tag)
}

/// Ends the visible text's current line, unless it is already at a line start.
fn end_line(text: &mut String) {
  if !text.is_empty() && !text.ends_with('\n') {
    text.push('\n');
  }
}

/// The text a page shows at open: every visible element's text, one line per block element and per source line break.
/// The body of a folded `details` block is left out, while its `summary` stays. `style` and `script` text is not text
/// the reader sees.
pub fn visible_text(page_text: &str) -> anyhow::Result<String> {
  struct Frame {
    tag: String,
    /// Whether text directly inside this element is visible.
    content_visible: bool,
    /// Whether a `summary` directly inside this element is visible, when the element is a `details`.
    summary_visible: bool,
  }
  let mut frames = vec![Frame { tag: "#root".to_owned(), content_visible: true, summary_visible: true }];
  let mut text = String::new();
  parse_with(page_text, true, |event| match event.kind {
    EventKind::StartTag | EventKind::StartEndTag => {
      let arguments = event.args.as_array().expect("a start-tag event has arguments");
      let tag = arguments[0].as_str().expect("a start-tag event has a tag").to_owned();
      let attribute_names = arguments[1]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|attribute| attribute.as_array().and_then(|pair| pair.first()).and_then(Value::as_str));
      let parent = frames.last().expect("the root frame stays on the stack");
      let parent_content = parent.content_visible;
      let (content_visible, summary_visible) = match tag.as_str() {
        "details" => {
          let folded = folds_at_open(&tag, attribute_names);
          (parent_content && !folded, parent_content)
        }
        "summary" => {
          let visible = if parent.tag == "details" { parent.summary_visible } else { parent_content };
          (visible, visible)
        }
        "style" | "script" => (false, false),
        _ => (parent_content, parent_content),
      };
      let block = ends_line(&tag);
      if block {
        end_line(&mut text);
      }
      if event.kind == EventKind::StartTag {
        frames.push(Frame { tag, content_visible, summary_visible });
      } else if block {
        end_line(&mut text);
      }
    }
    EventKind::EndTag => {
      let tag = event.args[0].as_str().expect("an end-tag event has a tag");
      if let Some(position) = (1..frames.len()).rev().find(|&position| frames[position].tag == tag) {
        frames.truncate(position);
      }
      if ends_line(tag) {
        end_line(&mut text);
      }
    }
    EventKind::Data => {
      if frames.last().expect("the root frame stays on the stack").content_visible
        && let Value::String(chunk) = &event.args[0]
      {
        text.push_str(chunk);
      }
    }
    EventKind::EntityReference
    | EventKind::CharacterReference
    | EventKind::Comment
    | EventKind::Declaration
    | EventKind::ProcessingInstruction
    | EventKind::UnknownDeclaration => {}
  })
  .map_err(|error| anyhow::anyhow!("{}: {}", error.exception, error.message))?;
  let lines = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>();
  Ok(lines.join("\n"))
}

/// The scheme a data URI starts with.
const DATA_SCHEME: &[u8] = b"data:";

/// The bytes that end a data URI's payload: the quote of its attribute or `url("...")`, the `)` of `url(...)`, the
/// `>` of an unquoted attribute or tag, and the whitespace before a `srcset` descriptor. A base64 payload holds none
/// of them, and a percent-encoded payload writes them as `%XX`.
const DATA_PAYLOAD_END: &[u8] = b"\"')<> \t\r\n\x0c";

/// Whether `byte` can continue the name a scheme follows, so that `metadata:` is not read as `data:`.
fn continues_name(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+')
}

/// The byte offset of the comma that ends a data URI's header, when `bytes` at `start` holds a header. A header is an
/// empty media type or a `type/subtype`, followed by `;name=value` parameters (RFC 2397).
fn data_header_end(bytes: &[u8], start: usize) -> Option<usize> {
  let mut index = start;
  while index < bytes.len() {
    match bytes[index] {
      b',' => {
        let header = &bytes[start..index];
        let media = header.split(|&byte| byte == b';').next().unwrap_or(header);
        let plausible = media.is_empty() || media.contains(&b'/');
        return plausible.then_some(index);
      }
      b if DATA_PAYLOAD_END.contains(&b) => return None,
      _ => index += 1,
    }
  }
  None
}

/// Whether a data URI header (the bytes between `data:` and the comma) carries the `;base64` parameter, whose
/// alphabet excludes the comma a `srcset` uses between candidates.
fn is_base64_header(header: &[u8]) -> bool {
  header.rsplit(|&byte| byte == b';').next().is_some_and(|part| part.eq_ignore_ascii_case(b"base64"))
}

/// Whether a byte belongs to the base64 alphabet, padding included.
fn is_base64_byte(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')
}

/// Whether a token is a srcset descriptor: a number and its unit, as in `2x`, `480w`, or `1.5x`.
fn is_srcset_descriptor(token: &[u8]) -> bool {
  token.split_last().is_some_and(|(unit, number)| {
    matches!(unit, b'w' | b'x' | b'h')
      && !number.is_empty()
      && number.iter().all(|&byte| byte.is_ascii_digit() || byte == b'.')
  })
}

/// The byte offset where the payload starting at `start` ends. A base64 payload inside quotes (`wraps`) may wrap onto
/// further lines, which a URL parser strips, so whitespace followed by a base64 token continues it; a srcset
/// descriptor after whitespace does not.
fn data_payload_end(bytes: &[u8], start: usize, base64: bool, wraps: bool) -> usize {
  let mut end = start;
  loop {
    while end < bytes.len() && !DATA_PAYLOAD_END.contains(&bytes[end]) && !(base64 && bytes[end] == b',') {
      end += 1;
    }
    if !wraps {
      return end;
    }
    let token_start = end + bytes[end..].iter().take_while(|byte| byte.is_ascii_whitespace()).count();
    let token_end = token_start + bytes[token_start..].iter().take_while(|&&byte| is_base64_byte(byte)).count();
    if token_start == end || token_end == token_start || is_srcset_descriptor(&bytes[token_start..token_end]) {
      return end;
    }
    end = token_end;
  }
}

/// The omitted payload's size as the placeholder states it: `512 B`, `1.4 KB`, `2.0 MB`.
fn omitted_size(bytes: usize) -> String {
  const KB: f64 = 1024.0;
  const MB: f64 = 1024.0 * 1024.0;
  let size = bytes as f64;
  if size >= MB {
    format!("{:.1} MB", size / MB)
  } else if size >= KB {
    format!("{:.1} KB", size / KB)
  } else {
    format!("{bytes} B")
  }
}

/// The page source a cold read sends: every `data:` URI replaced by `data:<header>,[<size> omitted]`. The header and
/// everything else in the source stay byte for byte. Inline screenshots make a self-contained page megabytes wide,
/// while a linked file keeps it at text size; the placeholder keeps the prompt at text size without touching the file.
fn omit_data_uris(page_text: &str) -> Cow<'_, str> {
  let bytes = page_text.as_bytes();
  let mut replaced: Option<String> = None;
  let mut copied = 0;
  let mut index = 0;
  while index + DATA_SCHEME.len() <= bytes.len() {
    let scheme = &bytes[index..index + DATA_SCHEME.len()];
    if !scheme.eq_ignore_ascii_case(DATA_SCHEME) || (index > 0 && continues_name(bytes[index - 1])) {
      index += 1;
      continue;
    }
    let header_start = index + DATA_SCHEME.len();
    let Some(comma) = data_header_end(bytes, header_start) else {
      index += 1;
      continue;
    };
    let payload_start = comma + 1;
    let base64 = is_base64_header(&bytes[header_start..comma]);
    let quoted = index > 0 && matches!(bytes[index - 1], b'"' | b'\'');
    let payload_end = data_payload_end(bytes, payload_start, base64, base64 && quoted);
    let out = replaced.get_or_insert_with(|| String::with_capacity(page_text.len()));
    out.push_str(&page_text[copied..payload_start]);
    out.push_str(&format!("[{} omitted]", omitted_size(payload_end - payload_start)));
    copied = payload_end;
    index = payload_end;
  }
  match replaced {
    None => Cow::Borrowed(page_text),
    Some(mut out) => {
      out.push_str(&page_text[copied..]);
      Cow::Owned(out)
    }
  }
}

/// The cold-read run's whole input: the page text with every `data:` URI replaced by a size placeholder, the VISIBLE
/// TEXT block of the page as it opens, then the questions with the trigger filled in. The visible text is read from
/// the prepared source, so a page-wide attribute value never reaches the HTML parser; a data URI sits in an attribute
/// value or CSS, which visible text does not carry, and a page without one is parsed as it came.
pub fn probe_input(page_text: &str, questions: &str, trigger: &str) -> anyhow::Result<String> {
  let source = omit_data_uris(page_text);
  Ok(format!(
    "{source}\n\n{VISIBLE_TEXT_HEADER}\n{}\n\n{}",
    visible_text(&source)?,
    questions.replace(TRIGGER_PLACEHOLDER, trigger)
  ))
}

/// The system prompt and the five questions one Waiting on you item is cold-read with, read fresh from
/// `prompts/cold_read.md` (`load_ask_texts`).
pub fn load_ask_texts(repo: &Path) -> anyhow::Result<(String, String)> {
  let row = row(ASK).expect("the ask row is registered");
  load_prompt_pair(repo, row.system_section, row.questions_section)
}

/// The ask probe's whole input: the item JSON, the host-served page text with every `data:` URI replaced by a size
/// placeholder and its VISIBLE TEXT block when there is a page, then the questions (`ask_probe_input`), joined by blank
/// lines.
pub fn ask_probe_input(item: &Value, linked_page: Option<&str>, questions: &str) -> anyhow::Result<String> {
  let mut pieces = vec![text_repr::json_dumps(item, &ITEM_JSON)];
  if let Some(page) = linked_page {
    let source = omit_data_uris(page);
    pieces.push(source.to_string());
    pieces.push(format!("{VISIBLE_TEXT_HEADER}\n{}", visible_text(&source)?));
  }
  pieces.push(questions.to_string());
  Ok(pieces.join("\n\n"))
}

/// The system prompt and the questions one web reply is cold-read with, read fresh from `prompts/cold_read.md`
/// (`load_reply_texts`).
pub fn load_reply_texts(repo: &Path) -> anyhow::Result<(String, String)> {
  let row = row(REPLY).expect("the reply row is registered");
  load_prompt_pair(repo, row.system_section, row.questions_section)
}

/// The whole input for a cold read of plain text, followed by its questions (`text_probe_input`).
pub fn text_probe_input(text: &str, questions: &str) -> String {
  format!("{text}\n\n{questions}")
}

/// The settle probe's whole input: the kept items as JSON, then the turn's record, then the questions
/// (`settle_probe_input`), joined by blank lines.
pub fn settle_probe_input(items: &Value, turn_text: &str, questions: &str) -> String {
  format!("{}\n\n{turn_text}\n\n{questions}", text_repr::json_dumps(items, &ITEM_JSON))
}

/// The findings parsed from a numbered cold-read answer with `question_count` questions.
///
/// Each numbered line is a reading of its question, and a line without a number joins the reading before it. A question
/// may repeat its numbered line: every reading that gives a finding is reported as its own finding, and readings that
/// all say none report nothing. A none reading beside a finding reading for the same question conflicts, so the answer
/// is refused.
pub fn parse_reply_findings(answer: &str, question_count: usize) -> anyhow::Result<Vec<String>> {
  let mut readings: Vec<Vec<String>> = vec![Vec::new(); question_count];
  let mut active = None;
  for line in answer.lines() {
    let trimmed = line.trim();
    let labelled = (0..question_count).find_map(|index| {
      let number = index + 1;
      [format!("({number})"), format!("{number}."), format!("（{number}）"), format!("{number}、")]
        .into_iter()
        .find_map(|label| trimmed.strip_prefix(label.as_str()).map(|rest| (index, rest)))
    });
    if let Some((index, rest)) = labelled {
      readings[index].push(rest.trim().trim_start_matches([':', '：', '.']).trim().to_string());
      active = Some((index, readings[index].len() - 1));
    } else if let Some((index, reading)) = active
      && !trimmed.is_empty()
    {
      let text = &mut readings[index][reading];
      text.push('\n');
      text.push_str(trimmed);
    }
  }
  anyhow::ensure!(
    readings.iter().all(|question| !question.is_empty()),
    "cold-read reply answer must answer all {question_count} numbered questions; raw answer:\n{answer}"
  );

  let mut findings = Vec::new();
  for (index, question) in readings.into_iter().enumerate() {
    let mut question_findings = Vec::new();
    let mut none_count = 0;
    for reading in question {
      let without_trailing = reading.trim().trim_end_matches('.').trim();
      if without_trailing.is_empty() || is_none_verdict(without_trailing) {
        none_count += 1;
      } else {
        question_findings.push(without_trailing.to_string());
      }
    }
    anyhow::ensure!(
      none_count == 0 || question_findings.is_empty(),
      "cold-read reply answer conflicts on question {}: lines say both none and a finding; raw answer:\n{}",
      index + 1,
      answer
    );
    findings.extend(question_findings);
  }
  Ok(findings)
}

fn is_none_verdict(text: &str) -> bool {
  let end = text.find(['.', '!', '?', '。', '！', '？']).unwrap_or(text.len());
  let verdict = text[..end].trim();
  let lower = verdict.to_ascii_lowercase();
  lower == "none" || lower == "no" || verdict == "无" || verdict == "没有"
}

/// The outcome of one light-backend probe: the failed attempts in order, then the answering backend and its verbatim
/// answer when one answers.
pub struct LightProbe {
  pub attempts: Vec<(String, String)>,
  pub backend_id: Option<String>,
  pub answer: Option<String>,
}

/// `run_probe`: sends `prompt` to each member of the `backends.calls.probe` target in turn, bounded by `PROBE_TIMEOUT`.
/// A backend that fails is recorded and the next member tried; a quota failure cools that member. Fails when the key is
/// unset.
pub async fn run_probe(host: &CliHost, prompt: &str, system_prompt: &str) -> anyhow::Result<LightProbe> {
  let target = host.cfg.backends.calls.get(CallClass::Probe);
  if target.is_none() {
    bail!("no probe backend resolvable: backends.calls.probe is unset");
  }
  let mut attempts = Vec::new();
  for option in backend_pool::one_shot_members(&host.cfg, Arc::clone(&host.pool), CallClass::Probe.key(), target) {
    let option = option?;
    let (error_type, outcome) =
      match backend_types::build_backend(Arc::clone(&host.registry), &option, &host.cfg, LaunchArgs::default()) {
        Ok(built) => (
          "RuntimeError",
          one_shot_text(built, &host.base_env.values, prompt, system_prompt, Some(PROBE_TIMEOUT)).await,
        ),
        Err(error) => ("ValueError", Err(error)),
      };
    match outcome {
      Ok(answer) => {
        return Ok(LightProbe { attempts, backend_id: Some(option.id), answer: Some(answer) });
      }
      Err(error) => {
        let message = format!("{error:#}");
        if let Some(reason) = backend_pool::classify_member_failure(&message, None).availability_reason() {
          let availability = host.pool.availability();
          availability.record(&host.cfg, &host.registry, &option.id, None, reason, &message)?;
        }
        attempts.push((option.id, format!("{error_type}: {message}")));
      }
    }
  }
  Ok(LightProbe { attempts, backend_id: None, answer: None })
}

/// The cold-read report body: the page and its sha256, one `attempt` line per failed run in order, then the answering
/// backend and its verbatim answer, or the all-failed trailer (`format_report`).
pub fn format_report(
  page_path: &str,
  page_sha256: &str,
  attempts: &[(String, String)],
  outcome: ColdReadOutcome,
) -> String {
  let mut lines = vec![format!("cold read of {page_path}"), format!("page sha256 {page_sha256}")];
  for (backend, error) in attempts {
    lines.push(format!("attempt {backend} failed: {error}"));
  }
  match outcome {
    ColdReadOutcome::AllFailed => lines.push(format!("every model failed ({} tried)", attempts.len())),
    ColdReadOutcome::Answered { backend_id, answer } => {
      lines.push(format!("backend {backend_id}"));
      lines.push(answer.to_string());
    }
  }
  lines.join("\n")
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;

  use anyhow::anyhow;
  use charliebot_core::config::{
    BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
  };
  use charliebot_registry::backend::{
    AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, Env, LaunchArgs, ResumeStyle, Translate,
    TransportKind,
  };
  use charliebot_registry::{Registry, RegistryError};
  use futures_util::future::BoxFuture;
  use serde_json::Value;
  use serde_yaml::Mapping;

  use super::*;

  /// A backend whose one-shot answers with `message` as an error; nothing starts a process.
  struct FailingBackend {
    message: String,
  }

  impl AgentBackend for FailingBackend {
    fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
      None
    }

    fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
      Ok(())
    }

    fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
      panic!("a failing one-shot never builds a command")
    }

    fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
      Ok(env)
    }

    fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
      None
    }

    fn translator(&self) -> Box<dyn Translate> {
      Box::new(IdentityTranslate)
    }

    fn transport(&self) -> TransportKind {
      TransportKind::RawLog
    }

    fn one_shot_text<'a>(
      &'a mut self,
      _base_env: &'a Env,
      _prompt: &'a str,
      _system: &'a str,
      _timeout: Option<Duration>,
    ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
      let message = self.message.clone();
      Some(Box::pin(async move { Err(anyhow!(message)) }))
    }
  }

  struct IdentityTranslate;

  impl Translate for IdentityTranslate {
    fn translate(&mut self, event: Value) -> anyhow::Result<Vec<Value>> {
      Ok(vec![event])
    }
  }

  /// Builds one failing backend per option: a `failing-probe` option fails with the option's message, and a
  /// `refused-probe` option fails with the gateway's model-access refusal.
  struct FailingFactory;

  impl BackendFactory for FailingFactory {
    fn build(
      &self,
      option: Option<&BackendOption>,
      _cfg: &CharlieBotConfig,
      _launch: LaunchArgs,
    ) -> anyhow::Result<Box<dyn AgentBackend>> {
      let option = option.expect("the probe builds a configured backend");
      let message = if option.backend_type == REFUSED_PROBE_TYPE {
        "Failed to authenticate. API Error: 401 Not allowed to access model due to tags configuration.".to_string()
      } else if option.id == "alpha" {
        String::new()
      } else {
        "kaput".to_string()
      };
      Ok(Box::new(FailingBackend { message }))
    }
  }

  fn register_failing(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.backend_type(BackendTypeRegistration {
      type_name: "failing-probe",
      traits: BackendTraits {
        resume: ResumeStyle::NativeId,
        restart_reattach: false,
        preassigned_session_id: false,
        family_prefix: None,
        manual_compact: false,
        compact_hint: None,
      },
      option_model: |_| Ok(()),
      factory: Arc::new(FailingFactory),
      lifecycle: None,
      translate_fallback: false,
    })?;
    registry.backend_type(BackendTypeRegistration {
      type_name: REFUSED_PROBE_TYPE,
      traits: BackendTraits {
        resume: ResumeStyle::NativeId,
        restart_reattach: false,
        preassigned_session_id: false,
        family_prefix: None,
        manual_compact: false,
        compact_hint: None,
      },
      option_model: |_| Ok(()),
      factory: Arc::new(FailingFactory),
      lifecycle: None,
      translate_fallback: false,
    })
  }

  /// The backend type whose one-shot answers with the review run's model-access refusal.
  const REFUSED_PROBE_TYPE: &str = "refused-probe";

  fn probe_cfg(ids: [&str; 2]) -> CharlieBotConfig {
    let options = ids
      .into_iter()
      .map(|id| BackendOption {
        id: id.to_string(),
        label: id.to_string(),
        model: "synthetic-model".to_string(),
        prompt_overlay: None,
        billing: None,
        backend_type: "failing-probe".to_string(),
        type_fields: Mapping::new(),
        visible: true,
      })
      .collect();
    CharlieBotConfig {
      charliebot_home: PathBuf::from("/synthetic/home"),
      headless_chrome_bin: "/synthetic/chrome".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18650,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 0,
        session_swap_max_mb: 0,
        public_url: None,
        session_tasks_max: 0,
        min_free_disk_gib: 0,
        enable_session_cpu_weight: true,
        enable_session_io_weight: true,
        enable_local_work_guard: false,
        enable_offline_shell: false,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig {
        routing: None,
        options,
        pools: crate::test_support::recovery_support::rotation_pools(ids.map(str::to_string).to_vec()).0,
        calls: crate::test_support::recovery_support::rotation_pools(ids.map(str::to_string).to_vec()).1,
      },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      remote_run: charliebot_core::config::RemoteRunConfig::default(),
      github: Default::default(),
      sections: Sections::default(),
    }
  }

  const FOLDED_PAGE: &str = r#"<html><body>
<p>Visible opening.</p>
<details class="details-layer"><summary>Fold title</summary><p>Folded body.</p></details>
<details open><summary>Open title</summary><p>Open body.</p></details>
</body></html>"#;

  #[test]
  fn visible_text_keeps_a_closed_folds_summary_and_drops_its_body() {
    let text = visible_text(FOLDED_PAGE).unwrap();
    assert!(text.contains("Fold title"), "{text}");
    assert!(!text.contains("Folded body."), "{text}");
  }

  #[test]
  fn visible_text_puts_each_block_element_on_its_own_line() {
    let page = "<ul><li>「keep」: keep the file.</li><li>「delete」: delete the file.</li></ul>\
                <div><p>Inline <b>text</b> stays.</p></div>\
                <p>Before <dialog>Shown</dialog> <hgroup>Heading</hgroup> after</p>";
    assert_eq!(
      visible_text(page).unwrap(),
      "「keep」: keep the file.\n「delete」: delete the file.\nInline text stays.\nBefore\nShown\nHeading\nafter"
    );
  }

  #[test]
  fn visible_text_keeps_an_open_folds_body() {
    let text = visible_text(FOLDED_PAGE).unwrap();
    assert!(text.contains("Open title"), "{text}");
    assert!(text.contains("Open body."), "{text}");
  }

  #[test]
  fn visible_text_drops_a_nested_folds_body() {
    let page =
      "<details><summary>Outer</summary><details><summary>Inner</summary><p>Inner body.</p></details></details>";
    let text = visible_text(page).unwrap();
    assert!(text.contains("Outer"), "{text}");
    assert!(!text.contains("Inner"), "{text}");
  }

  #[test]
  fn probe_input_keeps_the_full_source_before_the_visible_text_block() {
    let input = probe_input(FOLDED_PAGE, "questions", "the trigger").unwrap();
    let (source, rest) = input.split_once(VISIBLE_TEXT_HEADER).expect("the probe carries the visible text block");
    assert_eq!(source, format!("{FOLDED_PAGE}\n\n"));
    assert!(source.contains("Folded body."), "the other questions still read the whole source");
    assert!(rest.contains("Fold title"), "{rest}");
    assert!(!rest.contains("Folded body."), "{rest}");
  }

  #[test]
  fn ask_probe_input_carries_the_linked_pages_visible_text_block_and_the_questions() {
    let item = serde_json::json!({"text": "Choose."});
    let input = ask_probe_input(&item, Some(FOLDED_PAGE), "questions").unwrap();
    let (source, rest) = input.split_once(VISIBLE_TEXT_HEADER).expect("the ask probe carries the visible text block");
    assert!(source.contains("Folded body."), "{source}");
    assert!(!rest.contains("Folded body."), "{rest}");
    let without_page = ask_probe_input(&item, None, "questions").unwrap();
    assert!(!without_page.contains(VISIBLE_TEXT_HEADER), "{without_page}");
    assert!(without_page.ends_with("\n\nquestions"), "{without_page}");
  }

  /// A self-contained page whose one image is a base64 PNG payload of `payload_bytes` characters.
  fn page_with_inline_png(payload_bytes: usize) -> String {
    format!(
      concat!(
        r#"<html><body><p>Before the chart.</p>"#,
        r#"<img alt="The monitor line chart" src="data:image/png;base64,{}">"#,
        r#"<p>After the chart.</p></body></html>"#
      ),
      "A".repeat(payload_bytes)
    )
  }

  const INLINE_PNG_PLACEHOLDER: &str = "data:image/png;base64,[64.0 KB omitted]";

  #[test]
  fn probe_input_omits_an_inline_image_and_keeps_the_alt_text() {
    let page = page_with_inline_png(64 * 1024);
    let input = probe_input(&page, "questions", "the trigger").unwrap();
    assert!(input.len() < 50 * 1024, "the probe input is {} bytes", input.len());
    assert!(input.contains("The monitor line chart"), "the alt text stays in the source");
    assert!(input.contains(INLINE_PNG_PLACEHOLDER), "the placeholder states the media type and the omitted size");
    assert!(!input.contains("AAAA"), "no payload byte reaches the prompt");
    assert!(input.contains("Before the chart.") && input.contains("After the chart."), "{input}");
  }

  #[test]
  fn ask_probe_input_omits_an_inline_image_in_the_linked_page() {
    let page = page_with_inline_png(64 * 1024);
    let item = serde_json::json!({"text": "Choose."});
    let input = ask_probe_input(&item, Some(&page), "questions").unwrap();
    assert!(input.len() < 50 * 1024, "the ask probe input is {} bytes", input.len());
    assert!(input.contains("The monitor line chart"), "{input}");
    assert!(input.contains(INLINE_PNG_PLACEHOLDER), "{input}");
  }

  #[test]
  fn every_data_uri_position_is_replaced_and_the_rest_stays_byte_for_byte() {
    let page = concat!(
      r#"<a href="data:text/plain;charset=utf-8,hello%20world">link</a>"#,
      r#"<img srcset="data:image/png;base64,AAAA 1x, pic.png 2x" alt="chart">"#,
      r#"<div style="background: url(data:image/svg+xml;charset=utf-8,%3Csvg%3E)">x</div>"#,
      r#"<img src="data:image/jpeg;base64,BBBB" alt="kept alt">"#,
    );
    let expected = concat!(
      r#"<a href="data:text/plain;charset=utf-8,[13 B omitted]">link</a>"#,
      r#"<img srcset="data:image/png;base64,[4 B omitted] 1x, pic.png 2x" alt="chart">"#,
      r#"<div style="background: url(data:image/svg+xml;charset=utf-8,[9 B omitted])">x</div>"#,
      r#"<img src="data:image/jpeg;base64,[4 B omitted]" alt="kept alt">"#,
    );
    let input = probe_input(page, "questions", "the trigger").unwrap();
    let (source, _) = input.split_once(VISIBLE_TEXT_HEADER).expect("the probe carries the visible text block");
    assert_eq!(source, format!("{expected}\n\n"));
  }

  #[test]
  fn a_page_without_a_data_uri_yields_the_same_probe_input() {
    let input = probe_input(FOLDED_PAGE, "questions", "the trigger").unwrap();
    assert_eq!(
      input,
      format!("{FOLDED_PAGE}\n\n{VISIBLE_TEXT_HEADER}\n{}\n\nquestions", visible_text(FOLDED_PAGE).unwrap())
    );
  }

  #[test]
  fn a_data_word_in_the_page_text_is_not_replaced() {
    let page = "<p>metadata: a/b, and the data: schema, and data:foo, stay as prose.</p>";
    assert_eq!(omit_data_uris(page).as_ref(), page);
  }

  #[test]
  fn a_base64_payload_wrapped_inside_quotes_is_replaced_whole() {
    let page = r#"<img src="data:image/png;base64,AAAA
BBBB
  CCCC" alt="wrapped"><p>after</p>"#;
    assert_eq!(
      omit_data_uris(page).as_ref(),
      r#"<img src="data:image/png;base64,[16 B omitted]" alt="wrapped"><p>after</p>"#
    );
  }

  #[test]
  fn an_unquoted_base64_payload_ends_at_its_whitespace() {
    let page = "<img src=data:image/png;base64,AAAA alt=Chart>";
    assert_eq!(omit_data_uris(page).as_ref(), "<img src=data:image/png;base64,[4 B omitted] alt=Chart>");
  }

  #[tokio::test]
  async fn test_probe_records_each_failed_attempt_with_its_exception_type() {
    // The probe records both failures as `RuntimeError`, the first with an empty message.
    let mut registry = Registry::new();
    registry.register_package("tests.cold_read", register_failing).unwrap();
    let registry = Arc::new(registry);
    let home = tempfile::tempdir().unwrap();
    let host = CliHost {
      cfg: probe_cfg(["alpha", "beta"]),
      registry: Arc::clone(&registry),
      repo_root: PathBuf::from("/synthetic/repo"),
      base_env: charliebot_agents::agent_environment::AgentBaseEnvironment {
        values: Env::from([
          ("PATH".to_string(), "/synthetic/gh-wrapper:/bin:/usr/bin".to_string()),
          (charliebot_agents::gh_wrapper::WRAPPER_PATH_VAR.to_string(), "/synthetic/gh-wrapper/gh".to_string()),
        ]),
      },
      pool: std::sync::Arc::new(charliebot_agents::backend_pool::BackendPool::new(
        std::sync::Arc::new(charliebot_agents::backend_availability::BackendAvailability::load(home.path()).unwrap()),
        Arc::clone(&registry),
        Arc::new(charliebot_agents::model_pool::RotationPositions::default()),
      )),
    };

    let probe = run_probe(&host, "prompt", "system").await.unwrap();

    assert_eq!(
      probe.attempts,
      [("alpha".to_string(), "RuntimeError: ".to_string()), ("beta".to_string(), "RuntimeError: kaput".to_string())]
    );
    assert_eq!(probe.backend_id, None);
    assert_eq!(probe.answer, None);
  }

  #[tokio::test]
  async fn test_probe_records_a_refused_member_with_the_refusal_reason() {
    let mut registry = Registry::new();
    registry.register_package("tests.cold_read", register_failing).unwrap();
    let registry = Arc::new(registry);
    let home = tempfile::tempdir().unwrap();
    let mut cfg = probe_cfg(["alpha", "beta"]);
    for option in &mut cfg.backends.options {
      option.backend_type = REFUSED_PROBE_TYPE.to_string();
    }
    let pool = std::sync::Arc::new(charliebot_agents::backend_pool::BackendPool::new(
      std::sync::Arc::new(charliebot_agents::backend_availability::BackendAvailability::load(home.path()).unwrap()),
      Arc::clone(&registry),
      Arc::new(charliebot_agents::model_pool::RotationPositions::default()),
    ));
    let host = CliHost {
      cfg,
      registry,
      repo_root: PathBuf::from("/synthetic/repo"),
      base_env: charliebot_agents::agent_environment::AgentBaseEnvironment {
        values: Env::from([
          ("PATH".to_string(), "/synthetic/gh-wrapper:/bin:/usr/bin".to_string()),
          (charliebot_agents::gh_wrapper::WRAPPER_PATH_VAR.to_string(), "/synthetic/gh-wrapper/gh".to_string()),
        ]),
      },
      pool: std::sync::Arc::clone(&pool),
    };

    let probe = run_probe(&host, "prompt", "system").await.unwrap();

    assert_eq!(
      probe.attempts.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
      ["alpha", "beta"],
      "the probe tries every member after a refusal"
    );
    assert!(probe.attempts.iter().all(|(_, error)| error.contains("Failed to authenticate")));
    let row = pool.availability().row("alpha").expect("the refused member is recorded out");
    assert_eq!(row.reason, charliebot_agents::backend_availability::REASON_ACCESS_REFUSED);
    assert_eq!(probe.backend_id, None);
    assert_eq!(probe.answer, None);
  }

  #[test]
  fn reply_findings_parser_extracts_findings_and_ignores_none_answers() {
    let clean = "(1) none\n(2) none\n(3) none";
    assert!(parse_reply_findings(clean, REPLY_QUESTION_COUNT).unwrap().is_empty());

    let with_findings = "(1) 0a1c1ef stands without a name.\n(2) 「读回」is a rule-defined agent term.\n(3) none";
    assert_eq!(
      parse_reply_findings(with_findings, REPLY_QUESTION_COUNT).unwrap(),
      vec!["0a1c1ef stands without a name", "「读回」is a rule-defined agent term"]
    );
  }

  #[test]
  fn reply_findings_parser_takes_a_missing_decision_fact_as_the_third_finding() {
    let answer = "(1) none\n(2) none\n(3) The cost of option B is not stated; the deadline is missing.";

    assert_eq!(
      parse_reply_findings(answer, REPLY_QUESTION_COUNT).unwrap(),
      vec!["The cost of option B is not stated; the deadline is missing"]
    );
  }

  #[test]
  fn reply_findings_parser_accepts_chinese_answer_labels() {
    let answers =
      ["（1）none\n（2）none\n（3）none", "（1）：none\n（2）：none\n（3）：none", "1、none\n2、none\n3、none"];
    for answer in answers {
      assert!(parse_reply_findings(answer, REPLY_QUESTION_COUNT).unwrap().is_empty(), "{answer}");
    }
  }

  #[test]
  fn reply_findings_parser_ignores_explanations_after_chinese_none_verdicts() {
    let clean_answers = [
      "None. There are no bare identifiers.",
      "No. There are no unfamiliar terms.",
      "无。这条回复里没有 commit、session id ... 不构成裸标识符。",
      "没有。会话标识 dcbc00f8 已与可读名称「主动工作」并列并带链接，其余未出现裸标识符。",
    ];

    for answer in clean_answers {
      let response = format!("(1) {answer}\n(2) none\n(3) none");
      assert!(parse_reply_findings(&response, REPLY_QUESTION_COUNT).unwrap().is_empty(), "{response}");
    }
  }

  #[test]
  fn reply_findings_parser_keeps_a_finding_when_a_later_sentence_says_none() {
    let answer = r#"(1) 有三个：一是 "session" stands bare. None.
(2) none
(3) none"#;

    assert_eq!(
      parse_reply_findings(answer, REPLY_QUESTION_COUNT).unwrap(),
      [r#"有三个：一是 "session" stands bare. None"#]
    );
  }

  #[test]
  fn reply_findings_parser_rejects_an_incomplete_answer() {
    assert!(parse_reply_findings("(1) none", REPLY_QUESTION_COUNT).is_err());
    assert!(parse_reply_findings("(1) none\n(2) none", REPLY_QUESTION_COUNT).is_err());
    assert!(parse_reply_findings("No findings.", REPLY_QUESTION_COUNT).is_err());
  }

  #[test]
  fn reply_findings_parser_reports_an_unlabelled_answer() {
    let answer = "无。这条回复里没有 commit、session id ... 不构成裸标识符。";
    let error = parse_reply_findings(answer, REPLY_QUESTION_COUNT).unwrap_err().to_string();

    assert!(error.contains(answer), "{error}");
  }

  #[test]
  fn now_findings_parser_accepts_one_question_and_rejects_unparseable_answers() {
    assert!(parse_reply_findings("(1) none.", NOW_QUESTION_COUNT).unwrap().is_empty());
    assert_eq!(
      parse_reply_findings("(1) \"Please approve the change.\"", NOW_QUESTION_COUNT).unwrap(),
      vec!["\"Please approve the change.\""]
    );
    assert!(parse_reply_findings("The status is clear.", NOW_QUESTION_COUNT).is_err());
  }

  #[test]
  fn now_findings_parser_reports_each_line_of_a_repeated_question() {
    let n346 = concat!(
      r#"(1) "d4fa7106 等你定固定 IP 与联系刘旭" asks the user to decide the fixed IP and contact 刘旭."#,
      "\n",
      r#"(1) "fpt-k8s 的新版安装和 PodMonitor 同步等上手试跑结束后我发 resume" waits on the user's hands-on "#,
      r#"trial run to finish before the agent sends resume."#,
    );
    assert_eq!(
      parse_reply_findings(n346, NOW_QUESTION_COUNT).unwrap(),
      [
        "\"d4fa7106 等你定固定 IP 与联系刘旭\" asks the user to decide the fixed IP and contact 刘旭",
        concat!(
          "\"fpt-k8s 的新版安装和 PodMonitor 同步等上手试跑结束后我发 resume\" waits on the user's hands-on ",
          "trial run to finish before the agent sends resume",
        ),
      ]
    );

    let n231 = concat!(
      r#"(1) "等待回报后发你审阅" (the draft goes to the user for review)."#,
      "\n",
      r#"(1) "原理解页继续挂在 Waiting on you" (a mention of the user's Waiting on you list)."#,
    );
    assert_eq!(
      parse_reply_findings(n231, NOW_QUESTION_COUNT).unwrap(),
      [
        "\"等待回报后发你审阅\" (the draft goes to the user for review)",
        "\"原理解页继续挂在 Waiting on you\" (a mention of the user's Waiting on you list)",
      ]
    );

    let l0276 = concat!(
      r#"(1) "训练线计划待审" (the training-line plan is pending review; "#,
      r#"the Now text doesn't name the reviewer, "#,
      r#"but the approvals it lists suggest the user)"#,
      "\n",
      r#"(1) "接口计划 v3 已备（新增 sudo 取舍）" (the sudo trade-off in the interface plan "#,
      r#"may need the user's choice)."#,
    );
    assert_eq!(
      parse_reply_findings(l0276, NOW_QUESTION_COUNT).unwrap(),
      [
        concat!(
          "\"训练线计划待审\" (the training-line plan is pending review; ",
          "the Now text doesn't name the reviewer, ",
          "but the approvals it lists suggest the user)",
        ),
        "\"接口计划 v3 已备（新增 sudo 取舍）\" (the sudo trade-off in the interface plan may need the user's choice)",
      ]
    );
  }

  #[test]
  fn now_findings_parser_refuses_a_repeated_question_whose_lines_conflict() {
    for answer in ["(1) none\n(1) \"Please approve the change.\"", "(1) \"Please approve the change.\"\n(1) none"] {
      let error = parse_reply_findings(answer, NOW_QUESTION_COUNT).unwrap_err().to_string();
      assert!(error.contains("conflicts on question 1"), "{error}");
      assert!(error.contains(answer), "{error}");
    }
    assert!(parse_reply_findings("(1) none\n(1) none", NOW_QUESTION_COUNT).unwrap().is_empty());
  }

  #[test]
  fn prompt_pair_loader_reads_the_requested_section_names() {
    let repo = tempfile::tempdir().unwrap();
    let prompt_dir = repo.path().join("prompts");
    std::fs::create_dir_all(&prompt_dir).unwrap();
    std::fs::write(
      prompt_dir.join(PROMPT_FILENAME),
      "<!-- section: now_system_prompt -->\nNow system\n<!-- section: now_questions -->\nNow question\n",
    )
    .unwrap();

    assert_eq!(
      load_prompt_pair(repo.path(), NOW_SYSTEM_SECTION, NOW_QUESTIONS_SECTION).unwrap(),
      ("Now system".to_string(), "Now question".to_string())
    );
  }

  /// The registry's rows in order, each with the refusal rule the plan fixes for it.
  #[test]
  fn the_registry_names_every_check_with_its_refusal_rule() {
    let expected = [
      (PAGE, RefusalRule::None),
      (REPLY, RefusalRule::HoldOnFindings),
      (ASK, RefusalRule::RequireRecord),
      (SETTLE, RefusalRule::RequireRecordSinceInput),
      (NOW, RefusalRule::RefuseOnFindings),
    ];
    assert_eq!(REGISTRY.len(), expected.len());
    for ((name, refusal), registered) in expected.into_iter().zip(REGISTRY.iter()) {
      let found = row(name).expect("each registered name resolves to its row");
      assert_eq!((found.name, found.refusal), (registered.name, registered.refusal));
      assert_eq!((registered.name, registered.refusal), (name, refusal));
    }
    assert!(row("unknown-check").is_none());
    assert_eq!(RECORDED_NAMES, [REPLY, ASK, SETTLE]);
  }

  /// Every registry row's prompt pair resolves from the sections it names, and no row shares a section with another.
  #[test]
  fn every_registry_row_reads_its_own_prompt_pair() {
    let repo = tempfile::tempdir().unwrap();
    let prompt_dir = repo.path().join("prompts");
    std::fs::create_dir_all(&prompt_dir).unwrap();
    let sections = REGISTRY
      .iter()
      .flat_map(|row| {
        [
          format!("<!-- section: {} -->\n{} system", row.system_section, row.name),
          format!("<!-- section: {} -->\n{} questions", row.questions_section, row.name),
        ]
      })
      .collect::<Vec<_>>()
      .join("\n");
    std::fs::write(prompt_dir.join(PROMPT_FILENAME), sections).unwrap();

    let mut used = std::collections::HashSet::new();
    for registered in &REGISTRY {
      let (system, questions) = registered.prompt_pair(repo.path()).unwrap();
      assert_eq!(system, format!("{} system", registered.name));
      assert_eq!(questions, format!("{} questions", registered.name));
      assert!(used.insert(registered.system_section), "{} shares a section", registered.name);
      assert!(used.insert(registered.questions_section), "{} shares a section", registered.name);
    }
  }

  #[test]
  fn the_settle_probe_input_is_the_items_then_the_turn_record_then_the_questions() {
    let items = serde_json::json!([{"text": "Decide", "context": "The line waits."}]);
    let input = settle_probe_input(&items, "[2026-01-01T00:00:00] agent: done", "questions");
    assert!(input.starts_with("[\n  {"), "{input}");
    assert!(input.contains("\"text\": \"Decide\""), "{input}");
    assert!(input.ends_with("\n\n[2026-01-01T00:00:00] agent: done\n\nquestions"), "{input}");
  }
}

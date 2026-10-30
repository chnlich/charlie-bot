//! The cold-read run's shared contract: the prompt texts, the probe input, and the report body
//! (`src/runtime/cold_read.py`).
//!
//! `prompts/cold_read.md` is the only home of the cold-reader instructions and the seven questions. The artifact check
//! and the cold-read child session both build from here, so the foreground output and the completion report carry the
//! same lines.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use anyhow::bail;
use charliebot_agents::backend_pool::is_quota_error;
use charliebot_agents::cli_host::CliHost;
use charliebot_agents::{backend_types, one_shot_text};
use charliebot_core::text_repr::{self, JsonStyle};
use charliebot_html::{EventKind, folds_at_open, parse_with};
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
/// `json.dumps(item, ensure_ascii=False, indent=2)`: the item JSON an ask probe carries.
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

/// The system prompt and the seven questions, read fresh from `prompts/cold_read.md`. Trailing newlines stay out of
/// both, so the probe input is byte-identical to the file's section text.
pub fn load_texts(repo: &Path) -> anyhow::Result<(String, String)> {
  let sections = spawner_prompt::load_marker_sections(
    &repo.join("prompts").join(PROMPT_FILENAME),
    &[SYSTEM_SECTION, QUESTIONS_SECTION],
    "cold-read",
  )?;
  Ok((
    sections[SYSTEM_SECTION].trim_end_matches('\n').to_string(),
    sections[QUESTIONS_SECTION].trim_end_matches('\n').to_string(),
  ))
}

/// The block a probe adds after the full source: the text the reader sees at page open, folded blocks closed.
pub const VISIBLE_TEXT_HEADER: &str = "VISIBLE TEXT";

/// The elements that start and end a line in the visible text. Text joined across inline elements stays on one line;
/// each of these elements begins and ends its own line.
const BLOCK_TAGS: &[&str] = &[
  "address",
  "article",
  "aside",
  "blockquote",
  "body",
  "br",
  "dd",
  "details",
  "div",
  "dl",
  "dt",
  "fieldset",
  "figcaption",
  "figure",
  "footer",
  "form",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "head",
  "header",
  "hr",
  "li",
  "main",
  "nav",
  "ol",
  "p",
  "pre",
  "section",
  "summary",
  "table",
  "tbody",
  "td",
  "tfoot",
  "th",
  "thead",
  "tr",
  "ul",
];

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
      let block = BLOCK_TAGS.contains(&tag.as_str());
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
      if BLOCK_TAGS.contains(&tag) {
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
  let sections = spawner_prompt::load_marker_sections(
    &repo.join("prompts").join(PROMPT_FILENAME),
    &[ASK_SYSTEM_SECTION, ASK_QUESTIONS_SECTION],
    "cold-read",
  )?;
  Ok((
    sections[ASK_SYSTEM_SECTION].trim_end_matches('\n').to_string(),
    sections[ASK_QUESTIONS_SECTION].trim_end_matches('\n').to_string(),
  ))
}

/// The ask probe's policy section: the titled approval policy table the ask questions tier against, read fresh from
/// the host home (`ask_policy_section`).
pub fn ask_policy_section(home: &Path) -> anyhow::Result<String> {
  let loaded = charliebot_core::approval_policy::load(home)?;
  Ok(charliebot_core::approval_policy::prompt_section(&loaded.table))
}

/// The ask probe's whole input: the item JSON, the host-served page text with every `data:` URI replaced by a size
/// placeholder and its VISIBLE TEXT block when there is a page, the approval policy table section, then the questions
/// (`ask_probe_input`), joined by blank lines.
pub fn ask_probe_input(
  item: &Value,
  linked_page: Option<&str>,
  policy_section: &str,
  questions: &str,
) -> anyhow::Result<String> {
  let mut pieces = vec![text_repr::json_dumps(item, &ITEM_JSON)];
  if let Some(page) = linked_page {
    let source = omit_data_uris(page);
    pieces.push(source.to_string());
    pieces.push(format!("{VISIBLE_TEXT_HEADER}\n{}", visible_text(&source)?));
  }
  pieces.push(policy_section.to_string());
  pieces.push(questions.to_string());
  Ok(pieces.join("\n\n"))
}

/// The outcome of one light-backend probe: the failed attempts in order, then the answering backend and its verbatim
/// answer when one answers.
pub struct LightProbe {
  pub attempts: Vec<(String, String)>,
  pub backend_id: Option<String>,
  pub answer: Option<String>,
}

/// `run_light_probe`: sends `prompt` to each light backend in pool rotation with no timeout. A backend that fails is
/// recorded and the next member tried; a quota failure cools that member. Fails when `backends.pool` is empty.
pub async fn run_light_probe(host: &CliHost, prompt: &str, system_prompt: &str) -> anyhow::Result<LightProbe> {
  if host.cfg.backends.pool.is_empty() {
    bail!("no light backends resolvable: backends.pool is empty");
  }
  let mut attempts = Vec::new();
  let mut tried: Vec<String> = Vec::new();
  while let Some(option) = host.pool.select(&host.cfg, "light_oneshot", &tried)? {
    tried.push(option.id.clone());
    let (error_type, outcome) =
      match backend_types::build_backend(Arc::clone(&host.registry), &option, &host.cfg, LaunchArgs::default()) {
        Ok(built) => ("RuntimeError", one_shot_text(built, &host.base_env.values, prompt, system_prompt, None).await),
        Err(error) => ("ValueError", Err(error)),
      };
    match outcome {
      Ok(answer) => {
        return Ok(LightProbe { attempts, backend_id: Some(option.id), answer: Some(answer) });
      }
      Err(error) => {
        let message = format!("{error:#}");
        if is_quota_error(&message) {
          host.pool.cool(&option.id, None);
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
  use std::fs;
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

  /// Builds one failing backend per option: `alpha` fails with an empty message, `beta` with `kaput`.
  struct FailingFactory;

  impl BackendFactory for FailingFactory {
    fn build(
      &self,
      option: Option<&BackendOption>,
      _cfg: &CharlieBotConfig,
      _launch: LaunchArgs,
    ) -> anyhow::Result<Box<dyn AgentBackend>> {
      let id = option.expect("the probe builds a configured backend").id.clone();
      let message = if id == "alpha" { String::new() } else { "kaput".to_string() };
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
    })
  }

  fn probe_cfg(ids: [&str; 2]) -> CharlieBotConfig {
    let options = ids
      .into_iter()
      .map(|id| BackendOption {
        id: id.to_string(),
        label: id.to_string(),
        model: "synthetic-model".to_string(),
        prompt_overlay: None,
        backend_type: "failing-probe".to_string(),
        type_fields: Mapping::new(),
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
        session_tasks_max: 0,
        min_free_disk_gib: 0,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig { pool: ids.map(str::to_string).to_vec(), routing: None, options },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
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
                <div><p>Inline <b>text</b> stays.</p></div>";
    assert_eq!(
      visible_text(page).unwrap(),
      "「keep」: keep the file.\n「delete」: delete the file.\nInline text stays."
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
  fn ask_probe_input_carries_the_linked_pages_visible_text_block_and_the_policy_section() {
    let item = serde_json::json!({"text": "Choose."});
    let input = ask_probe_input(&item, Some(FOLDED_PAGE), "policy section", "questions").unwrap();
    let (source, rest) = input.split_once(VISIBLE_TEXT_HEADER).expect("the ask probe carries the visible text block");
    assert!(source.contains("Folded body."), "{source}");
    assert!(!rest.contains("Folded body."), "{rest}");
    let without_page = ask_probe_input(&item, None, "policy section", "questions").unwrap();
    assert!(!without_page.contains(VISIBLE_TEXT_HEADER), "{without_page}");
    // The policy section sits between the page and the questions.
    let (before_questions, questions) = without_page.split_once("\n\nquestions").expect("the questions come last");
    assert!(before_questions.ends_with("policy section"), "{before_questions}");
    assert_eq!(questions, "");
  }

  #[test]
  fn an_edited_table_reaches_the_next_ask_probe_without_a_prompt_file_change() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap().to_path_buf();
    let home = tempfile::tempdir().unwrap();
    let table = home.path().join("config.d").join("approval.d").join("policy.yaml");
    fs::create_dir_all(table.parent().unwrap()).unwrap();
    const TABLE: &str = "\
consequence_tests:
  - {id: irreversible, rank: 1, text: \"Deletes data with no backup, or sends what cannot be recalled.\"}
  - {id: chao-name, rank: 2, text: \"A message or publication in Chao's name reaches other people.\"}
  - {id: money, rank: 3, text: \"Spends money beyond a budget that Chao set.\"}
  - {id: authority, rank: 4, text: \"Widens what agents may do.\"}
  - {id: rule-named, rank: 4, text: \"A rule in force names the user's explicit approval for this step.\"}
  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}
standing_policies: []
enable_ask_tier_refusal: false
";
    fs::write(&table, TABLE).unwrap();
    let prompt_file = repo.join("prompts").join(PROMPT_FILENAME);
    let prompt_bytes_before = fs::read(&prompt_file).unwrap();

    let item = serde_json::json!({"text": "Choose."});
    let (_system_prompt, questions) = load_ask_texts(&repo).unwrap();
    let before = ask_probe_input(&item, None, &ask_policy_section(home.path()).unwrap(), &questions).unwrap();
    fs::write(
      &table,
      TABLE.replace(
        "standing_policies: []",
        "standing_policies:\n  - {id: release-restart, scope: \"s\", cap_per_pt_day: null, expires: 2099-01-01}",
      ),
    )
    .unwrap();
    let after = ask_probe_input(&item, None, &ask_policy_section(home.path()).unwrap(), &questions).unwrap();

    assert!(before.contains("## Approval policy table"), "{before}");
    assert!(!before.contains("release-restart"), "{before}");
    assert!(after.contains("- id: release-restart"), "{after}");
    // The same questions and system prompt join both probes; only the table section moved.
    let (before_section, _) = before.rsplit_once("## Approval policy table").unwrap();
    let (after_section, _) = after.rsplit_once("## Approval policy table").unwrap();
    assert_eq!(before_section, after_section);
    assert_eq!(fs::read(&prompt_file).unwrap(), prompt_bytes_before, "no prompt file changed");
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

  const INLINE_PNG_PLACEHOLDER: &str = "data:image/png;base64,[2.0 MB omitted]";

  #[test]
  fn probe_input_omits_an_inline_image_and_keeps_the_alt_text() {
    let page = page_with_inline_png(2 * 1024 * 1024);
    let input = probe_input(&page, "questions", "the trigger").unwrap();
    assert!(input.len() < 50 * 1024, "the probe input is {} bytes", input.len());
    assert!(input.contains("The monitor line chart"), "the alt text stays in the source");
    assert!(input.contains(INLINE_PNG_PLACEHOLDER), "the placeholder states the media type and the omitted size");
    assert!(!input.contains("AAAA"), "no payload byte reaches the prompt");
    assert!(input.contains("Before the chart.") && input.contains("After the chart."), "{input}");
  }

  #[test]
  fn ask_probe_input_omits_an_inline_image_in_the_linked_page() {
    let page = page_with_inline_png(2 * 1024 * 1024);
    let item = serde_json::json!({"text": "Choose."});
    let input = ask_probe_input(&item, Some(&page), "policy section", "questions").unwrap();
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
    // `tests/core/test_artifact_check.py::test_probe_records_each_failed_attempt_with_its_exception_type`.
    // Python records `TimeoutError: ` and `RuntimeError: kaput`; the Rust probe names its own failure class, so the
    // records below are `RuntimeError` for both failures, the first with an empty message.
    let mut registry = Registry::new();
    registry.register_package("tests.cold_read", register_failing).unwrap();
    let host = CliHost {
      cfg: probe_cfg(["alpha", "beta"]),
      registry: Arc::new(registry),
      repo_root: PathBuf::from("/synthetic/repo"),
      base_env: charliebot_agents::agent_environment::AgentBaseEnvironment { values: Env::new() },
      pool: charliebot_agents::backend_pool::BackendPool::default(),
    };

    let probe = run_light_probe(&host, "prompt", "system").await.unwrap();

    assert_eq!(
      probe.attempts,
      [("alpha".to_string(), "RuntimeError: ".to_string()), ("beta".to_string(), "RuntimeError: kaput".to_string())]
    );
    assert_eq!(probe.backend_id, None);
    assert_eq!(probe.answer, None);
  }
}

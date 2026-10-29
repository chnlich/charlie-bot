//! The cold-read run's shared contract: the prompt texts, the probe input, and the report body
//! (`src/runtime/cold_read.py`).
//!
//! `prompts/cold_read.md` is the only home of the cold-reader instructions and the seven questions. The artifact check
//! and the cold-read child session both build from here, so the foreground output and the completion report carry the
//! same lines.

use std::path::Path;
use std::sync::Arc;

use anyhow::bail;
use charliebot_agents::backend_pool::is_quota_error;
use charliebot_agents::cli_host::CliHost;
use charliebot_agents::{backend_types, one_shot_text};
use charliebot_core::pyfmt::{self, JsonStyle};
use charliebot_pyhtml::{EventKind, folds_at_open, parse_with};
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

/// The cold-read run's whole input: the page text, the VISIBLE TEXT block, then the questions with the trigger
/// filled in.
pub fn probe_input(page_text: &str, questions: &str, trigger: &str) -> anyhow::Result<String> {
  Ok(format!(
    "{page_text}\n\n{VISIBLE_TEXT_HEADER}\n{}\n\n{}",
    visible_text(page_text)?,
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

/// The ask probe's whole input: the item JSON, the host-served page text and its VISIBLE TEXT block when there is a
/// page, then the questions (`ask_probe_input`), joined by blank lines.
pub fn ask_probe_input(item: &Value, linked_page: Option<&str>, questions: &str) -> anyhow::Result<String> {
  let mut pieces = vec![pyfmt::json_dumps(item, &ITEM_JSON)];
  if let Some(page) = linked_page {
    pieces.push(page.to_string());
    pieces.push(format!("{VISIBLE_TEXT_HEADER}\n{}", visible_text(page)?));
  }
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
  fn ask_probe_input_carries_the_linked_pages_visible_text_block() {
    let item = serde_json::json!({"text": "Choose."});
    let input = ask_probe_input(&item, Some(FOLDED_PAGE), "questions").unwrap();
    let (source, rest) = input.split_once(VISIBLE_TEXT_HEADER).expect("the ask probe carries the visible text block");
    assert!(source.contains("Folded body."), "{source}");
    assert!(!rest.contains("Folded body."), "{rest}");
    let without_page = ask_probe_input(&item, None, "questions").unwrap();
    assert!(!without_page.contains(VISIBLE_TEXT_HEADER), "{without_page}");
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

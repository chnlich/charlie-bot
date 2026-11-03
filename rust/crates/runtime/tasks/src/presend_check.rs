//! The pre-send check on parent-to-child agent messages: the model flags passages the child does
//! not need before the message is admitted, and the sender deletes them or keeps one with a
//! recorded reason.
//!
//! `prompts/presend_check.md` is the only home of the check's instructions. The admission gate
//! [`gate`] runs from `Dispatcher::admit_input`, so both agent message routes pass it. The
//! context builder and the model check are separate functions, so the offline replay of a
//! historical message calls the same code with the context built as of that message's event.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::backend_types;
use charliebot_agents::one_shot_text;
use charliebot_core::event_types;
use charliebot_core::repo_root::RepoRoot;
use charliebot_core::str_methods::take_chars;
use charliebot_registry::backend::LaunchArgs;
use charliebot_registry::context::FeatureContext;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::{TaskSpec, UtcDateTime};
use charliebot_store::{SharedChatEvent, StoreError, StoreResult};
use serde_json::{Value, json};

use crate::autonamer;
use crate::spawner_prompt;

/// The check's instruction template under `prompts/`.
pub const PROMPT_FILENAME: &str = "presend_check.md";
const SYSTEM_SECTION: &str = "system";
const JUDGE_SECTION: &str = "judge";

/// The whole check's time limit; a check that exceeds it delivers nothing.
pub const CHECK_TIME_LIMIT: Duration = Duration::from_secs(120);

/// The passage kinds the check flags. `prompts/presend_check.md` states each kind's criteria.
pub const FLAG_KINDS: &[&str] = &["repeat", "acknowledgement", "other_line", "background", "other"];

/// Context bounds, so one send cannot push an unbounded prompt at the light backends.
const MAX_CHECKED_FILES: usize = 8;
const MAX_FILE_BYTES: usize = 20_000;
const MAX_EARLIER_MESSAGES: usize = 20;
const MAX_EARLIER_MESSAGE_CHARS: usize = 1_500;
const MAX_CHILD_REPORTS: usize = 10;
const MAX_CHILD_REPORT_CHARS: usize = 800;
/// The rendered refusal truncates a quote at this length; the event record keeps the full quote.
const REFUSAL_QUOTE_CHARS: usize = 80;

/// The heading that opens the passages block in the check's prompt.
pub const PASSAGES_HEADING: &str = "PASSAGES TO JUDGE";

/// One `--keep` argument: the flag number of the last refused check and the sender's reason.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct KeepEntry {
  pub n: u32,
  pub reason: String,
}

impl KeepEntry {
  /// Parse one entry of the admitted `keep` field: an object with an integer `n` and a string `reason`.
  pub fn from_value(value: &Value) -> StoreResult<KeepEntry> {
    let object =
      value.as_object().ok_or_else(|| StoreError::invalid("a keep entry must be an object with n and reason"))?;
    let n = object
      .get("n")
      .and_then(Value::as_u64)
      .filter(|n| *n >= 1 && *n <= u32::MAX as u64)
      .ok_or_else(|| StoreError::invalid("a keep entry's n must be a flag number of the last refused check"))?;
    let reason = object
      .get("reason")
      .and_then(Value::as_str)
      .filter(|reason| !reason.is_empty())
      .ok_or_else(|| StoreError::invalid("a keep entry's reason must be a non-empty string"))?;
    Ok(KeepEntry { n: n as u32, reason: reason.to_string() })
  }
}

/// One passage the check flags: its number in the check, its kind, the quoted passage, and the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flag {
  pub n: u32,
  pub kind: String,
  pub quote: String,
  pub reason: String,
}

impl Flag {
  fn to_value(&self) -> Value {
    json!({"n": self.n, "kind": self.kind, "quote": self.quote, "reason": self.reason})
  }
}

/// The refusal the server holds for one parent-child pair until the pair's next delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRefusal {
  pub check_id: String,
  pub backend: String,
  pub body: String,
  pub flags: Vec<Flag>,
}

/// The refused bodies and flags, one per parent-child pair, held until the pair's next delivery.
#[derive(Default)]
pub struct RefusalMemory(Mutex<HashMap<(String, String), HeldRefusal>>);

impl RefusalMemory {
  fn get(&self, parent_id: &str, child_id: &str) -> Option<HeldRefusal> {
    self
      .0
      .lock()
      .expect("the refusal memory lock is not poisoned")
      .get(&(parent_id.to_string(), child_id.to_string()))
      .cloned()
  }

  fn insert(&self, parent_id: &str, child_id: &str, held: HeldRefusal) {
    self
      .0
      .lock()
      .expect("the refusal memory lock is not poisoned")
      .insert((parent_id.to_string(), child_id.to_string()), held);
  }

  fn clear(&self, parent_id: &str, child_id: &str) {
    self
      .0
      .lock()
      .expect("the refusal memory lock is not poisoned")
      .remove(&(parent_id.to_string(), child_id.to_string()));
  }
}

/// The context one check reads: the child's task record, the earlier parent-to-child messages, the
/// child's reports since then, and the local files the message points to.
#[derive(Clone, Debug, PartialEq)]
pub struct PresendContext {
  pub task_record: Value,
  pub earlier_messages: Vec<String>,
  pub child_reports: Vec<String>,
  pub files: Vec<(String, String)>,
}

/// The handles the admission gate reads.
pub struct GateDeps<'a> {
  pub features: &'a Arc<FeatureContext>,
  pub memory: &'a RefusalMemory,
}

/// The admission gate behind both agent message routes. In scope — an `agent_message` whose
/// recipient's task parent is the sending session — it runs the check, refuses with the flagged
/// passages, or hands back the `presend_check` record the admitted event carries. Out of scope it
/// answers `None` and the message is admitted as today, unchecked.
pub async fn gate(
  deps: GateDeps<'_>,
  child_id: &str,
  content: &str,
  from_session: Option<&str>,
  keep: Option<&[KeepEntry]>,
  request_id: Option<&str>,
) -> StoreResult<Option<Value>> {
  let Some(parent_id) = from_session else { return Ok(None) };
  let Some(meta) = deps.features.store.get_session(child_id).await? else { return Ok(None) };
  if meta.task_parent_id.as_deref() != Some(parent_id) {
    return Ok(None);
  }
  // A send whose derived request id already admitted replays below; a delivered message never
  // re-runs the check.
  if let Some(request_id) = request_id {
    let stable_id = charliebot_store::control_events::stable_request_event_id(child_id, request_id);
    let replayed = deps
      .features
      .store
      .load_all_facts_view_sync(child_id)?
      .iter()
      .any(|event| event.get("id").and_then(CompactValue::as_str) == Some(stable_id.as_str()));
    if replayed {
      return Ok(None);
    }
  }

  let keep = keep.unwrap_or(&[]);
  let held = deps.memory.get(parent_id, child_id);
  if !keep.is_empty() {
    validate_keep(keep, held.as_ref())?;
  }

  // The passages the model still has to judge: the whole message on a first send, and on a re-send
  // only the parts its held refusal has not already judged. A kept passage stays flagged in the
  // delivered event's record, so the audit shows what the check flagged in the delivered body.
  let (passages, kept, reused_flags) = match &held {
    Some(held) => {
      let kept_entries: Vec<(&KeepEntry, &Flag)> = keep
        .iter()
        .filter_map(|entry| {
          let flag = &held.flags[(entry.n - 1) as usize];
          content.contains(&flag.quote).then_some((entry, flag))
        })
        .collect();
      let kept: Vec<Value> =
        kept_entries.iter().map(|(entry, _)| json!({"n": entry.n, "reason": entry.reason})).collect();
      let reused: Vec<Flag> = kept_entries.iter().map(|(_, flag)| (*flag).clone()).collect();
      let kept_quotes: Vec<&str> = reused.iter().map(|flag| flag.quote.as_str()).collect();
      let fresh = new_passages(content, held, &kept_quotes);
      if fresh.is_empty() {
        deps.memory.clear(parent_id, child_id);
        tracing::info!(
          target: "charliebot_tasks",
          parent = %parent_id,
          child = %child_id,
          check_id = %held.check_id,
          kept_count = kept.len(),
          "presend_check_delivered_by_reuse"
        );
        return Ok(Some(delivered_record(&held.check_id, &held.backend, &reused, &kept)));
      }
      (fresh.join("\n\n-----\n\n"), kept, reused)
    }
    None => (content.to_string(), Vec::new(), Vec::new()),
  };

  let check_id = new_check_id(parent_id, child_id, content);
  let deadline = std::time::Instant::now() + CHECK_TIME_LIMIT;
  let checked = tokio::time::timeout(
    CHECK_TIME_LIMIT,
    run_check(deps.features, parent_id, child_id, meta.task.as_ref(), &passages, deadline),
  )
  .await;
  let (backend, flags) = match checked {
    Err(_) => {
      return Err(StoreError::new(format!(
        "presend check {check_id} exceeded {}s; nothing was delivered",
        CHECK_TIME_LIMIT.as_secs()
      )));
    }
    Ok(Err(error)) => {
      return Err(StoreError::new(format!("presend check {check_id} failed; nothing was delivered: {error:#}")));
    }
    Ok(Ok((backend, flags))) => (backend, flags),
  };
  let flags: Vec<Flag> = flags
    .into_iter()
    .enumerate()
    .map(|(index, mut flag)| {
      flag.n = index as u32 + 1;
      flag
    })
    .collect();
  if flags.is_empty() {
    deps.memory.clear(parent_id, child_id);
    tracing::info!(target: "charliebot_tasks", parent = %parent_id, child = %child_id, check_id = %check_id, backend = %backend, "presend_check_passed");
    return Ok(Some(delivered_record(&check_id, &backend, &reused_flags, &kept)));
  }
  deps.memory.insert(
    parent_id,
    child_id,
    HeldRefusal { check_id: check_id.clone(), backend, body: content.to_string(), flags: flags.clone() },
  );
  tracing::info!(target: "charliebot_tasks", parent = %parent_id, child = %child_id, check_id = %check_id, flag_count = flags.len(), "presend_check_refused");
  Err(StoreError::refused(render_refusal(&check_id, &flags)))
}

/// The `presend_check` record one delivered agent_message event carries.
fn delivered_record(check_id: &str, backend: &str, flags: &[Flag], kept: &[Value]) -> Value {
  json!({
    "check_id": check_id,
    "backend": backend,
    "flags": flags.iter().map(Flag::to_value).collect::<Vec<_>>(),
    "kept": kept,
  })
}

/// A `--keep` entry without a held refusal, or naming a flag the refusal does not carry, is a
/// sender mistake: the numbers refer to that refused check.
fn validate_keep(keep: &[KeepEntry], held: Option<&HeldRefusal>) -> StoreResult<()> {
  let Some(held) = held else {
    return Err(StoreError::invalid(
      "--keep names a flag of the last refused check, but no refused check is held for this parent-child pair; send the message without --keep",
    ));
  };
  for entry in keep {
    if entry.n as usize > held.flags.len() {
      return Err(StoreError::invalid(format!(
        "--keep {} names no flag of refused check {}, which carries {} flag(s)",
        entry.n,
        held.check_id,
        held.flags.len()
      )));
    }
  }
  Ok(())
}

/// `pc-` and the first six hex digits of the check's identity hash.
fn new_check_id(parent_id: &str, child_id: &str, content: &str) -> String {
  let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
  let digest = charliebot_store::control_events::sha256_hex(&format!("{parent_id}\0{child_id}\0{content}\0{nanos}"));
  format!("pc-{}", &digest[..6])
}

/// The rendered refusal: each flagged passage with its kind and reason, then how to delete and how
/// to keep one. The text alone has to carry the sender through the re-send.
pub fn render_refusal(check_id: &str, flags: &[Flag]) -> String {
  let mut text =
    format!("Refused: presend check {check_id} flagged {} passage(s) the child session does not need.\n", flags.len());
  for flag in flags {
    text.push_str(&format!(
      "\n[{}] {} · \"{}\"\n    reason: {}",
      flag.n,
      flag.kind,
      take_chars(flag.quote.trim(), REFUSAL_QUOTE_CHARS),
      flag.reason
    ));
  }
  text.push_str(
    "\n\nDelete the flagged passages and send the message again. To keep one passage, send the \
     message unchanged and add --keep <n>=\"your reason\" with the flag's number. The next send \
     reuses this check for the passages it already judged.",
  );
  text
}

/// The segments of `body` the check has already judged and not flagged: the body with every
/// occurrence of each flagged quote cut out.
pub fn approved_segments(body: &str, flags: &[Flag]) -> Vec<String> {
  let mut covered = vec![false; body.len()];
  for flag in flags {
    mark_occurrences(body, &flag.quote, &mut covered);
  }
  let mut segments = Vec::new();
  let mut start = None;
  for (index, covered_byte) in covered.iter().chain(std::iter::once(&true)).enumerate() {
    if !*covered_byte && start.is_none() {
      start = Some(index);
    }
    if *covered_byte && let Some(run_start) = start.take() {
      segments.push(body[run_start..index].to_string());
    }
  }
  segments
}

/// Mark every occurrence of `needle` in `body` covered.
fn mark_occurrences(body: &str, needle: &str, covered: &mut [bool]) {
  if needle.is_empty() {
    return;
  }
  let mut position = 0;
  while let Some(found) = body[position..].find(needle) {
    let start = position + found;
    for covered_byte in &mut covered[start..start + needle.len()] {
      *covered_byte = true;
    }
    position = start + needle.len();
  }
}

/// The maximal covered-but-uncovered runs of `body`: the passages the held refusal has not judged.
/// A kept flagged quote counts as judged, so a `--keep` re-send delivers without a model call.
pub fn new_passages(body: &str, held: &HeldRefusal, kept_quotes: &[&str]) -> Vec<String> {
  let mut covered = vec![false; body.len()];
  for segment in approved_segments(&held.body, &held.flags) {
    mark_occurrences(body, &segment, &mut covered);
  }
  for quote in kept_quotes {
    mark_occurrences(body, quote, &mut covered);
  }
  let mut passages = Vec::new();
  let mut start = None;
  for (index, covered_byte) in covered.iter().chain(std::iter::once(&true)).enumerate() {
    if !*covered_byte && start.is_none() {
      start = Some(index);
    }
    if *covered_byte && let Some(run_start) = start.take() {
      push_passage(body, run_start, index, &mut passages);
    }
  }
  passages
}

/// Trim one uncovered run and keep it when text remains.
fn push_passage(body: &str, start: usize, end: usize, passages: &mut Vec<String>) {
  let run = &body[start..end];
  let trimmed = run.trim();
  if !trimmed.is_empty() {
    passages.push(trimmed.to_string());
  }
}

/// The context part of the check's input: the context sections, then the passages to judge inside a
/// fence the quotes come from verbatim. [`check_prompt`] puts the judge instructions before it.
pub fn probe_input(context: &PresendContext, passages: &str) -> String {
  let earlier = if context.earlier_messages.is_empty() {
    "(none)".to_string()
  } else {
    context
      .earlier_messages
      .iter()
      .enumerate()
      .map(|(index, message)| format!("[{}] {}", index + 1, message))
      .collect::<Vec<_>>()
      .join("\n")
  };
  let reports = if context.child_reports.is_empty() {
    "(none)".to_string()
  } else {
    context
      .child_reports
      .iter()
      .enumerate()
      .map(|(index, report)| format!("[{}] {}", index + 1, report))
      .collect::<Vec<_>>()
      .join("\n")
  };
  let files = if context.files.is_empty() {
    "(none)".to_string()
  } else {
    context.files.iter().map(|(path, text)| format!("--- {path}\n{text}")).collect::<Vec<_>>().join("\n")
  };
  [
    format!("CHILD TASK RECORD:\n{}", context.task_record),
    format!("EARLIER MESSAGES from this parent to the child (oldest first):\n{earlier}"),
    format!("CHILD REPORTS since the last earlier message:\n{reports}"),
    format!("LOCAL FILES the message points to:\n{files}"),
    format!("{PASSAGES_HEADING} (quote verbatim from this text):\n<<<PASSAGES\n{passages}\nPASSAGES>>>"),
  ]
  .join("\n\n")
}

/// The one prompt the check reads: `prompts/presend_check.md`'s judge instructions, then the
/// context sections and the passages fence the quotes come from.
pub fn check_prompt(judge: &str, context: &PresendContext, passages: &str) -> String {
  format!("{}\n\n{}", judge.trim_end_matches('\n'), probe_input(context, passages))
}

/// Parse the model's answer into flags. An answer that is not a JSON array fails the check; an
/// entry whose kind is unknown, whose quote is not verbatim from the passages, or whose reason is
/// empty is dropped, because a dropped flag cannot match a passage on the re-send.
pub fn parse_flags(answer: &str, passages: &str) -> anyhow::Result<Vec<Flag>> {
  let stripped = autonamer::strip_markdown_fences(answer.trim());
  let value: Value = serde_json::from_str(stripped.trim())
    .map_err(|error| anyhow::anyhow!("the presend check answer is not valid JSON: {error}"))?;
  let items = value.as_array().ok_or_else(|| anyhow::anyhow!("the presend check answer is not a JSON array"))?;
  let mut flags = Vec::new();
  for item in items {
    let kind = item.get("kind").and_then(Value::as_str).unwrap_or_default();
    let Some(quote) = item.get("quote").and_then(Value::as_str).map(str::trim).filter(|quote| !quote.is_empty()) else {
      tracing::warn!(target: "charliebot_tasks", error = "flag without a quote", "presend_flag_dropped");
      continue;
    };
    let Some(reason) = item.get("reason").and_then(Value::as_str).map(str::trim).filter(|reason| !reason.is_empty())
    else {
      tracing::warn!(target: "charliebot_tasks", error = "flag without a reason", "presend_flag_dropped");
      continue;
    };
    if !FLAG_KINDS.contains(&kind) {
      tracing::warn!(target: "charliebot_tasks", kind, "presend_flag_dropped");
      continue;
    }
    if !passages.contains(quote) {
      tracing::warn!(target: "charliebot_tasks", error = "quote is not verbatim from the passages", "presend_flag_dropped");
      continue;
    }
    flags.push(Flag { n: 0, kind: kind.to_string(), quote: quote.to_string(), reason: reason.to_string() });
  }
  Ok(flags)
}

/// The model check over one batch of passages: the light backends in pool rotation, each answer
/// parsed against the passages. Each call gets the budget left until `deadline`, so a hung backend
/// cannot outlive the check's limit. Fails when every backend fails or answers off shape.
pub async fn run_check(
  features: &Arc<FeatureContext>,
  parent_id: &str,
  child_id: &str,
  task_record: Option<&TaskSpec>,
  passages: &str,
  deadline: std::time::Instant,
) -> anyhow::Result<(String, Vec<Flag>)> {
  let cfg = features.config.get().context("reading config.yaml")?;
  let pool = features.startup.require::<BackendPool>()?;
  let base_env = features.startup.require::<AgentBaseEnvironment>()?;
  let repo_root = features.startup.require::<RepoRoot>()?;
  let sections = spawner_prompt::load_marker_sections(
    &repo_root.path().join("prompts").join(PROMPT_FILENAME),
    &[SYSTEM_SECTION, JUDGE_SECTION],
    "presend-check",
  )?;
  let system_prompt = sections[SYSTEM_SECTION].trim_end_matches('\n').to_string();
  let judge_prompt = sections[JUDGE_SECTION].trim_end_matches('\n').to_string();

  // The context reads are synchronous store and file reads; they run off the async runtime.
  let store = Arc::clone(&features.store);
  let parent = parent_id.to_string();
  let child = child_id.to_string();
  let message = passages.to_string();
  let record = task_record.cloned();
  let context = tokio::task::spawn_blocking(move || -> anyhow::Result<PresendContext> {
    let child_events = store.load_chat_events_shared_sync(&child)?;
    let parent_events = store.load_chat_events_shared_sync(&parent)?;
    Ok(context_from_events(record.as_ref(), &child_events, &parent_events, &parent, &child, &message, None))
  })
  .await
  .context("joining the presend context reader")??;
  let prompt = check_prompt(&judge_prompt, &context, passages);

  let mut last_error = None;
  for option in autonamer::iter_light_backends(&cfg, Arc::clone(&pool)) {
    let option = option?;
    let backend_id = option.id.clone();
    let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()).filter(|left| !left.is_zero())
    else {
      bail!("the presend check exceeded its {}s limit", CHECK_TIME_LIMIT.as_secs());
    };
    let launch = LaunchArgs { cgroup_session_id: Some(parent_id.to_string()), ..LaunchArgs::default() };
    let outcome = match backend_types::build_backend(Arc::clone(&features.registry), &option, &cfg, launch) {
      Ok(built) => {
        ("RuntimeError", one_shot_text(built, &base_env.values, &prompt, &system_prompt, Some(remaining)).await)
      }
      Err(error) => ("ValueError", Err(error)),
    };
    match outcome {
      (_, Ok(answer)) => match parse_flags(&answer, passages) {
        Ok(flags) => return Ok((backend_id, flags)),
        Err(error) => last_error = Some(error.to_string()),
      },
      (error_type, Err(error)) => {
        let message = format!("{error:#}");
        if charliebot_agents::backend_pool::is_quota_error(&message) {
          pool.cool(&backend_id, None);
        }
        last_error = Some(format!("{error_type}: {message}"));
      }
    }
  }
  bail!(
    "every light backend failed the presend check: {}",
    last_error.unwrap_or_else(|| "no light backend is configured".to_string())
  )
}

/// The context one check reads, built as of `as_of_event_id`: the events up to that event id, so an
/// offline replay of a historical message passes the historical message's event id and reads the
/// same context the live check read. `None` keeps every event (the live case, where the message
/// under check is not in the log yet).
pub fn context_from_events(
  task_record: Option<&TaskSpec>,
  child_events: &[SharedChatEvent],
  parent_events: &[SharedChatEvent],
  parent_id: &str,
  child_id: &str,
  message: &str,
  as_of_event_id: Option<&str>,
) -> PresendContext {
  let as_of_time = as_of_timestamp(child_events, parent_events, as_of_event_id);
  let child_events = events_as_of(child_events, as_of_event_id, as_of_time);
  let parent_events = events_as_of(parent_events, as_of_event_id, as_of_time);
  let earlier_messages: Vec<(&str, String)> = child_events
    .iter()
    .filter(|event| {
      event.get("type").and_then(CompactValue::as_str) == Some(event_types::AGENT_MESSAGE)
        && event.get("from_session").and_then(CompactValue::as_str) == Some(parent_id)
    })
    .map(|event| {
      (
        event.get("timestamp").and_then(CompactValue::as_str).unwrap_or_default(),
        event.get("content").and_then(CompactValue::as_str).unwrap_or_default().to_string(),
      )
    })
    .collect();
  // The child's reports since the parent's last message: the fact the week-one review's re-judging
  // was missing. Without an earlier message nothing is "since then".
  let cutoff = earlier_messages.last().and_then(|(timestamp, _)| UtcDateTime::parse(timestamp).ok());
  let child_reports: Vec<String> = parent_events
    .iter()
    .filter(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::CHILD_REPORT))
    .filter(|event| event.get("child_session_id").and_then(CompactValue::as_str) == Some(child_id))
    .filter(|event| {
      cutoff.is_some_and(|cutoff| {
        event
          .get("timestamp")
          .and_then(CompactValue::as_str)
          .and_then(|ts| UtcDateTime::parse(ts).ok())
          .is_some_and(|ts| ts > cutoff)
      })
    })
    .map(|event| event.get("summary").and_then(CompactValue::as_str).unwrap_or_default().to_string())
    .collect();
  PresendContext {
    task_record: task_record
      .map(|task| serde_json::to_value(task).expect("a task record serializes"))
      .unwrap_or(Value::Null),
    earlier_messages: earlier_messages
      .iter()
      .rev()
      .take(MAX_EARLIER_MESSAGES)
      .rev()
      .map(|(_, message)| take_chars(message, MAX_EARLIER_MESSAGE_CHARS))
      .collect(),
    child_reports: child_reports
      .iter()
      .rev()
      .take(MAX_CHILD_REPORTS)
      .rev()
      .map(|report| take_chars(report, MAX_CHILD_REPORT_CHARS))
      .collect(),
    files: read_pointed_files(message),
  }
}

/// The timestamp of the event with `as_of_event_id`, so the log that does not hold the id is cut at
/// the same moment.
fn as_of_timestamp(
  child_events: &[SharedChatEvent],
  parent_events: &[SharedChatEvent],
  as_of_event_id: Option<&str>,
) -> Option<UtcDateTime> {
  let as_of_event_id = as_of_event_id?;
  child_events
    .iter()
    .chain(parent_events.iter())
    .find(|event| event.get("id").and_then(CompactValue::as_str) == Some(as_of_event_id))
    .and_then(|event| event.get("timestamp").and_then(CompactValue::as_str))
    .and_then(|timestamp| UtcDateTime::parse(timestamp).ok())
}

/// The events strictly before the one with `as_of_event_id`: the events before it in the log that
/// holds it, and, in the other log, the events stamped before its time. Every event when the id is
/// absent from both logs.
fn events_as_of<'a>(
  events: &'a [SharedChatEvent],
  as_of_event_id: Option<&str>,
  as_of_time: Option<UtcDateTime>,
) -> Vec<&'a SharedChatEvent> {
  if let Some(as_of_event_id) = as_of_event_id
    && let Some(position) =
      events.iter().position(|event| event.get("id").and_then(CompactValue::as_str) == Some(as_of_event_id))
  {
    return events[..position].iter().collect();
  }
  match as_of_time {
    Some(as_of_time) => events
      .iter()
      .filter(|event| {
        event
          .get("timestamp")
          .and_then(CompactValue::as_str)
          .and_then(|timestamp| UtcDateTime::parse(timestamp).ok())
          .is_some_and(|timestamp| timestamp < as_of_time)
      })
      .collect(),
    None => events.iter().collect(),
  }
}

/// The local files the message points to: the `~/` and absolute paths it names that exist as files,
/// read with each file's leading bytes, in the order the message names them.
fn read_pointed_files(message: &str) -> Vec<(String, String)> {
  static PATH_PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
  let pattern =
    PATH_PATTERN.get_or_init(|| regex::Regex::new(r"(?:~/|/)[A-Za-z0-9_\-./]+").expect("the path pattern is valid"));
  let mut seen = HashSet::new();
  let mut files = Vec::new();
  for found in pattern.find_iter(message) {
    if files.len() == MAX_CHECKED_FILES {
      break;
    }
    let mut path = PathBuf::from(found.as_str().trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '"', '\'']));
    if let Ok(home) = std::env::var("HOME")
      && let Ok(stripped) = path.strip_prefix("~")
    {
      path = Path::new(&home).join(stripped);
    }
    if path.is_file() && seen.insert(path.clone()) {
      match std::fs::read(&path) {
        Ok(bytes) => files.push((
          path.display().to_string(),
          String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_FILE_BYTES)]).to_string(),
        )),
        Err(error) => {
          tracing::warn!(target: "charliebot_tasks", path = %path.display(), error = %error, "presend_file_unreadable")
        }
      }
    }
  }
  files
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::sync::Arc;

  use charliebot_agents::backend_pool::BackendPool;
  use charliebot_registry::Registry;
  use charliebot_registry::backend::Env;
  use charliebot_store::metadata::{TaskProfile, TaskSpec};
  use charliebot_store::{CreateTask, TaskCaller};
  use serde_json::{Map, Value, json};

  use super::*;

  const FLAGGED: &str = "HostKey、PidFile、UsePAM 三项 23:43 已发过";
  const ANSWER_ONE_FLAG: &str = r#"[{"kind":"repeat","quote":"HostKey、PidFile、UsePAM 三项 23:43 已发过","reason":"the 23:43 message sent these three items"}]"#;

  // --- pure-function tests -------------------------------------------------

  #[test]
  fn approved_segments_cut_every_flagged_quote_out_of_the_body() {
    let body = format!("keep a\n{FLAGGED}\nkeep b\n{FLAGGED} keep c");
    let flags = vec![
      Flag { n: 1, kind: "repeat".into(), quote: FLAGGED.into(), reason: "r".into() },
      Flag { n: 2, kind: "other".into(), quote: "keep b".into(), reason: "r".into() },
    ];
    let segments = approved_segments(&body, &flags);
    assert_eq!(segments, vec!["keep a\n".to_string(), "\n".to_string(), "\n".to_string(), " keep c".to_string()]);
  }

  #[test]
  fn a_resend_without_the_flagged_quotes_and_keeps_has_no_new_passages() {
    let body = format!("first line\n{FLAGGED}\nlast line");
    let held = HeldRefusal {
      check_id: "pc-aaaaaa".into(),
      backend: "stub".into(),
      body: body.clone(),
      flags: vec![Flag { n: 1, kind: "repeat".into(), quote: FLAGGED.into(), reason: "r".into() }],
    };
    let deleted = "first line\n\nlast line";
    assert_eq!(new_passages(deleted, &held, &[]), Vec::<String>::new());
    // Keeping the flagged passage with a reason also counts as judged.
    assert_eq!(new_passages(&body, &held, &[FLAGGED]), Vec::<String>::new());
    // The same passage without --keep is new again.
    assert_eq!(new_passages(&body, &held, &[]), vec![FLAGGED.to_string()]);
    // A partial deletion of the quote is new too.
    let partial = &FLAGGED[..FLAGGED.len() / 2];
    let partial_body = format!("first line\n{partial}\nlast line");
    assert_eq!(new_passages(&partial_body, &held, &[]), vec![partial.to_string()]);
    // Added text next to the deleted flagged passage is the only new passage.
    let extended = "first line\n\nlast line\nnew news";
    assert_eq!(new_passages(extended, &held, &[]), vec!["new news".to_string()]);
  }

  #[test]
  fn parse_flags_accepts_the_contract_shape_and_drops_unusable_entries() {
    let passages = "one two three";
    let answer = concat!(
      "```json\n",
      r#"[{"kind":"repeat","quote":"one two","reason":"sent before"},"#,
      r#"{"kind":"nope","quote":"three","reason":"x"},"#,
      r#"{"kind":"other","quote":"missing from passages","reason":"x"},"#,
      r#"{"kind":"background","quote":"three","reason":""},"#,
      r#"{"kind":"other","reason":"no quote"}]"#,
      "\n```"
    );
    let flags = parse_flags(answer, passages).unwrap();
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0].kind, "repeat");
    assert_eq!(flags[0].quote, "one two");
    assert!(parse_flags("no json here", passages).is_err());
    assert!(parse_flags("{\"kind\":\"other\"}", passages).is_err());
  }

  #[test]
  fn the_refusal_lists_every_flag_with_its_number_kind_quote_and_reason() {
    let flags = vec![
      Flag { n: 1, kind: "repeat".into(), quote: FLAGGED.into(), reason: "the 23:43 message sent these".into() },
      Flag {
        n: 2,
        kind: "other_line".into(),
        quote: "another cluster waits".into(),
        reason: "the child does not wait on it".into(),
      },
    ];
    let text = render_refusal("pc-3f9a1c", &flags);
    assert!(text.starts_with("Refused: presend check pc-3f9a1c flagged 2 passage(s)"), "{text}");
    assert!(text.contains("[1] repeat · "), "{text}");
    assert!(text.contains("[2] other_line · "), "{text}");
    assert!(text.contains("reason: the 23:43 message sent these"), "{text}");
    assert!(text.contains("--keep <n>=\"your reason\""), "{text}");
    assert!(text.contains("Delete the flagged passages"), "{text}");
  }

  fn child_event(id: &str, from: &str, content: &str, timestamp: &str) -> SharedChatEvent {
    Arc::new(json!({"id": id, "type": event_types::AGENT_MESSAGE, "from_session": from, "content": content, "timestamp": timestamp}).into())
  }

  fn report_event(id: &str, child: &str, summary: &str, timestamp: &str) -> SharedChatEvent {
    Arc::new(
      json!({"id": id, "type": event_types::CHILD_REPORT, "child_session_id": child, "summary": summary, "timestamp": timestamp})
        .into(),
    )
  }

  #[test]
  fn the_context_holds_the_task_record_earlier_messages_reports_and_files() {
    let parent = "parent-1";
    let child = "child-1";
    let child_events = vec![
      child_event("m1", parent, "earlier one", "2026-10-19T22:00:00Z"),
      child_event("m2", "someone-else", "not from the parent", "2026-10-19T22:30:00Z"),
      child_event("m3", parent, "earlier two", "2026-10-19T23:00:00Z"),
    ];
    let parent_events = vec![
      report_event("r1", child, "before the last message", "2026-10-19T22:30:00Z"),
      report_event("r2", child, "since the last message", "2026-10-19T23:30:00Z"),
      report_event("r3", "child-2", "another child's report", "2026-10-19T23:40:00Z"),
    ];
    let task = TaskSpec { goal: "install sshd".into(), ..TaskSpec::default() };
    let context =
      context_from_events(Some(&task), &child_events, &parent_events, parent, child, "see /etc/ssh/sshd_config", None);
    assert_eq!(context.task_record, json!(task));
    assert_eq!(context.earlier_messages, vec!["earlier one".to_string(), "earlier two".to_string()]);
    assert_eq!(context.child_reports, vec!["since the last message".to_string()]);
    assert_eq!(context.files.len(), 1);
    assert_eq!(context.files[0].0, "/etc/ssh/sshd_config");
  }

  #[test]
  fn the_context_truncates_at_the_as_of_event() {
    let parent = "parent-1";
    let child = "child-1";
    let child_events = vec![
      child_event("m1", parent, "earlier one", "2026-10-19T22:00:00Z"),
      child_event("m2", parent, "the message under replay", "2026-10-19T23:00:00Z"),
      child_event("m3", parent, "sent after it", "2026-10-19T23:10:00Z"),
    ];
    let parent_events = vec![
      report_event("r1", child, "before the replayed message", "2026-10-19T22:30:00Z"),
      report_event("r2", child, "after the replayed message", "2026-10-19T23:30:00Z"),
    ];
    let context = context_from_events(None, &child_events, &parent_events, parent, child, "m", Some("m2"));
    assert_eq!(context.earlier_messages, vec!["earlier one".to_string()]);
    // The parent's log holds no event with the child's id, so it is cut at that event's time: a
    // report sent after the replayed message is not part of the context the live check read.
    assert_eq!(context.child_reports, vec!["before the replayed message".to_string()]);
  }

  #[test]
  fn the_probe_carries_the_context_sections_and_the_passages_fence() {
    let context = PresendContext {
      task_record: json!({"goal": "install sshd"}),
      earlier_messages: vec!["earlier".into()],
      child_reports: vec!["report".into()],
      files: vec![("/etc/hosts".into(), "127.0.0.1 localhost".into())],
    };
    let probe = probe_input(&context, "the passages");
    assert!(probe.contains("CHILD TASK RECORD:\n{\"goal\":\"install sshd\"}"), "{probe}");
    assert!(probe.contains("[1] earlier"), "{probe}");
    assert!(probe.contains("[1] report"), "{probe}");
    assert!(probe.contains("--- /etc/hosts\n127.0.0.1 localhost"), "{probe}");
    let (before, after) = probe.split_once("<<<PASSAGES\n").expect("the passages fence opens");
    assert!(after.starts_with("the passages\nPASSAGES>>>"), "{after}");
    assert!(before.contains("127.0.0.1 localhost"), "{before}");
  }

  #[test]
  fn the_check_prompt_carries_the_judge_instructions_before_the_passages() {
    let context =
      PresendContext { task_record: json!(null), earlier_messages: vec![], child_reports: vec![], files: vec![] };
    let prompt = check_prompt("Flag each passage under one of these kinds.", &context, "the passages");
    assert!(prompt.starts_with("Flag each passage under one of these kinds.\n\nCHILD TASK RECORD:"), "{prompt}");
    assert!(prompt.contains("<<<PASSAGES\nthe passages\nPASSAGES>>>"), "{prompt}");
  }

  // --- gate tests over a real dispatcher -----------------------------------

  /// The feature context over `home` with the stub backend, its pool, and the agent base environment.
  fn stub_features(home: &std::path::Path, config_body: &str) -> Arc<FeatureContext> {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("config.yaml"), config_body).unwrap();
    let mut registry = Registry::new();
    registry.register_package("tests.presend", crate::test_fixtures::register_presend_stub).unwrap();
    let registry = Arc::new(registry);
    let config = registry.load_config(home).unwrap();
    let config_registry = Arc::clone(&registry);
    let config_home = home.to_path_buf();
    let store_config = config.clone();
    let config = Arc::new(charliebot_core::hot_reload::HotReload::new(
      config.config_file(),
      "presend_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let store = Arc::new(
      charliebot_store::Store::new(
        store_config,
        registry.metadata_slots(),
        charliebot_registry::store_hooks::store_hooks(Arc::clone(&registry)),
      )
      .unwrap(),
    );
    let credential_home = home.to_path_buf();
    let credentials = Arc::new(charliebot_core::hot_reload::HotReload::new(
      home.join("credentials.yaml"),
      "presend_test_credentials_reload_failed",
      Box::new(move || charliebot_core::credentials::load_credentials(&credential_home)),
    ));
    let startup = Arc::new(charliebot_registry::context::StartupState::default());
    startup.insert(BackendPool::default()).unwrap();
    startup.insert(charliebot_agents::agent_environment::AgentBaseEnvironment { values: Env::default() }).unwrap();
    let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap().to_path_buf();
    startup.insert(charliebot_core::RepoRoot::new(Some(repo_root.into_os_string())).unwrap()).unwrap();
    Arc::new(FeatureContext { config, credentials, store, registry, startup })
  }

  async fn parent_and_child(
    features: &FeatureContext,
    name: &str,
  ) -> (charliebot_store::metadata::SessionMetadata, charliebot_store::metadata::SessionMetadata) {
    let parent = features
      .store
      .create_task(
        CreateTask {
          request_id: format!("{name}-parent"),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Synthetic parent".into()),
          backend: None,
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    let child = features
      .store
      .create_task(
        CreateTask {
          request_id: format!("{name}-child"),
          task_parent_id: Some(parent.id.clone()),
          profile: TaskProfile::Worker,
          task: Some(TaskSpec { goal: "install sshd on the box".into(), ..TaskSpec::default() }),
          name: Some("Synthetic child".into()),
          backend: None,
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    (
      features.store.get_session(&parent.id).await.unwrap().unwrap(),
      features.store.get_session(&child.id).await.unwrap().unwrap(),
    )
  }

  fn dispatcher(features: &Arc<FeatureContext>) -> crate::dispatch::Dispatcher {
    let completion = Arc::new(crate::completion::TaskCompletion::new(Arc::clone(features)));
    let sequence: Arc<dyn charliebot_registry::sequence::SequenceCompletion> = completion.clone();
    let run: Arc<dyn crate::dispatch::RunFinishedCompletion> = completion;
    crate::dispatch::Dispatcher::new(Arc::clone(&features.store), Arc::clone(features), sequence, run)
  }

  fn agent_fields(parent_id: &str) -> Value {
    json!({"from_session": parent_id})
  }

  /// A real user message on the parent, so the delegation path lets the parent's agent messages through.
  async fn start_user_line(dispatcher: &crate::dispatch::Dispatcher, parent_id: &str) {
    dispatcher
      .admit_input(parent_id, event_types::USER, "start this line", "user", json!({}))
      .await
      .expect("the user line starts");
  }

  fn call_count(calls_file: &std::path::Path) -> usize {
    fs::read_to_string(calls_file).map(|text| text.lines().count()).unwrap_or(0)
  }

  async fn presend_record_of(features: &FeatureContext, child_id: &str) -> Value {
    let events = features.store.load_chat_events_sync(child_id).unwrap();
    events
      .iter()
      .find(|event| {
        event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some(event_types::AGENT_MESSAGE)
      })
      .expect("the child holds the delivered agent message")
      .get("presend_check")
      .map(charliebot_store::compact::CompactValue::to_value)
      .expect("a delivered parent-to-child message carries its presend_check record")
  }

  #[tokio::test]
  async fn a_flagged_parent_message_is_refused_with_each_passage_listed() {
    let home = tempfile::tempdir().unwrap();
    let calls_file = home.path().join("calls.log");
    let features = stub_features(
      &home.path().join("home"),
      &crate::test_fixtures::presend_stub_config(ANSWER_ONE_FLAG, &calls_file),
    );
    let (parent, child) = parent_and_child(&features, "refused").await;
    let dispatcher = dispatcher(&features);
    start_user_line(&dispatcher, &parent.id).await;
    let content = format!("Please go on.\n{FLAGGED}\nAlso the other cluster waits.");

    let error = dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, &content, "agent", agent_fields(&parent.id))
      .await
      .expect_err("the flagged message is refused");
    let detail = error.to_string();
    assert!(detail.starts_with("Refused: presend check pc-"), "{detail}");
    assert!(detail.contains("[1] repeat · "), "{detail}");
    assert!(detail.contains(FLAGGED), "{detail}");
    assert!(detail.contains("reason: the 23:43 message sent these three items"), "{detail}");
    assert_eq!(call_count(&calls_file), 1);

    let events = features.store.load_chat_events_sync(&child.id).unwrap();
    assert!(
      !events.iter().any(|event| event.get("type").and_then(charliebot_store::compact::CompactValue::as_str)
        == Some(event_types::AGENT_MESSAGE)),
      "nothing reached the child: {events:?}"
    );
  }

  #[tokio::test]
  async fn deleting_the_flagged_passages_and_resending_delivers_without_a_new_model_call() {
    let home = tempfile::tempdir().unwrap();
    let calls_file = home.path().join("calls.log");
    let features = stub_features(
      &home.path().join("home"),
      &crate::test_fixtures::presend_stub_config(ANSWER_ONE_FLAG, &calls_file),
    );
    let (parent, child) = parent_and_child(&features, "deleted").await;
    let dispatcher = dispatcher(&features);
    start_user_line(&dispatcher, &parent.id).await;
    let content = format!("Please go on.\n{FLAGGED}\nAlso the other cluster waits.");
    dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, &content, "agent", agent_fields(&parent.id))
      .await
      .expect_err("the first send is refused");
    assert_eq!(call_count(&calls_file), 1);

    let deleted = "Please go on.\n\nAlso the other cluster waits.";
    dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, deleted, "agent", agent_fields(&parent.id))
      .await
      .expect("the deleted re-send delivers");
    assert_eq!(call_count(&calls_file), 1, "the remaining passages reuse the refused check");

    let record = presend_record_of(&features, &child.id).await;
    assert_eq!(record["flags"], json!([]));
    assert_eq!(record["kept"], json!([]));
    assert!(record["check_id"].as_str().unwrap().starts_with("pc-"), "{record}");
    assert_eq!(record["backend"], "stub");
  }

  #[tokio::test]
  async fn keeping_a_flagged_passage_delivers_and_records_the_reason() {
    let home = tempfile::tempdir().unwrap();
    let calls_file = home.path().join("calls.log");
    let features = stub_features(
      &home.path().join("home"),
      &crate::test_fixtures::presend_stub_config(ANSWER_ONE_FLAG, &calls_file),
    );
    let (parent, child) = parent_and_child(&features, "kept").await;
    let dispatcher = dispatcher(&features);
    start_user_line(&dispatcher, &parent.id).await;
    let content = format!("Please go on.\n{FLAGGED}\nAlso the other cluster waits.");
    let error = dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, &content, "agent", agent_fields(&parent.id))
      .await
      .expect_err("the first send is refused");
    let check_id = error.to_string()["Refused: presend check ".len()..].split(' ').next().unwrap().to_string();

    let keep = vec![KeepEntry { n: 1, reason: "please verify the worker instructions carry these three items".into() }];
    dispatcher
      .admit_input(
        &child.id,
        event_types::AGENT_MESSAGE,
        &content,
        "agent",
        json!({"from_session": parent.id, "keep": keep}),
      )
      .await
      .expect("the kept re-send delivers");
    assert_eq!(call_count(&calls_file), 1, "the kept passage reuses the refused check");

    let record = presend_record_of(&features, &child.id).await;
    assert_eq!(record["check_id"], json!(check_id), "the delivery cites the check that judged it");
    assert_eq!(
      record["kept"],
      json!([{"n": 1, "reason": "please verify the worker instructions carry these three items"}])
    );
    assert_eq!(record["flags"].as_array().unwrap().len(), 1, "{record}");
    assert_eq!(record["flags"][0]["n"], json!(1));
    assert_eq!(record["flags"][0]["kind"], "repeat");
  }

  #[tokio::test]
  async fn a_message_with_nothing_flagged_is_delivered_unchanged_and_the_next_message_is_checked_again() {
    let home = tempdir_with_config(r#"[]"#);
    let calls_file = home.calls_file.clone();
    let (parent, child) = parent_and_child(&home.features, "clean").await;
    let dispatcher = dispatcher(&home.features);
    start_user_line(&dispatcher, &parent.id).await;
    let content = "Please install sshd and report back.";

    dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, content, "agent", agent_fields(&parent.id))
      .await
      .expect("delivers");
    let record = presend_record_of(&home.features, &child.id).await;
    assert_eq!(record["flags"], json!([]));
    assert_eq!(call_count(&calls_file), 1);

    dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, "One more thing.", "agent", agent_fields(&parent.id))
      .await
      .expect("delivers");
    assert_eq!(call_count(&calls_file), 2, "a new message after a delivery gets a full check");
  }

  #[tokio::test]
  async fn messages_outside_the_parent_child_direction_are_delivered_unchecked() {
    let home = tempdir_with_config(ANSWER_ONE_FLAG);
    let calls_file = home.calls_file.clone();
    let (parent, child) = parent_and_child(&home.features, "out-of-scope").await;
    let dispatcher = dispatcher(&home.features);

    // A manager-to-manager message is not parent-to-child.
    dispatcher
      .admit_input(
        &parent.id,
        event_types::AGENT_MESSAGE,
        "update for another manager",
        "agent",
        agent_fields(&child.id),
      )
      .await
      .expect("delivers");
    assert_eq!(call_count(&calls_file), 0, "no model call out of scope");

    // A child-to-parent report path admits a child_report event, not an agent message.
    dispatcher.admit_input(&parent.id, event_types::CHILD_REPORT, "done", "agent", json!({})).await.expect("delivers");
    assert_eq!(call_count(&calls_file), 0, "child reports stay unchecked");
  }

  #[tokio::test]
  async fn keep_without_a_refused_check_is_a_client_error() {
    let home = tempdir_with_config(r#"[]"#);
    let (parent, child) = parent_and_child(&home.features, "keep-none").await;
    let dispatcher = dispatcher(&home.features);
    start_user_line(&dispatcher, &parent.id).await;
    let error = dispatcher
      .admit_input(
        &child.id,
        event_types::AGENT_MESSAGE,
        "anything",
        "agent",
        json!({"from_session": parent.id, "keep": [{"n": 1, "reason": "r"}]}),
      )
      .await
      .expect_err("--keep without a refused check is a mistake");
    assert!(matches!(error, charliebot_store::StoreError::Invalid(_)), "{error:?}");
  }

  #[tokio::test]
  async fn keep_numbering_names_only_flags_the_refused_check_carries() {
    let home = tempdir_with_config(ANSWER_ONE_FLAG);
    let (parent, child) = parent_and_child(&home.features, "keep-range").await;
    let dispatcher = dispatcher(&home.features);
    start_user_line(&dispatcher, &parent.id).await;
    let content = format!("go on\n{FLAGGED}");
    dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, &content, "agent", agent_fields(&parent.id))
      .await
      .expect_err("refused first");
    let error = dispatcher
      .admit_input(
        &child.id,
        event_types::AGENT_MESSAGE,
        &content,
        "agent",
        json!({"from_session": parent.id, "keep": [{"n": 2, "reason": "r"}]}),
      )
      .await
      .expect_err("flag 2 does not exist");
    assert!(matches!(error, charliebot_store::StoreError::Invalid(_)), "{error:?}");
  }

  #[tokio::test]
  async fn a_failing_check_delivers_nothing() {
    let home = tempdir_with_config("not a json array at all");
    let (parent, child) = parent_and_child(&home.features, "failing").await;
    let dispatcher = dispatcher(&home.features);
    start_user_line(&dispatcher, &parent.id).await;
    let error = dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, "go on", "agent", agent_fields(&parent.id))
      .await
      .expect_err("the failed check delivers nothing");
    assert!(matches!(error, charliebot_store::StoreError::Failure(_)), "{error:?}");
    let events = home.features.store.load_chat_events_sync(&child.id).unwrap();
    assert!(
      !events.iter().any(|event| event.get("type").and_then(charliebot_store::compact::CompactValue::as_str)
        == Some(event_types::AGENT_MESSAGE)),
      "nothing reached the child: {events:?}"
    );
  }

  #[tokio::test]
  async fn a_refusal_after_a_partially_new_resend_holds_the_new_body_and_flags() {
    let home = tempdir_with_config(ANSWER_ONE_FLAG);
    let calls_file = home.calls_file.clone();
    let (parent, child) = parent_and_child(&home.features, "mixed").await;
    let dispatcher = dispatcher(&home.features);
    start_user_line(&dispatcher, &parent.id).await;
    let content = format!("go on\n{FLAGGED}");
    dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, &content, "agent", agent_fields(&parent.id))
      .await
      .expect_err("refused first");

    // The re-send keeps the flagged passage without --keep, so it is new again, and adds text the
    // stub also flags; the second refusal numbers the flags of its own check.
    let flagged = "the other cluster's install waits";
    let answer = format!(r#"[{{"kind":"other_line","quote":"{flagged}","reason":"the child does not depend on it"}}]"#);
    std::fs::write(&home.config_path, crate::test_fixtures::presend_stub_config(&answer, &calls_file)).unwrap();
    let resent = format!("go on\n{FLAGGED}\n{flagged}");
    let error = dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, &resent, "agent", agent_fields(&parent.id))
      .await
      .expect_err("the re-send is refused again on its new passage");
    let detail = error.to_string();
    assert!(detail.contains("[1] other_line · "), "{detail}");
    assert!(!detail.contains("repeat"), "{detail}");
  }

  /// A tempdir whose home holds the stub config, plus the paths the tests read back.
  struct TestHome {
    features: Arc<FeatureContext>,
    calls_file: std::path::PathBuf,
    config_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
  }

  fn tempdir_with_config(answer: &str) -> TestHome {
    let dir = tempfile::tempdir().unwrap();
    let calls_file = dir.path().join("calls.log");
    let config_path = dir.path().join("home").join("config.yaml");
    let features =
      stub_features(&dir.path().join("home"), &crate::test_fixtures::presend_stub_config(answer, &calls_file));
    TestHome { features, calls_file, config_path, _dir: dir }
  }
}

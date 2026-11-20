//! The server's judge of Waiting on you items. A status set that stores a new or changed item asks one model call one
//! question about it: whether a settled sentence already decides the step, which only the user could decide otherwise.
//! The judge refuses the item only with a quoted settled sentence, and accepts every other item.
//!
//! The settled text is the session's effective manager prompt, every memory entry, and the session memo. The call runs
//! on the `backends.calls` pool of `JUDGE_CALL_CLASS`. A call that fails, or an answer that is not the verdict contract,
//! is the "判定不可用" error: the set stores nothing, and the error does not turn into an accept or a refusal.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use axum::http::StatusCode;
use charliebot_agents::{agent_environment, backend_pool as pools, backend_types, model_pool, one_shot_text};
use charliebot_compat::file_urls;
use charliebot_core::RepoRoot;
use charliebot_core::config::{CallClass, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_core::run_kind::RunKind;
use charliebot_core::str_methods::take_chars;
use charliebot_memory::memory;
use charliebot_registry::FeatureContext;
use charliebot_registry::backend::LaunchArgs;
use charliebot_registry::turn::SCOPE_GOAL;
use charliebot_store::compact::CompactValue;
use charliebot_store::layout;
use charliebot_store::metadata::SessionMetadata;
use charliebot_tasks::cold_read;
use charliebot_tasks::prompts::{self, PromptBlock, PromptEnv};
use serde::Deserialize;
use serde_json::Value;

use crate::metadata::{JudgedKind, WaitingItem};
use crate::reply::{self, WebReplyError};

/// The judge contract under `prompts/`, sent as the system prompt of every judge call.
const JUDGE_PROMPT: &str = include_str!("../../../../../prompts/waiting_judge.md");
/// The call class whose `backends.calls` pool answers the judge. A rule judgment is review work, so it runs on the
/// `review` pool; the `thread` pool is the interactive Opus pool that host rules reserve for interactive sessions.
pub const JUDGE_CALL_CLASS: CallClass = CallClass::Review;
/// The longest wait for one judge call, which reads the whole effective prompt and every memory entry.
const JUDGE_TIMEOUT: Duration = Duration::from_secs(120);
/// The newest user messages of the session the judge reads, each cut to `USER_MESSAGE_CHARS`.
const USER_MESSAGE_COUNT: usize = 20;
const USER_MESSAGE_CHARS: usize = 2000;
/// The start of a judge answer that the "判定不可用" detail quotes when the answer holds no verdict object.
const ANSWER_EXCERPT_CHARS: usize = 200;
const USER_ONLY_KINDS_LEAD: &str = "- A Waiting on you item asks the user for one of these:";
const MEMO_LABEL: &str = "memo.md";

/// The judge's answer for one item: the item is stored with the kind it matched, or the rule that refuses it. Each
/// variant carries the reason the model gave, which the server logs with the verdict.
#[derive(Debug, PartialEq, Eq)]
pub enum Judgement {
  Accepted {
    judged_kind: Option<JudgedKind>,
    reason: String,
  },
  /// `rule` names the settled source and quotes the sentence that decides the step.
  Refused {
    rule: String,
    reason: String,
  },
}

/// One settled text the judge may cite: its label in a refusal and its text.
struct Settled {
  label: String,
  text: String,
}

/// The settled text of a session and the user-only kinds that the effective prompt lists.
struct Settlement {
  settled: Vec<Settled>,
  kinds: String,
}

/// Text the judge reads as context that never decides a refusal, such as a repository contract the item's step runs
/// under. The judge can name it in no verdict: a refusal cites only settled text.
pub struct ContextText {
  pub label: String,
  pub text: String,
}

/// The judge's answer as the prompt's output contract states it. The reason comes first, before the decision.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
  reason: String,
  decision: String,
  judged_kind: String,
  cited: String,
}

/// Judge one item of a status set using the session's `goal` and the `context` texts, which never decide a refusal.
pub async fn judge_item(
  features: &FeatureContext,
  session_id: &str,
  goal: Option<&str>,
  item: &WaitingItem,
  context: &[ContextText],
) -> Result<Judgement, WebReplyError> {
  let cfg = features.config.get().map_err(|error| WebReplyError::Failed(error.into()))?;
  let meta = reply::require_web_session(features, session_id).await?;
  let memo = read_memo(&features.store.session_dir(session_id))?;
  let user_messages = user_messages(features, session_id).await?;
  let Settlement { mut settled, kinds } = prompt_settlement(features, &cfg, &meta).await?;
  settled.extend(memory_settled(&cfg.charliebot_home).await?);
  if !memo.trim().is_empty() {
    settled.push(Settled { label: MEMO_LABEL.to_string(), text: memo });
  }
  let item_block = cold_read::ask_probe_input(
    &serde_json::to_value(item).map_err(|e| WebReplyError::Failed(e.into()))?,
    linked_page(item)?.as_deref(),
    "",
  )
  .map_err(WebReplyError::Failed)?;
  let prompt = render_prompt(&item_block, goal, &user_messages, &settled, context, &kinds);
  let answer = match light_answer(features, session_id, &prompt).await {
    Ok(answer) => answer,
    Err(error) => return Err(unavailable(&item.text, &format!("the model call failed: {error:#}"))),
  };
  let judgement = parse_verdict(&answer, &settled).map_err(|reason| unavailable(&item.text, &reason))?;
  tracing::info!(target: "charliebot_web_reply", session_id, item = %item.text, verdict = ?judgement, "waiting_judge_verdict");
  Ok(judgement)
}

/// The "判定不可用" refusal: no verdict was reached, so nothing is stored and nothing is refused.
fn unavailable(text: &str, reason: &str) -> WebReplyError {
  WebReplyError::refused(
    StatusCode::SERVICE_UNAVAILABLE,
    format!("判定不可用: the waiting-item judge reached no verdict for {text:?}: {reason}; nothing was stored"),
  )
}

/// The session memo, or the empty text before the session writes one.
fn read_memo(session_dir: &Path) -> Result<String, WebReplyError> {
  match fs::read_to_string(layout::memo_path(session_dir)) {
    Ok(text) => Ok(text),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
    Err(error) => Err(WebReplyError::Failed(error.into())),
  }
}

/// The newest user messages of the session, oldest first.
async fn user_messages(features: &FeatureContext, session_id: &str) -> Result<Vec<String>, WebReplyError> {
  let events = reply::load_events(features, session_id).await.map_err(WebReplyError::Failed)?;
  let messages: Vec<String> = events
    .iter()
    .filter(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::USER))
    .filter_map(|event| event.get("content").and_then(CompactValue::as_str))
    .map(|text| take_chars(text, USER_MESSAGE_CHARS))
    .collect();
  let first = messages.len().saturating_sub(USER_MESSAGE_COUNT);
  Ok(messages[first..].to_vec())
}

/// Every memory entry of every topic, as settled text.
async fn memory_settled(home: &Path) -> Result<Vec<Settled>, WebReplyError> {
  let memory_dir = home.join("memory");
  let entries = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Settled>> {
    let store = memory::load_store(&memory_dir)?;
    Ok(store.entries.iter().map(|entry| Settled { label: entry.id(), text: memory::full_text(entry) }).collect())
  })
  .await
  .map_err(|error| WebReplyError::Failed(error.into()))?;
  entries.map_err(WebReplyError::Failed)
}

/// The session's effective manager prompt, read as the session's next manager turn runs with it: the session's backend
/// option decides its overlay, and the blocks are the same ones the launch injects. The Goal block is left out, since a
/// Goal never decides a refusal. The user-only kinds come from the block that lists them.
async fn prompt_settlement(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
) -> Result<Settlement, WebReplyError> {
  let backend_id = model_pool::turn_member(features, cfg, meta)
    .map_err(WebReplyError::Failed)?
    .with_context(|| format!("session {} has no selectable backend member", meta.id))
    .map_err(WebReplyError::Failed)?;
  let option = cfg
    .get_backend_option(&backend_id)
    .with_context(|| format!("backend {backend_id} is not a backends.options id"))
    .map_err(WebReplyError::Failed)?;
  let repo_root = features.startup.require::<RepoRoot>().map_err(|error| WebReplyError::Failed(error.into()))?;
  let env = PromptEnv { cfg, registry: &features.registry, checkout: &repo_root };
  let coherent =
    prompts::assemble_coherent_snapshot(&features.store, &env, meta, RunKind::ManagerTurn.as_str(), option)
      .await
      .map_err(WebReplyError::Failed)?;
  let kinds = user_only_kinds(&coherent.snapshot.blocks).map_err(WebReplyError::Failed)?;
  let settled = coherent.snapshot.blocks.iter().filter(|block| !is_goal_block(block)).map(settled_block).collect();
  Ok(Settlement { settled, kinds })
}

/// Whether a block holds only the Goal lines.
fn is_goal_block(block: &PromptBlock) -> bool {
  !block.sources.is_empty() && block.sources.iter().all(|source| source.scope == SCOPE_GOAL)
}

/// One effective-prompt block as settled text: its label is the source files it merges.
fn settled_block(block: &PromptBlock) -> Settled {
  let label = block.sources.iter().map(|source| source.source_ref.as_str()).collect::<Vec<_>>().join(", ");
  Settled { label, text: block.text.clone() }
}

/// The linked host file's text when the item links one; a link to a missing path or a directory has none.
fn linked_page(item: &WaitingItem) -> Result<Option<String>, WebReplyError> {
  let Some(path) = file_urls::host_file(item.link.as_deref()).map_err(|error| WebReplyError::Failed(anyhow!(error)))?
  else {
    return Ok(None);
  };
  if !file_urls::is_file(&path).map_err(|error| WebReplyError::Failed(error.into()))? {
    return Ok(None);
  }
  fs::read_to_string(&path).map(Some).map_err(|error| WebReplyError::Failed(error.into()))
}

/// The bullet that lists the four user-only kinds of Waiting on you items, with its indented sub-bullets, from the first
/// effective-prompt block that holds it.
fn user_only_kinds(blocks: &[PromptBlock]) -> anyhow::Result<String> {
  for block in blocks {
    let mut lines = block.text.lines().skip_while(|line| *line != USER_ONLY_KINDS_LEAD);
    if let Some(lead) = lines.next() {
      let mut kinds = vec![lead];
      kinds.extend(lines.take_while(|line| line.starts_with("  ")));
      return Ok(kinds.join("\n"));
    }
  }
  bail!("the session's effective prompt lists no user-only kinds")
}

/// The judge prompt's user message: the item and its page, the Goal, the user messages, the settled text, the context
/// texts and the user-only kinds, each under its heading.
fn render_prompt(
  item_block: &str,
  goal: Option<&str>,
  user_messages: &[String],
  settled: &[Settled],
  context: &[ContextText],
  kinds: &str,
) -> String {
  let mut sections = vec![format!("## Waiting on you item\n{item_block}")];
  sections.push(format!("## Session Goal\n{}", goal.unwrap_or("(none)")));
  let messages = if user_messages.is_empty() {
    "(none)".to_string()
  } else {
    user_messages.iter().map(|message| format!("- {message}")).collect::<Vec<_>>().join("\n")
  };
  sections.push(format!("## Session user messages, oldest first\n{messages}"));
  for source in settled {
    sections.push(format!("## Settled text: {}\n{}", source.label, source.text));
  }
  for source in context {
    sections.push(format!("## Context, never settled text: {}\n{}", source.label, source.text));
  }
  sections.push(format!("## User-only kinds (effective prompt)\n{kinds}"));
  sections.join("\n\n")
}

/// One call to the `JUDGE_CALL_CLASS` pool in pool rotation. A member that fails is recorded out and the next member is
/// tried; when none answers, the call fails with each member's failure.
async fn light_answer(features: &FeatureContext, session_id: &str, prompt: &str) -> anyhow::Result<String> {
  let cfg = features.config.get()?;
  let class = JUDGE_CALL_CLASS.key();
  let target = cfg.backends.calls.get(JUDGE_CALL_CLASS);
  if target.is_none() {
    bail!("backends.calls.{class} is unset");
  }
  let pool = pools::backend_pool(features)?;
  let base_env = agent_environment::spawn_base_environment(features)?;
  let mut failures = Vec::new();
  for option in pools::one_shot_members(&cfg, Arc::clone(&pool), class, target) {
    let option = option?;
    let launch = LaunchArgs { cgroup_session_id: Some(session_id.to_string()), ..LaunchArgs::default() };
    let outcome = match backend_types::build_backend(Arc::clone(&features.registry), &option, &cfg, launch) {
      Ok(built) => one_shot_text(built, &base_env.values, prompt, JUDGE_PROMPT, Some(JUDGE_TIMEOUT)).await,
      Err(error) => Err(error),
    };
    match outcome {
      Ok(answer) => return Ok(answer),
      Err(error) => {
        let detail = format!("{error:#}");
        tracing::warn!(target: "charliebot_web_reply", session_id, backend = %option.id, error = %detail, "waiting_judge_member_failed");
        if let Some(reason) = pools::classify_member_failure(&detail, None).availability_reason() {
          pool.availability().record(&cfg, &pool.registry(), &option.id, None, reason, &detail)?;
        }
        failures.push(format!("{}: {detail}", option.id));
      }
    }
  }
  bail!("no light backend answered: {}", failures.join("; "))
}

/// The one extraction the judge's answer allows: a whole answer inside one outer code fence unwraps to the fenced
/// text. Any other shape reads as it is, so text around the object keeps the answer from being one verdict object.
fn strip_code_fence(answer: &str) -> &str {
  let Some(rest) = answer.strip_prefix("```") else {
    return answer;
  };
  let Some(first_line_end) = rest.find('\n') else {
    return answer;
  };
  match rest[first_line_end + 1..].strip_suffix("```") {
    Some(body) => body.trim(),
    None => answer,
  }
}

/// Parse the judge's answer against the contract, and check that a refusal's quote is in a settled text. The verdict
/// is the whole answer, or the fenced text of an answer inside one outer code fence; every other answer, including
/// free text and text around the object, reaches no verdict.
fn parse_verdict(answer: &str, settled: &[Settled]) -> Result<Judgement, String> {
  let trimmed = answer.trim();
  if trimmed.is_empty() {
    return Err("the answer is empty".to_string());
  }
  let parsed: Answer = serde_json::from_str(strip_code_fence(trimmed)).map_err(|error| {
    format!(
      "the answer is not exactly one verdict object: {error}; the answer begins: {}",
      take_chars(trimmed, ANSWER_EXCERPT_CHARS)
    )
  })?;
  let reason = parsed.reason.trim().to_string();
  if reason.is_empty() {
    return Err("the verdict gives no reason".to_string());
  }
  let cited = parsed.cited.trim();
  match parsed.decision.as_str() {
    "accept" => {
      if !cited.is_empty() {
        return Err("an accept verdict cites text".to_string());
      }
      Ok(Judgement::Accepted { judged_kind: judged_kind(&parsed.judged_kind)?, reason })
    }
    "refuse" => {
      if !parsed.judged_kind.trim().is_empty() {
        return Err("a refuse verdict names a kind".to_string());
      }
      if cited.is_empty() {
        return Err("a refuse verdict cites no text".to_string());
      }
      let quote = normalize(cited);
      let source = settled
        .iter()
        .find(|source| normalize(&source.text).contains(&quote))
        .ok_or_else(|| format!("the cited text is in no settled source: {cited}"))?;
      Ok(Judgement::Refused { rule: format!("{}: {cited}", source.label), reason })
    }
    other => Err(format!("unknown decision {other:?}")),
  }
}

/// The kind the verdict names: empty when none fits, else one of the four user-only kinds.
fn judged_kind(name: &str) -> Result<Option<JudgedKind>, String> {
  if name.is_empty() {
    return Ok(None);
  }
  serde_json::from_value(Value::String(name.to_string())).map(Some).map_err(|_| format!("unknown judged_kind {name:?}"))
}

/// Text with its whitespace runs collapsed to single spaces, so a quote matches across a line break.
fn normalize(text: &str) -> String {
  text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
  use axum::http::StatusCode;
  use charliebot_core::event_types::WEB_STATUS;
  use serde_json::json;

  use super::*;
  use crate::status::{record_ask_check, set_status};
  use crate::test_support::Home;

  fn source(label: &str, text: &str) -> Settled {
    Settled { label: label.to_string(), text: text.to_string() }
  }

  #[test]
  fn the_user_only_kinds_come_from_the_block_that_lists_them() {
    let block = |text: &str| PromptBlock {
      sources: Vec::new(),
      body_ref: String::new(),
      delivery: String::new(),
      text: text.to_string(),
    };
    let listing = format!("{USER_ONLY_KINDS_LEAD}\n  - one\n  - two\n  - three\n  - four\n## Next");
    let kinds = user_only_kinds(&[block("no list here"), block(&listing)]).unwrap();
    assert!(kinds.starts_with(USER_ONLY_KINDS_LEAD));
    assert_eq!(kinds.lines().count(), 5);
    assert!(user_only_kinds(&[block("no list here")]).is_err());
  }

  #[test]
  fn an_accept_names_its_kind_and_an_empty_kind_is_none() {
    let settled = [source("memo.md", "anything")];
    let verdict =
      parse_verdict(r#"{"reason":"a step","decision":"accept","judged_kind":"take_off","cited":""}"#, &settled);
    assert_eq!(
      verdict,
      Ok(Judgement::Accepted { judged_kind: Some(JudgedKind::TakeOff), reason: "a step".to_string() })
    );
    let verdict = parse_verdict(r#"{"reason":"a step","decision":"accept","judged_kind":"","cited":""}"#, &settled);
    assert_eq!(verdict, Ok(Judgement::Accepted { judged_kind: None, reason: "a step".to_string() }));
  }

  #[test]
  fn a_refusal_names_the_source_of_its_quote_across_line_breaks() {
    let settled =
      [source("memo.md", "Unrelated."), source("surpass/gpuq", "The queue gate runs the\ntests on the CPU first.")];
    let verdict = parse_verdict(
      r#"{"reason":"a step","decision":"refuse","judged_kind":"","cited":"The queue gate runs the tests on the CPU first."}"#,
      &settled,
    );
    assert_eq!(
      verdict,
      Ok(Judgement::Refused {
        rule: "surpass/gpuq: The queue gate runs the tests on the CPU first.".to_string(),
        reason: "a step".to_string(),
      })
    );
  }

  #[test]
  fn a_contract_break_is_an_error_not_a_verdict() {
    let settled = [source("memo.md", "text")];
    for answer in [
      "",
      "not json",
      r#"{"reason":"r","decision":"accept","judged_kind":"take_off","cited":"text","extra":1}"#,
      r#"{"reason":"r","decision":"accept","judged_kind":"approval","cited":""}"#,
      r#"{"reason":"r","decision":"accept","judged_kind":"","cited":"text"}"#,
      r#"{"reason":"r","decision":"refuse","judged_kind":"take_off","cited":"text"}"#,
      r#"{"reason":"r","decision":"refuse","judged_kind":"","cited":""}"#,
      r#"{"reason":"r","decision":"refuse","judged_kind":"","cited":"invented"}"#,
      r#"{"reason":"r","decision":"maybe","judged_kind":"","cited":""}"#,
      r#"{"reason":"","decision":"accept","judged_kind":"","cited":""}"#,
      r#"{"decision":"accept","judged_kind":"","cited":""}"#,
    ] {
      assert!(parse_verdict(answer, &settled).is_err(), "{answer} must not parse as a verdict");
    }
  }

  #[test]
  fn a_verdict_in_one_outer_code_fence_parses_and_text_around_the_object_reaches_no_verdict() {
    let settled = [source("memo.md", "text")];
    let fenced = format!("```json\n{TAKE_OFF_ACCEPT}\n```");
    assert_eq!(
      parse_verdict(&fenced, &settled),
      Ok(Judgement::Accepted {
        judged_kind: Some(JudgedKind::TakeOff),
        reason: "an out-of-window deploy with no settled sentence".to_string(),
      })
    );
    for answer in [
      format!("Here is the verdict:\n```json\n{TAKE_OFF_ACCEPT}\n```\nDone."),
      format!("the verdict is {TAKE_OFF_ACCEPT}"),
      format!("{TAKE_OFF_ACCEPT}\n{TAKE_OFF_ACCEPT}"),
    ] {
      assert!(parse_verdict(&answer, &settled).is_err(), "{answer} must reach no verdict");
    }
  }

  /// The settled sentence the refusal test writes into a rulings entry and quotes.
  const SETTLED: &str = "The unit tests of the gate run on the CPU, so they need no take off.";
  const TAKE_OFF_ACCEPT: &str = r#"{"reason": "an out-of-window deploy with no settled sentence", "decision": "accept", "judged_kind": "take_off", "cited": ""}"#;

  fn refuse(cited: &str) -> String {
    json!({"reason": "a settled sentence decides the step", "decision": "refuse", "judged_kind": "", "cited": cited})
      .to_string()
  }

  /// An item the judge reads as an out-of-window production take off, with its context.
  fn production_item(text: &str) -> Value {
    json!({
      "text": text,
      "context": "The release window closed. Deploy now or wait for the next window; deploy is recommended.",
    })
  }

  /// Records a cold read of each item, then sets them as the waiting list.
  async fn set_with_cold_reads(home: &Home, session_id: &str, items: &[Value]) -> Result<Value, WebReplyError> {
    for item in items {
      record_ask_check(&home.features, session_id, item, "synthetic-haiku", "Deploy now or wait.")
        .await
        .expect("the cold read records");
    }
    set_status(&home.features, session_id, &json!({"waiting": items}).to_string()).await
  }

  async fn stored_status(home: &Home, session_id: &str) -> Value {
    home.meta(session_id).await.extra.get(WEB_STATUS).cloned().unwrap_or(Value::Null)
  }

  fn status_events(home: &Home, session_id: &str) -> usize {
    home.events(session_id).iter().filter(|event| event["type"] == WEB_STATUS).count()
  }

  fn expect_refused(error: WebReplyError, expected: StatusCode) -> String {
    match error {
      WebReplyError::Refused { status, detail } if status == expected => detail,
      other => panic!("expected a refusal with {expected}, got {other:?}"),
    }
  }

  /// A rulings entry whose body is `sentence`, written where the test home's memory store reads it.
  fn write_rulings_entry(home: &Home, slug: &str, sentence: &str) {
    let dir = home.root().join("memory").join("entries").join("rulings");
    fs::create_dir_all(&dir).expect("the rulings directory writes");
    let text =
      format!("---\nscope: user\ntopic: rulings\naudience: master, worker\ntitle: {slug}\n---\n- {sentence}\n");
    fs::write(dir.join(format!("{slug}.md")), text).expect("the rulings entry writes");
  }

  #[tokio::test]
  async fn a_refused_item_stores_nothing_and_names_the_settled_rule() {
    let home = Home::judging(&refuse(SETTLED));
    write_rulings_entry(&home, "unit-tests-cpu", SETTLED);
    let sid = home.manager("session", None).await;

    let error = set_with_cold_reads(&home, &sid, &[production_item("Approve the gate run")])
      .await
      .expect_err("a settled rule refuses the item");
    let detail = expect_refused(error, StatusCode::CONFLICT);
    assert!(detail.contains(&format!("rulings/unit-tests-cpu: {SETTLED}")), "{detail}");
    assert_eq!(stored_status(&home, &sid).await, Value::Null);
    assert_eq!(status_events(&home, &sid), 0);
  }

  #[tokio::test]
  async fn an_out_of_window_take_off_in_a_session_whose_approver_is_its_parent_is_accepted() {
    let home = Home::judging(TAKE_OFF_ACCEPT);
    let root = home.manager("root", None).await;
    let child = home.child(&root, "deploy").await;
    set_status(
      &home.features,
      &child,
      &json!({"goal": "Ship the release; the parent session approves its steps."}).to_string(),
    )
    .await
    .expect("the goal stores");

    set_with_cold_reads(&home, &child, &[production_item("Deploy the release outside its window")])
      .await
      .expect("the judge accepts the item");
    let waiting = &stored_status(&home, &child).await["waiting"][0];
    assert_eq!(waiting["judged_kind"], "take_off");
    assert_eq!(waiting["judgement_basis"], "");
  }

  #[tokio::test]
  async fn an_out_of_window_take_off_with_no_citable_basis_is_accepted_with_an_empty_basis() {
    let home = Home::judging(TAKE_OFF_ACCEPT);
    let sid = home.manager("session", None).await;

    set_with_cold_reads(&home, &sid, &[production_item("Deploy the release outside its window")])
      .await
      .expect("the judge accepts the item");
    let waiting = &stored_status(&home, &sid).await["waiting"][0];
    assert_eq!(waiting["judged_kind"], "take_off");
    assert_eq!(waiting["judgement_basis"], "");
    assert_eq!(status_events(&home, &sid), 1);
  }

  #[tokio::test]
  async fn a_failed_or_unparsable_judge_call_stores_nothing_and_reports_the_unavailable_verdict() {
    for verdict in ["", "not a verdict object"] {
      let home = Home::judging(verdict);
      let sid = home.manager("session", None).await;

      let error = set_with_cold_reads(&home, &sid, &[production_item("Deploy the release")])
        .await
        .expect_err("no verdict refuses the set");
      let detail = expect_refused(error, StatusCode::SERVICE_UNAVAILABLE);
      assert!(detail.starts_with("判定不可用"), "{detail}");
      assert_eq!(stored_status(&home, &sid).await, Value::Null);
      assert_eq!(status_events(&home, &sid), 0);
    }
  }

  #[tokio::test]
  async fn a_caller_cannot_submit_the_judged_fields_of_a_waiting_item() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    for field in [("judged_kind", json!("take_off")), ("judgement_basis", json!("a rule"))] {
      let mut item = production_item("Deploy the release");
      item[field.0] = field.1;
      let error = set_status(&home.features, &sid, &json!({"waiting": [item.clone()]}).to_string())
        .await
        .expect_err("a caller-supplied judged field is refused");
      let detail = expect_refused(error, StatusCode::UNPROCESSABLE_ENTITY);
      assert!(detail.contains(field.0), "{detail}");
      let error = record_ask_check(&home.features, &sid, &item, "synthetic-haiku", "Deploy now or wait.")
        .await
        .expect_err("the ask-check refuses a judged field too");
      expect_refused(error, StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert_eq!(stored_status(&home, &sid).await, Value::Null);
  }

  #[test]
  fn the_judge_input_carries_the_goal_the_memo_the_user_messages_and_the_kinds() {
    let settled = [Settled { label: "memo.md".to_string(), text: "MEMO TEXT".to_string() }];
    let context = [ContextText { label: "repo/gate.sh:1-2".to_string(), text: "CONTEXT TEXT".to_string() }];
    let messages = ["first user message".to_string()];
    let prompt = render_prompt("ITEM BLOCK", Some("GOAL TEXT"), &messages, &settled, &context, "KINDS TEXT");
    assert!(prompt.contains("## Waiting on you item\nITEM BLOCK"));
    assert!(prompt.contains("## Session Goal\nGOAL TEXT"));
    assert!(prompt.contains("- first user message"));
    assert!(prompt.contains("## Settled text: memo.md\nMEMO TEXT"));
    assert!(prompt.contains("repo/gate.sh:1-2") && prompt.contains("CONTEXT TEXT"));
    assert!(prompt.contains("## User-only kinds (effective prompt)\nKINDS TEXT"));
  }

  #[test]
  fn a_refusal_that_quotes_only_a_context_text_is_no_verdict() {
    // Context texts stay out of the citable set: the judge checks a quote against the settled texts alone.
    let context =
      ContextText { label: "repo/gate.sh:1-2".to_string(), text: "The gate's paths are its own.".to_string() };
    let settled = [source("memo.md", "Unrelated.")];
    let verdict = parse_verdict(&refuse(&context.text), &settled);
    assert!(verdict.is_err(), "a context text decides no refusal: {verdict:?}");
  }

  fn write_memory(home: &Home) {
    let memory = home.root().join("memory");
    fs::create_dir_all(&memory).expect("the memory directory writes");
    fs::write(memory.join("topics"), "rulings resident\nsurpass\nworkflow resident\n")
      .expect("the topics vocabulary writes");
    for (topic, slug) in [("rulings", "a"), ("surpass", "gpuq"), ("workflow", "w")] {
      let dir = memory.join("entries").join(topic);
      fs::create_dir_all(&dir).expect("the topic directory writes");
      let entry = format!("---\nscope: user\ntopic: {topic}\naudience: master, worker\ntitle: {slug}\n---\n- a rule\n");
      fs::write(dir.join(format!("{slug}.md")), entry).expect("the entry writes");
    }
  }

  #[tokio::test]
  async fn the_settled_memory_is_every_entry_of_every_topic() {
    let home = Home::new();
    write_memory(&home);
    let settled = memory_settled(&home.root()).await.expect("the settled memory loads");
    let mut labels: Vec<_> = settled.into_iter().map(|source| source.label).collect();
    labels.sort();
    assert_eq!(labels, ["rulings/a", "surpass/gpuq", "workflow/w"]);
  }

  #[tokio::test]
  async fn the_effective_prompt_is_settled_text_but_its_goal_block_is_not() {
    let home = Home::judging(TAKE_OFF_ACCEPT);
    let sid = home.manager("session", None).await;
    set_status(&home.features, &sid, &json!({"goal": "GOAL SENTENCE ONLY"}).to_string())
      .await
      .expect("the goal stores");
    let meta = home.meta(&sid).await;
    let cfg = home.features.config.get().expect("the config reads");

    let Settlement { settled, kinds } =
      prompt_settlement(&home.features, &cfg, &meta).await.expect("the effective prompt reads");
    assert!(settled.iter().any(|source| source.label == "prompts/master.md"), "the master prompt is settled text");
    assert!(settled.iter().all(|source| !source.text.contains("GOAL SENTENCE ONLY")));
    assert!(kinds.starts_with(USER_ONLY_KINDS_LEAD));
  }
}

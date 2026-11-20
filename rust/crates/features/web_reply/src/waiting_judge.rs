//! The server's judge of Waiting on you items. A status set that stores a new or changed item asks the light backends
//! one question about it: whether a settled rule or ruling already decides the step, which only the user could decide
//! otherwise. The judge refuses the item only with a quoted settled sentence, and accepts every other item.
//!
//! The call runs on `backends.calls.probe` through the backend pool's rotation, as the cold read does. A call that
//! fails, or an answer that is not the verdict contract, is the "判定不可用" error: the set stores nothing, and the
//! error does not turn into an accept or a refusal.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use axum::http::StatusCode;
use charliebot_agents::{agent_environment, backend_pool as pools, backend_types, one_shot_text};
use charliebot_compat::file_urls;
use charliebot_core::config::CallClass;
use charliebot_core::event_types;
use charliebot_core::str_methods::take_chars;
use charliebot_memory::memory;
use charliebot_registry::FeatureContext;
use charliebot_registry::backend::LaunchArgs;
use charliebot_store::compact::CompactValue;
use charliebot_store::layout;
use charliebot_tasks::cold_read;
use serde::Deserialize;
use serde_json::Value;

use crate::metadata::{JudgedKind, WaitingItem};
use crate::reply::{self, WebReplyError};

/// The judge contract under `prompts/`, sent as the system prompt of every judge call.
const JUDGE_PROMPT: &str = include_str!("../../../../../prompts/waiting_judge.md");
/// The CharlieBot rules the judge reads its settled text and its user-only kinds from.
const MASTER_PROMPT: &str = include_str!("../../../../../prompts/master.md");
/// The memory topic of the host's approval rulings, read for every session.
const RULINGS_TOPIC: &str = "rulings";
/// The newest user messages of the session the judge reads, each cut to `USER_MESSAGE_CHARS`.
const USER_MESSAGE_COUNT: usize = 20;
const USER_MESSAGE_CHARS: usize = 2000;
const ACTION_BOUNDARIES_HEADING: &str = "## Action Boundaries";
const USER_ONLY_KINDS_LEAD: &str = "- A Waiting on you item asks the user for one of these:";
const MEMO_LABEL: &str = "memo.md";
const ACTION_BOUNDARIES_LABEL: &str = "prompts/master.md, Action Boundaries";

/// The judge's answer for one item: the item is stored with the kind it matched, or the rule that refuses it.
#[derive(Debug, PartialEq, Eq)]
pub enum Judgement {
  Accepted {
    judged_kind: Option<JudgedKind>,
  },
  /// `rule` names the settled source and quotes the sentence that decides the step.
  Refused {
    rule: String,
  },
}

/// One settled text the judge may cite: its label in a refusal and its text.
struct Settled {
  label: String,
  text: String,
}

/// The judge's answer as the prompt's output contract states it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
  decision: String,
  judged_kind: String,
  cited: String,
}

/// Judge one item of a status set whose session has `group` as its project topic and `goal` as its Goal.
pub async fn judge_item(
  features: &FeatureContext,
  session_id: &str,
  group: Option<&str>,
  goal: Option<&str>,
  item: &WaitingItem,
) -> Result<Judgement, WebReplyError> {
  let cfg = features.config.get().map_err(|error| WebReplyError::Failed(error.into()))?;
  let memo = read_memo(&features.store.session_dir(session_id))?;
  let user_messages = user_messages(features, session_id).await?;
  let mut settled = memory_settled(&cfg.charliebot_home, group).await?;
  if !memo.trim().is_empty() {
    settled.insert(0, Settled { label: MEMO_LABEL.to_string(), text: memo });
  }
  settled.push(Settled {
    label: ACTION_BOUNDARIES_LABEL.to_string(),
    text: action_boundaries().map_err(WebReplyError::Failed)?.to_string(),
  });
  let item_block = cold_read::ask_probe_input(
    &serde_json::to_value(item).map_err(|e| WebReplyError::Failed(e.into()))?,
    linked_page(item)?.as_deref(),
    "",
  )
  .map_err(WebReplyError::Failed)?;
  let kinds = user_only_kinds().map_err(WebReplyError::Failed)?;
  let prompt = render_prompt(&item_block, goal, &user_messages, &settled, &kinds);
  let answer = match light_answer(features, session_id, &prompt).await {
    Ok(answer) => answer,
    Err(error) => return Err(unavailable(&item.text, &format!("the model call failed: {error:#}"))),
  };
  parse_verdict(&answer, &settled).map_err(|reason| unavailable(&item.text, &reason))
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

/// The memory entries of the host's rulings topic and of the session's project topic, which is its group when the
/// group names a topic.
async fn memory_settled(home: &Path, group: Option<&str>) -> Result<Vec<Settled>, WebReplyError> {
  let memory_dir = home.join("memory");
  let group = group.map(str::to_string);
  let entries = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Settled>> {
    let store = memory::load_store(&memory_dir)?;
    Ok(
      store
        .entries
        .iter()
        .filter(|entry| {
          entry.topic.as_deref().is_some_and(|topic| topic == RULINGS_TOPIC || group.as_deref() == Some(topic))
        })
        .map(|entry| Settled { label: entry.id(), text: memory::full_text(entry) })
        .collect(),
    )
  })
  .await
  .map_err(|error| WebReplyError::Failed(error.into()))?;
  entries.map_err(WebReplyError::Failed)
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

/// The Action Boundaries section of `prompts/master.md`: from its heading to the next level-two heading.
fn action_boundaries() -> anyhow::Result<&'static str> {
  let start = MASTER_PROMPT
    .find(&format!("\n{ACTION_BOUNDARIES_HEADING}\n"))
    .context("prompts/master.md has no Action Boundaries heading")?
    + 1;
  let section = &MASTER_PROMPT[start..];
  let end = section[ACTION_BOUNDARIES_HEADING.len()..]
    .find("\n## ")
    .map_or(section.len(), |offset| ACTION_BOUNDARIES_HEADING.len() + offset);
  Ok(section[..end].trim_end())
}

/// The bullet that lists the four user-only kinds of Waiting on you items, with its indented sub-bullets.
fn user_only_kinds() -> anyhow::Result<String> {
  let mut lines = MASTER_PROMPT.lines().skip_while(|line| *line != USER_ONLY_KINDS_LEAD);
  let lead = lines.next().context("prompts/master.md has no user-only kinds list")?;
  let mut kinds = vec![lead];
  kinds.extend(lines.take_while(|line| line.starts_with("  ")));
  Ok(kinds.join("\n"))
}

/// The judge prompt's user message: the item and its page, the Goal, the user messages, the settled text and the
/// user-only kinds, each under its heading.
fn render_prompt(
  item_block: &str,
  goal: Option<&str>,
  user_messages: &[String],
  settled: &[Settled],
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
  sections.push(format!("## User-only kinds (prompts/master.md)\n{kinds}"));
  sections.join("\n\n")
}

/// One call to the light backends: `backends.calls.probe` in pool rotation. A member that fails is recorded out and the
/// next member is tried; when none answers, the call fails with each member's failure.
async fn light_answer(features: &FeatureContext, session_id: &str, prompt: &str) -> anyhow::Result<String> {
  let cfg = features.config.get()?;
  let target = cfg.backends.calls.get(CallClass::Probe);
  if target.is_none() {
    bail!("backends.calls.probe is unset");
  }
  let pool = pools::backend_pool(features)?;
  let base_env = agent_environment::spawn_base_environment(features)?;
  let mut failures = Vec::new();
  for option in pools::one_shot_members(&cfg, Arc::clone(&pool), "probe", target) {
    let option = option?;
    let launch = LaunchArgs { cgroup_session_id: Some(session_id.to_string()), ..LaunchArgs::default() };
    let outcome = match backend_types::build_backend(Arc::clone(&features.registry), &option, &cfg, launch) {
      Ok(built) => one_shot_text(built, &base_env.values, prompt, JUDGE_PROMPT, Some(cold_read::PROBE_TIMEOUT)).await,
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

/// Parse the judge's answer against the contract, and check that a refusal's quote is in a settled text.
fn parse_verdict(answer: &str, settled: &[Settled]) -> Result<Judgement, String> {
  let parsed: Answer =
    serde_json::from_str(answer.trim()).map_err(|error| format!("the answer is not the verdict object: {error}"))?;
  let cited = parsed.cited.trim();
  match parsed.decision.as_str() {
    "accept" => {
      if !cited.is_empty() {
        return Err("an accept verdict cites text".to_string());
      }
      Ok(Judgement::Accepted { judged_kind: judged_kind(&parsed.judged_kind)? })
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
      Ok(Judgement::Refused { rule: format!("{}: {cited}", source.label) })
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
  fn the_master_prompt_yields_its_action_boundaries_and_user_only_kinds() {
    let boundaries = action_boundaries().unwrap();
    assert!(boundaries.starts_with("## Action Boundaries"));
    assert!(boundaries.contains("high-consequence steps"));
    assert!(!boundaries.contains("\n## "));
    let kinds = user_only_kinds().unwrap();
    assert!(kinds.starts_with(USER_ONLY_KINDS_LEAD));
    assert_eq!(kinds.lines().count(), 4);
  }

  #[test]
  fn an_accept_names_its_kind_and_an_empty_kind_is_none() {
    let settled = [source("memo.md", "anything")];
    let verdict = parse_verdict(r#"{"decision":"accept","judged_kind":"take_off","cited":""}"#, &settled);
    assert_eq!(verdict, Ok(Judgement::Accepted { judged_kind: Some(JudgedKind::TakeOff) }));
    let verdict = parse_verdict(r#"{"decision":"accept","judged_kind":"","cited":""}"#, &settled);
    assert_eq!(verdict, Ok(Judgement::Accepted { judged_kind: None }));
  }

  #[test]
  fn a_refusal_names_the_source_of_its_quote_across_line_breaks() {
    let settled =
      [source("memo.md", "Unrelated."), source("surpass/gpuq", "The queue gate runs the\ntests on the CPU first.")];
    let verdict = parse_verdict(
      r#"{"decision":"refuse","judged_kind":"","cited":"The queue gate runs the tests on the CPU first."}"#,
      &settled,
    );
    assert_eq!(
      verdict,
      Ok(Judgement::Refused { rule: "surpass/gpuq: The queue gate runs the tests on the CPU first.".to_string() })
    );
  }

  #[test]
  fn a_contract_break_is_an_error_not_a_verdict() {
    let settled = [source("memo.md", "text")];
    for answer in [
      "not json",
      r#"{"decision":"accept","judged_kind":"take_off","cited":"text","extra":1}"#,
      r#"{"decision":"accept","judged_kind":"approval","cited":""}"#,
      r#"{"decision":"accept","judged_kind":"","cited":"text"}"#,
      r#"{"decision":"refuse","judged_kind":"take_off","cited":"text"}"#,
      r#"{"decision":"refuse","judged_kind":"","cited":""}"#,
      r#"{"decision":"refuse","judged_kind":"","cited":"invented"}"#,
      r#"{"decision":"maybe","judged_kind":"","cited":""}"#,
    ] {
      assert!(parse_verdict(answer, &settled).is_err(), "{answer} must not parse as a verdict");
    }
  }

  /// The settled sentence the refusal test writes into a rulings entry and quotes.
  const SETTLED: &str = "The unit tests of the gate run on the CPU, so they need no take off.";
  const TAKE_OFF_ACCEPT: &str = r#"{"decision": "accept", "judged_kind": "take_off", "cited": ""}"#;

  fn refuse(cited: &str) -> String {
    json!({"decision": "refuse", "judged_kind": "", "cited": cited}).to_string()
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
    let messages = ["first user message".to_string()];
    let prompt = render_prompt("ITEM BLOCK", Some("GOAL TEXT"), &messages, &settled, "KINDS TEXT");
    assert!(prompt.contains("## Waiting on you item\nITEM BLOCK"));
    assert!(prompt.contains("## Session Goal\nGOAL TEXT"));
    assert!(prompt.contains("- first user message"));
    assert!(prompt.contains("## Settled text: memo.md\nMEMO TEXT"));
    assert!(prompt.contains("## User-only kinds (prompts/master.md)\nKINDS TEXT"));
  }
}

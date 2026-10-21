//! The Rust side of `tests/thread_helpers_differential.py`: the pure thread-core helpers over one cases file.
//!
//! Usage: `thread_helpers <cases.json> <out_dir>`. It writes `<out_dir>/results.json`, one list of answers per helper.
//! The Slack and Discord platforms are the crates' own `thread_contract.rs` files, included by path because the crates
//! keep those modules private. Each include is the file itself, so the constants compared are the shipped ones.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, bail};
use charliebot_chat_threads::thread_entry::{
  ThreadDetail, ThreadPlatform, ThreadReplyError, application_route_links, assert_no_file_server_links, chunk_text,
  follow_floor, follow_wake_label, lost_summons, newest_thread_input, noticed, nudged, operator_only_note, replied,
  unread_after,
};
use serde_json::{Map, Value, json};

#[allow(dead_code)]
#[path = "../../slack/src/event_types.rs"]
mod slack_event_types;

#[allow(dead_code)]
#[path = "../../discord/src/event_types.rs"]
mod discord_event_types;

/// The crate-root `event_types` the included contract files name: each platform's reply constant.
#[allow(dead_code)]
mod event_types {
  pub use crate::discord_event_types::DISCORD_REPLY;
  pub use crate::slack_event_types::SLACK_REPLY;
}

#[allow(dead_code)]
#[path = "../../slack/src/metadata.rs"]
mod metadata;

#[allow(dead_code)]
#[path = "../../slack/src/thread_contract.rs"]
mod slack_thread_contract;

#[allow(dead_code)]
#[path = "../../discord/src/thread_contract.rs"]
mod discord_thread_contract;

/// One synthetic message of the unread helper.
struct Message {
  id: String,
  human: bool,
}

fn main() -> anyhow::Result<()> {
  let mut args = std::env::args().skip(1);
  let cases_path = PathBuf::from(args.next().context("usage: thread_helpers <cases.json> <out_dir>")?);
  let out_dir = PathBuf::from(args.next().context("usage: thread_helpers <cases.json> <out_dir>")?);
  let cases: Value =
    serde_json::from_slice(&fs::read(&cases_path).context("read the cases file")?).context("parse the cases file")?;
  let results = run_cases(&cases)?;
  fs::create_dir_all(&out_dir).context("create the output directory")?;
  fs::write(out_dir.join("results.json"), serde_json::to_string_pretty(&results)?).context("write the results")?;
  Ok(())
}

/// The answers of every helper, keyed by helper name, in the order the cases file lists them.
fn run_cases(cases: &Value) -> anyhow::Result<Map<String, Value>> {
  let mut results = Map::new();
  for (helper, list) in cases.as_object().context("the cases file is an object")? {
    let answers = list
      .as_array()
      .with_context(|| format!("the {helper} cases are a list"))?
      .iter()
      .map(|case| answer(helper, case))
      .collect::<anyhow::Result<Vec<Value>>>()?;
    results.insert(helper.clone(), Value::Array(answers));
  }
  Ok(results)
}

fn answer(helper: &str, case: &Value) -> anyhow::Result<Value> {
  Ok(match helper {
    "chunk_text" => json!(chunk_text(str_of(case, "text")?, usize_of(case, "limit")?)),
    "follow_wake_label" => json!(follow_wake_label(platform_of(case)?, str_of(case, "floor")?)),
    "follow_floor" => json!(follow_floor(str_of(case, "label")?)),
    "newest_thread_input" => {
      let events = array_of(case, "events")?;
      let event_ids = strings_of(case, "event_ids")?;
      match newest_thread_input(platform_of(case)?, events, &event_ids) {
        Some((id, block)) => json!([id, block]),
        None => Value::Null,
      }
    }
    "replied" => json!(replied(platform_of(case)?, array_of(case, "events")?, str_of(case, "summon_id")?)),
    "nudged" => json!(nudged(platform_of(case)?, array_of(case, "events")?, str_of(case, "summon_id")?)),
    "noticed" => json!(noticed(platform_of(case)?, array_of(case, "events")?, str_of(case, "summon_id")?)),
    "lost_summons" => json!(lost_summons(
      platform_of(case)?,
      array_of(case, "events")?,
      &strings_of(case, "owned")?.into_iter().collect(),
      &strings_of(case, "running")?.into_iter().collect(),
    )),
    "unread_after" => {
      let messages = array_of(case, "messages")?
        .iter()
        .map(|message| Ok(Message { id: str_of(message, "id")?.to_string(), human: bool_of(message, "human")? }))
        .collect::<anyhow::Result<Vec<Message>>>()?;
      let watermark = optional_str_of(case, "watermark")?;
      let platform = platform_of(case)?;
      let unread = unread_after(&messages, |m| m.human, |m| m.id.as_str(), watermark.as_deref(), platform.id_key);
      json!(unread.iter().map(|m| m.id.clone()).collect::<Vec<String>>())
    }
    "assert_no_file_server_links" => match assert_no_file_server_links(str_of(case, "text")?, None) {
      Ok(()) => json!({"refused": false}),
      Err(error) => refusal(&error),
    },
    "application_route_links" => {
      let port = u16::try_from(usize_of(case, "port")?).context("the port fits a u16")?;
      json!(application_route_links(str_of(case, "text")?, port))
    }
    "operator_only_note" => json!(operator_only_note(&strings_of(case, "links")?)),
    other => bail!("no helper named {other}"),
  })
}

/// The refusal a helper raises, as the cases file records it: its status and its text detail.
fn refusal(error: &ThreadReplyError) -> Value {
  let detail = match &error.detail {
    ThreadDetail::Text(text) => text.clone(),
    ThreadDetail::Structured(value) => value.to_string(),
  };
  json!({"refused": true, "status": error.status.as_u16(), "detail": detail})
}

/// The platform a case names: its constants come from the crates' own `thread_contract.rs` files.
fn platform_of(case: &Value) -> anyhow::Result<&'static ThreadPlatform> {
  match str_of(case, "platform")? {
    "slack" => Ok(&slack_thread_contract::SLACK),
    "discord" => Ok(&discord_thread_contract::DISCORD),
    other => bail!("no platform named {other}"),
  }
}

fn str_of<'a>(case: &'a Value, key: &str) -> anyhow::Result<&'a str> {
  case.get(key).and_then(Value::as_str).with_context(|| format!("the case field {key} is a string"))
}

fn optional_str_of(case: &Value, key: &str) -> anyhow::Result<Option<String>> {
  match case.get(key) {
    Some(Value::Null) | None => Ok(None),
    Some(value) => Ok(Some(value.as_str().with_context(|| format!("the case field {key} is a string"))?.to_string())),
  }
}

fn usize_of(case: &Value, key: &str) -> anyhow::Result<usize> {
  let number = case.get(key).and_then(Value::as_u64).with_context(|| format!("the case field {key} is a number"))?;
  usize::try_from(number).context("the number fits a usize")
}

fn bool_of(case: &Value, key: &str) -> anyhow::Result<bool> {
  case.get(key).and_then(Value::as_bool).with_context(|| format!("the case field {key} is a boolean"))
}

fn array_of<'a>(case: &'a Value, key: &str) -> anyhow::Result<&'a [Value]> {
  case.get(key).and_then(Value::as_array).map(Vec::as_slice).with_context(|| format!("the case field {key} is a list"))
}

fn strings_of(case: &Value, key: &str) -> anyhow::Result<Vec<String>> {
  array_of(case, key)?
    .iter()
    .map(|value| Ok(value.as_str().with_context(|| format!("an entry of {key} is a string"))?.to_string()))
    .collect()
}

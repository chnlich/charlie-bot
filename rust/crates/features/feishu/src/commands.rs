//! The Feishu server-side commands behind `charliebot feishu read`, `download` and `check`.
//!
//! A read pages the session's thread through the app's client, acks the unread messages it returns (Feishu has no
//! separate ack verb, so the read is the ack), and flags the pending summon's message with `summon: true`. A download
//! fetches one image or file of the session's thread with the tenant token and writes it into the session's directory.
//! The token never leaves the server: the command returns the readback, and the file.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::http::StatusCode;
use charliebot_chat_threads::thread_entry::{self, IdOrdering, ThreadAdapter, ThreadMessage, ThreadReplyError};
use charliebot_core::event_types;
use charliebot_core::paths;
use charliebot_store::ChatEvent;
use charliebot_store::compact::CompactValue;
use charliebot_store::message_utils::master_done_input_event_ids;
use serde_json::{Value, json};

use crate::adapter::{Bot, FeishuThreadAdapter, api_failure, message_id_of, thread_message};
use crate::client::FeishuClient;
use crate::config::FeishuConfig;
use crate::metadata::FeishuOrigin;
use crate::thread_contract::FEISHU;

/// The readback's view of one thread message: ids, the author and the configured person, the text, the message type,
/// and the unread and summon flags.
fn message_view(message: &ThreadMessage, unread: bool, summon: bool, allowed: &BTreeMap<String, String>) -> Value {
  let meta = message.metadata.as_ref();
  let field = |name: &str| meta.and_then(|meta| meta.get(name)).cloned().unwrap_or(Value::Null);
  let author_id = message.user.clone().or_else(|| message.bot_id.clone());
  json!({
    "id": message.id,
    "message_id": field("message_id"),
    "message_type": field("message_type"),
    "author_id": author_id,
    "person": message.user.as_deref().and_then(|user| allowed.get(user)),
    "timestamp": message.created_at,
    "content": message.text,
    "unread": unread,
    "summon": summon,
  })
}

/// The composite ids of the session's pending summons. A summon is pending while its event carries the feishu block
/// without `nudge_of`, no master_done names its id, no lost-summon backfill marker names it, and it has a mention id.
fn pending_summon_mention_ids(events: &[ChatEvent]) -> anyhow::Result<HashSet<String>> {
  let backfill_key = FEISHU.backfill_key();
  let mut answered: HashSet<String> = HashSet::new();
  let mut marked: HashSet<String> = HashSet::new();
  for event in events {
    if event.get("type").and_then(CompactValue::as_str) == Some(event_types::MASTER_DONE) {
      answered.extend(master_done_input_event_ids(event));
    } else if let Some(marker) = event.get(&backfill_key) {
      marked.extend(marker.get(event_types::INPUT_EVENT_ID).and_then(CompactValue::as_str).map(str::to_string));
    }
  }
  let mut pending = HashSet::new();
  for event in events {
    let Some(block) = event.get(FEISHU.name).filter(|block| block.is_object()) else {
      continue;
    };
    if event.get("type").and_then(CompactValue::as_str) != Some(event_types::AGENT_MESSAGE) {
      continue;
    }
    let id = event["id"].as_str().expect("a chat event carries a string id");
    if block.get(event_types::NUDGE_OF).is_some() || answered.contains(id) || marked.contains(id) {
      continue;
    }
    if let Some(mention) = block.get(FEISHU.mention_key).filter(|mention| !mention.is_null()) {
      pending.insert(mention.as_str().context("a summon's mention id is not a string")?.to_string());
    }
  }
  Ok(pending)
}

/// The session's chat events in log order, read on the blocking pool.
async fn load_chat_events(bot: &Bot, session_id: &str) -> Result<Vec<ChatEvent>, ThreadReplyError> {
  let store = Arc::clone(&bot.features.store);
  let session_id = session_id.to_string();
  let events = tokio::task::spawn_blocking(move || store.load_chat_events_sync(&session_id))
    .await
    .map_err(ThreadReplyError::failure)?;
  Ok(events?)
}

/// Read the session's thread, and ack the unread messages the window returns. The window starts at the earlier of the
/// oldest unread message and the oldest pending summon's message, or holds the newest `limit` messages when neither
/// exists. The summon's root message rides first, outside the count, when the thread's listing does not carry it.
pub async fn read_thread(bot: &Bot, session_id: &str, limit: usize) -> Result<Value, ThreadReplyError> {
  let session = thread_entry::require_thread_session(&FEISHU, &bot.features, session_id).await?;
  let origin: FeishuOrigin = serde_json::from_value(session.origin.clone()).map_err(ThreadReplyError::failure)?;
  let watermark = session.meta.extra(FEISHU.watermark_field).and_then(Value::as_str).map(str::to_string);
  let config = bot.config().map_err(ThreadReplyError::failure)?;
  let allowed = &config.section::<FeishuConfig>("feishu").allowed_users;
  let adapter = Arc::new(FeishuThreadAdapter::new(bot.clone()));
  let thread_messages = adapter
    .thread_messages(&origin.root_id, origin.thread_id.as_deref(), allowed)
    .await
    .map_err(ThreadReplyError::failure)?;
  let pending =
    pending_summon_mention_ids(&load_chat_events(bot, session_id).await?).map_err(ThreadReplyError::failure)?;
  let unread_ids: HashSet<String> = thread_entry::unread_after(
    &thread_messages,
    |message| message.eligible,
    |message| message.id.as_str(),
    watermark.as_deref(),
    IdOrdering::TimeThenId,
  )
  .into_iter()
  .map(|message| message.id.clone())
  .collect();
  let mut starts = Vec::new();
  if let Some(first) = thread_messages.iter().position(|message| unread_ids.contains(&message.id)) {
    starts.push(first);
  }
  if let Some(first) = thread_messages.iter().position(|message| pending.contains(&message.id)) {
    starts.push(first);
  }
  let window: &[ThreadMessage] = match starts.into_iter().min() {
    None => &thread_messages[thread_messages.len().saturating_sub(limit)..],
    Some(start) => &thread_messages[start..thread_messages.len().min(start + limit)],
  };
  let window_unread_ids: Vec<String> =
    window.iter().filter(|message| unread_ids.contains(&message.id)).map(|message| message.id.clone()).collect();
  let mut watermark_id = json!(watermark);
  if !window_unread_ids.is_empty() {
    let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();
    let ack = thread_entry::ack_messages(&dyn_adapter, &bot.features, session_id, &window_unread_ids).await?;
    watermark_id = ack["watermark_id"].clone();
  }
  let mut messages = Vec::new();
  if let Some(root) = root_message(bot, &origin, &thread_messages, allowed).await.map_err(api_failure)? {
    messages.push(message_view(&root, false, pending.contains(&root.id), allowed));
  }
  messages.extend(
    window
      .iter()
      .map(|message| message_view(message, unread_ids.contains(&message.id), pending.contains(&message.id), allowed)),
  );
  Ok(json!({
    "messages": messages,
    "watermark_id": watermark_id,
    "more_unread": unread_ids.len() - window_unread_ids.len(),
  }))
}

/// The summon's root message, when the thread listing does not already carry it (`root_message`). The root holds the
/// summon's text in a thread the bot started from a message, and it is read with the thread's eligibility.
async fn root_message(
  bot: &Bot,
  origin: &FeishuOrigin,
  thread_messages: &[ThreadMessage],
  allowed: &BTreeMap<String, String>,
) -> anyhow::Result<Option<ThreadMessage>> {
  if thread_messages.iter().any(|message| message_id_of(&message.id).ok() == Some(origin.root_id.as_str())) {
    return Ok(None);
  }
  let client = bot.client()?;
  let item = client.message(&origin.root_id).await?;
  Ok(Some(thread_message(&item, allowed)?))
}

/// The readback of `charliebot feishu check`: each setup check with its outcome, and `ok` when all of them pass. The
/// checks are: a tenant token, the bot's open_id, the long-connection endpoint, and a non-empty allow-list. Refusals:
/// 409 when the app id or secret is unset.
pub async fn check_setup(bot: &Bot) -> Result<Value, ThreadReplyError> {
  let credentials = bot.features.credentials.get().map_err(ThreadReplyError::failure)?;
  for key in ["app_id", "app_secret"] {
    if credentials.get("feishu", key).is_none() {
      return Err(ThreadReplyError::refused(StatusCode::CONFLICT, format!("credentials.feishu.{key} is not set")));
    }
  }
  let client = bot.client().map_err(ThreadReplyError::failure)?;
  let config = bot.config().map_err(ThreadReplyError::failure)?;
  let allowed = &config.section::<FeishuConfig>("feishu").allowed_users;
  let token = check_token(&client).await;
  let bot_open_id = check_bot(&client).await;
  let endpoint = check_endpoint(&client).await;
  let allow_list = json!({"ok": !allowed.is_empty(), "count": allowed.len()});
  let ok = [&token, &bot_open_id, &endpoint, &allow_list].iter().all(|check| check["ok"] == json!(true));
  Ok(json!({
    "ok": ok,
    "tenant_token": token,
    "bot": bot_open_id,
    "long_connection_endpoint": endpoint,
    "allowed_users": allow_list,
  }))
}

async fn check_token(client: &FeishuClient) -> Value {
  match client.tenant_token().await {
    Ok(_) => json!({"ok": true}),
    Err(error) => json!({"ok": false, "error": error.to_string()}),
  }
}

async fn check_bot(client: &FeishuClient) -> Value {
  match client.bot_open_id().await {
    Ok(open_id) => json!({"ok": true, "open_id": open_id}),
    Err(error) => json!({"ok": false, "error": error.to_string()}),
  }
}

async fn check_endpoint(client: &FeishuClient) -> Value {
  match client.long_connection_endpoint().await {
    Ok(_) => json!({"ok": true}),
    Err(error) => json!({"ok": false, "error": error.to_string()}),
  }
}

/// Download one image or file of the session's thread into the session's directory, and return its path and size
/// (`download`). The message must belong to the session's thread, and `output` must be a path inside the session's
/// directory. Refusals: 404 unknown session, 409 no Feishu thread, 422 a path outside the session directory or an id
/// that is not a message id, 404 a message of another thread, 502 Feishu refused the call.
pub async fn download(
  bot: &Bot,
  session_id: &str,
  message_id: &str,
  file_key: &str,
  kind: &str,
  output: &str,
) -> Result<Value, ThreadReplyError> {
  let session = thread_entry::require_thread_session(&FEISHU, &bot.features, session_id).await?;
  let origin: FeishuOrigin = serde_json::from_value(session.origin.clone()).map_err(ThreadReplyError::failure)?;
  let om_id = message_id_of(message_id)
    .map_err(|error| ThreadReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
  let session_dir = bot.features.store.session_dir(session_id);
  let target = output_inside(&session_dir, output)
    .map_err(|error| ThreadReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
  let client = bot.client().map_err(ThreadReplyError::failure)?;
  let message = client.message(om_id).await.map_err(api_failure)?;
  let in_thread = message.get("root_id").and_then(Value::as_str) == Some(origin.root_id.as_str())
    || origin
      .thread_id
      .as_deref()
      .is_some_and(|thread| message.get("thread_id").and_then(Value::as_str) == Some(thread));
  if !in_thread {
    return Err(ThreadReplyError::refused(StatusCode::NOT_FOUND, "the message is not in this session's thread"));
  }
  let bytes = client.download_resource(om_id, file_key, kind).await.map_err(api_failure)?;
  std::fs::write(&target, &bytes)
    .map_err(|error| ThreadReplyError::failure(format!("could not write {}: {error}", target.display())))?;
  Ok(json!({"path": paths::display(&target), "bytes": bytes.len()}))
}

/// The output path, checked to sit inside `session_dir`. The path must be absolute, its parent directory must already
/// exist inside the session directory, and it must not name an existing directory.
fn output_inside(session_dir: &Path, output: &str) -> anyhow::Result<PathBuf> {
  let path = Path::new(output);
  if !path.is_absolute() {
    bail!("the output path must be absolute: {output}");
  }
  if path.components().any(|part| matches!(part, Component::ParentDir)) {
    bail!("the output path must not contain '..': {output}");
  }
  let parent = path.parent().context("the output path has no directory")?;
  let parent =
    parent.canonicalize().with_context(|| format!("the output directory {} does not exist", parent.display()))?;
  let session = session_dir
    .canonicalize()
    .with_context(|| format!("the session directory {} does not exist", session_dir.display()))?;
  if !paths::is_within(&parent, &session) {
    bail!("the output path {output} is outside the session directory");
  }
  Ok(parent.join(path.file_name().context("the output path names no file")?))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::adapter::composite_id;

  #[test]
  fn an_output_path_must_sit_inside_the_session_directory() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("session");
    std::fs::create_dir(&session).unwrap();
    let inside = output_inside(&session, &session.join("image.png").display().to_string()).unwrap();
    assert_eq!(inside.file_name().unwrap(), "image.png");
    let outside = dir.path().join("elsewhere.png");
    assert!(output_inside(&session, &outside.display().to_string()).is_err());
    assert!(output_inside(&session, "relative.png").is_err());
    assert!(output_inside(&session, &session.join("..").join("x.png").display().to_string()).is_err());
  }

  #[test]
  fn a_composite_id_names_its_message_for_the_download_check() {
    assert_eq!(message_id_of(&composite_id("1700000000000", "om_1")).unwrap(), "om_1");
  }
}

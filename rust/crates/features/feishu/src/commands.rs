//! The Feishu server-side commands behind `charliebot feishu read`, `download` and `check`.
//!
//! A default read pages the session's topic and acks its unread messages. Group chat and topic reads are read-only and
//! use the session origin's chat id. Downloads accept any message in that group.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::http::StatusCode;
use charliebot_chat_threads::thread_entry::{self, IdOrdering, ThreadAdapter, ThreadMessage, ThreadReplyError};
use charliebot_core::paths;
use charliebot_core::str_methods;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::adapter::{Bot, FeishuThreadAdapter, api_failure, message_id_of, thread_message};
use crate::client::{FeishuClient, PAGE_SIZE};
use crate::config::FeishuConfig;
use crate::fields::{CHAT_ID, MESSAGE_ID, MESSAGE_TYPE, ROOT_ID, THREAD_ID};
use crate::metadata::FeishuOrigin;
use crate::package::NAME;
use crate::thread_contract::FEISHU;

const TOPIC_PAGE_TOKEN_PREFIX: &str = "charliebot-topic-v1:";

/// Feishu advances past every listed item; the cursor keeps an overflow reply with its topic so paging cannot skip it.
#[derive(Deserialize, Serialize)]
struct TopicPageCursor {
  thread_id: String,
  page_token: Option<String>,
  buffered_message_id: Option<String>,
}

/// The readback's view of one message: ids and topic, listed person's name when available, text, type, and read flags.
fn message_view(message: &ThreadMessage, unread: bool, summon: bool, allowed: &BTreeMap<String, String>) -> Value {
  let meta = message.metadata.as_ref();
  let field = |name: &str| meta.and_then(|meta| meta.get(name)).cloned().unwrap_or(Value::Null);
  let author_id = message.user.clone().or_else(|| message.bot_id.clone());
  json!({
    "id": message.id,
    (MESSAGE_ID): field(MESSAGE_ID),
    (MESSAGE_TYPE): field(MESSAGE_TYPE),
    (THREAD_ID): field(THREAD_ID),
    "author_id": author_id,
    "person": message.user.as_deref().and_then(|user| allowed.get(user)),
    "timestamp": message.created_at,
    "content": message.text,
    "unread": unread,
    "summon": summon,
  })
}

/// The session's pending summons' composite mention ids. A mention is a string id; any other value fails the read.
async fn pending_summon_ids(bot: &Bot, session_id: &str) -> Result<HashSet<String>, ThreadReplyError> {
  let events = thread_entry::load_chat_events(&bot.features, session_id).await?;
  thread_entry::pending_summon_mention_ids(&FEISHU, &bot.features.store, session_id, &events, |mention| {
    mention.as_str().context("a summon's mention id is not a string").map(str::to_string)
  })
  .map_err(ThreadReplyError::failure)
}

/// The read of the session's topic, which acks the unread messages the window returns. The window starts at the
/// earlier of the oldest unread message and the oldest pending summon's message, or holds the newest `limit` messages
/// when neither exists. The summon's root message rides first, outside the count, when the thread's listing does not
/// carry it.
pub async fn read_thread(bot: &Bot, session_id: &str, limit: usize) -> Result<Value, ThreadReplyError> {
  let session = thread_entry::require_thread_session(&FEISHU, &bot.features, session_id).await?;
  let origin: FeishuOrigin = serde_json::from_value(session.origin.clone()).map_err(ThreadReplyError::failure)?;
  let watermark = session.meta.extra(FEISHU.watermark_field).and_then(Value::as_str).map(str::to_string);
  let config = bot.config().map_err(ThreadReplyError::failure)?;
  let allowed = &config.section::<FeishuConfig>(NAME).allowed_users;
  let adapter = Arc::new(FeishuThreadAdapter::new(bot.clone()));
  let thread_messages =
    adapter.thread_messages(&origin.root_id, origin.thread_id.as_deref()).await.map_err(api_failure)?;
  for message in &thread_messages {
    require_thread_message_group(message, &origin.chat_id)?;
  }
  let root = root_message(bot, &origin, &thread_messages).await.map_err(api_failure)?;
  if let Some(root) = &root {
    require_thread_message_group(root, &origin.chat_id)?;
  }
  let pending = pending_summon_ids(bot, session_id).await?;
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
  if let Some(root) = root {
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

/// The read of up to `limit` messages from the session group's main chat, newest first, without changing the topic
/// watermark.
pub async fn read_chat(
  bot: &Bot,
  session_id: &str,
  limit: usize,
  page: Option<&str>,
) -> Result<Value, ThreadReplyError> {
  let session = thread_entry::require_thread_session(&FEISHU, &bot.features, session_id).await?;
  let origin: FeishuOrigin = serde_json::from_value(session.origin.clone()).map_err(ThreadReplyError::failure)?;
  let config = bot.config().map_err(ThreadReplyError::failure)?;
  let allowed = &config.section::<FeishuConfig>(NAME).allowed_users;
  let client = bot.client().map_err(ThreadReplyError::failure)?;
  let mut messages = Vec::with_capacity(limit);
  let mut page_token = page.map(str::to_string);
  let next_page = loop {
    let page_size = (limit - messages.len()).min(PAGE_SIZE);
    let page = client.chat_messages(&origin.chat_id, page_size, page_token.as_deref()).await.map_err(api_failure)?;
    for item in &page.items {
      require_item_group(item, &origin.chat_id)?;
    }
    messages.extend(
      page
        .items
        .iter()
        .map(|item| thread_message(item).map_err(ThreadReplyError::failure))
        .collect::<Result<Vec<_>, _>>()?,
    );
    if messages.len() >= limit || page.next_page_token.is_none() {
      break page.next_page_token;
    }
    page_token = page.next_page_token;
  };
  let pending = pending_summon_ids(bot, session_id).await?;
  let mut output = json!({
    "messages": messages
      .iter()
      .map(|message| message_view(message, false, pending.contains(&message.id), allowed))
      .collect::<Vec<_>>(),
  });
  if let Some(token) = next_page {
    output["next_page"] = json!(token);
  }
  Ok(output)
}

/// The read of up to `limit` messages in one topic of the session's group, oldest first, without changing its
/// watermark.
pub async fn read_topic(
  bot: &Bot,
  session_id: &str,
  thread_id: &str,
  limit: usize,
  page: Option<&str>,
) -> Result<Value, ThreadReplyError> {
  let session = thread_entry::require_thread_session(&FEISHU, &bot.features, session_id).await?;
  let origin: FeishuOrigin = serde_json::from_value(session.origin.clone()).map_err(ThreadReplyError::failure)?;
  let config = bot.config().map_err(ThreadReplyError::failure)?;
  let allowed = &config.section::<FeishuConfig>(NAME).allowed_users;
  let client = bot.client().map_err(ThreadReplyError::failure)?;
  let cursor = page.map(|token| decode_topic_page_cursor(token, thread_id)).transpose()?.unwrap_or(TopicPageCursor {
    thread_id: thread_id.to_string(),
    page_token: None,
    buffered_message_id: None,
  });
  let mut page_token = cursor.page_token;
  let mut buffered_message_id = None;
  let mut messages = Vec::with_capacity(limit);
  if let Some(message_id) = cursor.buffered_message_id {
    let item = client.message(&message_id).await.map_err(api_failure)?;
    require_item_group(&item, &origin.chat_id)?;
    messages.push(thread_message(&item).map_err(ThreadReplyError::failure)?);
  }
  let mut first_page = page.is_none();
  while messages.len() < limit && (first_page || page_token.is_some()) {
    let page_size = (limit - messages.len()).min(PAGE_SIZE);
    let response =
      client.thread_messages_sized(thread_id, page_size, page_token.as_deref()).await.map_err(api_failure)?;
    for item in &response.items {
      require_item_group(item, &origin.chat_id)?;
    }
    if first_page {
      let root_id = response
        .items
        .iter()
        .find_map(|item| item.get(ROOT_ID).filter(|value| !value.is_null()))
        .map(|value| {
          value.as_str().context("a Feishu topic message has a non-string root_id").map_err(ThreadReplyError::failure)
        })
        .transpose()?;
      if let Some(root_id) = root_id
        && !response.items.iter().any(|item| item.get(MESSAGE_ID).and_then(Value::as_str) == Some(root_id))
      {
        let root = client.message(root_id).await.map_err(api_failure)?;
        require_item_group(&root, &origin.chat_id)?;
        messages.push(thread_message(&root).map_err(ThreadReplyError::failure)?);
      }
      first_page = false;
    }
    messages.extend(
      response
        .items
        .iter()
        .map(|item| thread_message(item).map_err(ThreadReplyError::failure))
        .collect::<Result<Vec<_>, _>>()?,
    );
    page_token = response.next_page_token;
    if messages.len() > limit {
      let overflow = messages.pop().expect("a topic page over the limit has a last message");
      buffered_message_id = Some(message_id_of(&overflow.id).map_err(ThreadReplyError::failure)?.to_string());
    }
  }
  if messages.is_empty() {
    return Err(ThreadReplyError::refused(StatusCode::NOT_FOUND, "the topic has no message in this session's group"));
  }
  let pending = pending_summon_ids(bot, session_id).await?;
  let mut output = json!({
    "messages": messages
      .iter()
      .map(|message| message_view(message, false, pending.contains(&message.id), allowed))
      .collect::<Vec<_>>(),
  });
  if let Some(token) = topic_next_page_token(thread_id, page_token, buffered_message_id)? {
    output["next_page"] = json!(token);
  }
  Ok(output)
}

fn decode_topic_page_cursor(token: &str, thread_id: &str) -> Result<TopicPageCursor, ThreadReplyError> {
  let Some(encoded) = token.strip_prefix(TOPIC_PAGE_TOKEN_PREFIX) else {
    return Ok(TopicPageCursor {
      thread_id: thread_id.to_string(),
      page_token: Some(token.to_string()),
      buffered_message_id: None,
    });
  };
  if encoded.is_empty() || encoded.len() % 2 != 0 {
    return Err(invalid_topic_page_token());
  }
  let mut bytes = Vec::with_capacity(encoded.len() / 2);
  for pair in encoded.as_bytes().as_chunks::<2>().0 {
    let high = str_methods::hex_value(char::from(pair[0])).ok_or_else(invalid_topic_page_token)?;
    let low = str_methods::hex_value(char::from(pair[1])).ok_or_else(invalid_topic_page_token)?;
    bytes.push((high << 4) | low);
  }
  let cursor: TopicPageCursor = serde_json::from_slice(&bytes).map_err(|_| invalid_topic_page_token())?;
  if cursor.thread_id != thread_id || (cursor.page_token.is_none() && cursor.buffered_message_id.is_none()) {
    return Err(invalid_topic_page_token());
  }
  Ok(cursor)
}

fn topic_next_page_token(
  thread_id: &str,
  page_token: Option<String>,
  buffered_message_id: Option<String>,
) -> Result<Option<String>, ThreadReplyError> {
  if buffered_message_id.is_none()
    && !page_token.as_deref().is_some_and(|token| token.starts_with(TOPIC_PAGE_TOKEN_PREFIX))
  {
    return Ok(page_token);
  }
  let cursor = TopicPageCursor { thread_id: thread_id.to_string(), page_token, buffered_message_id };
  let bytes = serde_json::to_vec(&cursor).map_err(ThreadReplyError::failure)?;
  let digits = b"0123456789abcdef";
  let mut token = String::with_capacity(TOPIC_PAGE_TOKEN_PREFIX.len() + bytes.len() * 2);
  token.push_str(TOPIC_PAGE_TOKEN_PREFIX);
  for byte in bytes {
    token.push(digits[(byte >> 4) as usize] as char);
    token.push(digits[(byte & 0x0f) as usize] as char);
  }
  Ok(Some(token))
}

fn invalid_topic_page_token() -> ThreadReplyError {
  ThreadReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "the topic page token is invalid")
}

fn require_item_group(item: &Value, chat_id: &str) -> Result<(), ThreadReplyError> {
  require_chat_id(item.get(CHAT_ID).and_then(Value::as_str), chat_id)
}

fn require_thread_message_group(message: &ThreadMessage, chat_id: &str) -> Result<(), ThreadReplyError> {
  require_chat_id(message.metadata.as_ref().and_then(|metadata| metadata.get(CHAT_ID)).and_then(Value::as_str), chat_id)
}

fn require_chat_id(item_chat_id: Option<&str>, chat_id: &str) -> Result<(), ThreadReplyError> {
  let item_chat_id = item_chat_id.context("a Feishu message has no chat_id").map_err(ThreadReplyError::failure)?;
  if item_chat_id == chat_id {
    Ok(())
  } else {
    Err(ThreadReplyError::refused(StatusCode::NOT_FOUND, "the message is not in this session's group"))
  }
}

/// The summon's root message, when the thread listing does not already carry it (`root_message`). The root holds the
/// summon's text in a thread the bot started from a message, and it is read with the thread's eligibility.
async fn root_message(
  bot: &Bot,
  origin: &FeishuOrigin,
  thread_messages: &[ThreadMessage],
) -> anyhow::Result<Option<ThreadMessage>> {
  if thread_messages.iter().any(|message| message_id_of(&message.id).ok() == Some(origin.root_id.as_str())) {
    return Ok(None);
  }
  let client = bot.client()?;
  let item = client.message(&origin.root_id).await?;
  Ok(Some(thread_message(&item)?))
}

/// The readback of `charliebot feishu check`: each setup check with its outcome, and `ok` when all of them pass. The
/// checks are: a tenant token, the bot's open_id, the long-connection endpoint, and a non-empty owner list
/// (`allowed_users`). Refusals: 409 when the app id or secret is unset.
pub async fn check_setup(bot: &Bot) -> Result<Value, ThreadReplyError> {
  let credentials = bot.features.credentials.get().map_err(ThreadReplyError::failure)?;
  for key in ["app_id", "app_secret"] {
    if credentials.get(NAME, key).is_none() {
      return Err(ThreadReplyError::refused(StatusCode::CONFLICT, format!("credentials.feishu.{key} is not set")));
    }
  }
  let client = bot.client().map_err(ThreadReplyError::failure)?;
  let config = bot.config().map_err(ThreadReplyError::failure)?;
  let allowed = &config.section::<FeishuConfig>(NAME).allowed_users;
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

/// The download of one image or file from the session's group into its session directory. Refusals: 404 unknown
/// session or message from another group, 409 no Feishu thread, 422 invalid message id or output path, and 502 Feishu
/// refusal.
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
  require_item_group(&message, &origin.chat_id)?;
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
  use crate::test_support::{Answer, Home, Recorded, serve};
  use serde_json::json;
  use std::sync::Arc;

  const CHAT_ID: &str = "oc_group";
  const OTHER_CHAT_ID: &str = "oc_other_group";
  const TOPIC_ID: &str = "omt_other_topic";
  const SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";
  const USER_ID: &str = "ou_0000000000000000000000000000000000000001";

  fn origin() -> Value {
    json!({"tenant_key": "tenant", "chat_id": CHAT_ID, "root_id": "om_summon", "thread_id": "omt_summon"})
  }

  fn message(
    id: &str,
    created_at: &str,
    chat_id: &str,
    thread_id: Option<&str>,
    root_id: Option<&str>,
    content: &str,
  ) -> Value {
    json!({
      "message_id": id,
      "chat_id": chat_id,
      "thread_id": thread_id,
      "root_id": root_id,
      "msg_type": "text",
      "create_time": created_at,
      "sender": {"sender_type": "user", "id": USER_ID},
      "body": {"content": json!({"text": content}).to_string()},
    })
  }

  fn list(items: Vec<Value>, more: bool, token: Option<&str>) -> Answer {
    Answer::json(json!({
      "code": 0,
      "data": {"items": items, "has_more": more, "page_token": token},
    }))
  }

  fn api_error(status: u16, code: i64, message: &str) -> Answer {
    Answer {
      status,
      content_type: "application/json",
      body: json!({"code": code, "msg": message}).to_string().into_bytes(),
    }
  }

  fn response_items(items: Vec<Value>) -> Answer {
    Answer::json(json!({"code": 0, "data": {"items": items}}))
  }

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

  #[tokio::test]
  async fn group_chat_reads_are_newest_first_paged_and_leave_the_topic_watermark_untouched() {
    let home = Home::new();
    let watermark = "1700000000000-om_seen";
    let session_id = home.thread_session(SESSION_ID, origin(), Some(watermark)).await;
    let first = vec![
      message("om_new", "1700000002000", CHAT_ID, Some("omt_new_topic"), None, "newest"),
      message("om_middle", "1700000001000", CHAT_ID, None, None, "middle"),
    ];
    let older = vec![message("om_old", "1700000000000", CHAT_ID, None, None, "older")];
    let first_page = first.clone();
    let older_page = older.clone();
    let stub = serve(move |request| {
      if request.target.contains("page_token=older") {
        list(older_page.clone(), false, None)
      } else {
        list(first_page.clone(), true, Some("older"))
      }
    })
    .await;
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());
    let before = home.meta(&session_id).await.extra(crate::metadata::FEISHU_WATERMARK_ID).cloned();

    let first_read = read_chat(&bot, &session_id, 2, None).await.unwrap();
    assert_eq!(first_read["messages"].as_array().unwrap().len(), 2);
    assert_eq!(first_read["messages"][0]["content"], "newest");
    assert_eq!(first_read["messages"][0]["author_id"], USER_ID);
    assert_eq!(first_read["messages"][0]["timestamp"], crate::adapter::rfc3339_of_millis("1700000002000").unwrap());
    assert_eq!(first_read["messages"][0]["thread_id"], "omt_new_topic");
    assert_eq!(first_read["messages"][1]["thread_id"], Value::Null);
    assert_eq!(first_read["next_page"], "older");

    let second_read = read_chat(&bot, &session_id, 2, Some("older")).await.unwrap();
    assert_eq!(second_read["messages"][0]["content"], "older");
    assert!(second_read.get("next_page").is_none());
    assert_eq!(home.meta(&session_id).await.extra(crate::metadata::FEISHU_WATERMARK_ID).cloned(), before);

    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].target.contains("container_id_type=chat"));
    assert!(requests[0].target.contains("sort_type=ByCreateTimeDesc"));
    assert!(requests[0].target.contains("page_size=2"));
    assert!(requests[1].target.contains("page_token=older"));
    assert!(requests.iter().all(|request| !request.target.contains("/reactions")));
  }

  #[tokio::test]
  async fn group_chat_reads_fetch_a_second_api_page_to_fill_a_larger_limit() {
    let home = Home::new();
    let session_id = home.thread_session(SESSION_ID, origin(), Some("1700000000000-om_seen")).await;
    let first_page = (0..50)
      .map(|index| message(&format!("om_first_{index}"), "1700000002000", CHAT_ID, None, None, "first page"))
      .collect::<Vec<_>>();
    let second_page = (0..10)
      .map(|index| message(&format!("om_second_{index}"), "1700000001000", CHAT_ID, None, None, "second page"))
      .collect::<Vec<_>>();
    let first = first_page.clone();
    let second = second_page.clone();
    let stub = serve(move |request| {
      if request.target.contains("page_token=page_two") {
        list(second.clone(), true, Some("page_three"))
      } else {
        list(first.clone(), true, Some("page_two"))
      }
    })
    .await;
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());

    let readback = read_chat(&bot, &session_id, 60, None).await.unwrap();
    assert_eq!(readback["messages"].as_array().unwrap().len(), 60);
    assert_eq!(readback["next_page"], "page_three");
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].target.contains("page_size=50"));
    assert!(requests[1].target.contains("page_size=10"));
    assert!(requests[1].target.contains("page_token=page_two"));
  }

  #[tokio::test]
  async fn topic_reads_pages_without_changing_the_watermark() {
    let home = Home::new();
    let watermark = "1700000000000-om_seen";
    let session_id = home.thread_session(SESSION_ID, origin(), Some(watermark)).await;
    let reply_one =
      message("om_reply_one", "1700000000000", CHAT_ID, Some(TOPIC_ID), Some("om_topic_root"), "reply one");
    let reply_two =
      message("om_reply_two", "1700000001000", CHAT_ID, Some(TOPIC_ID), Some("om_topic_root"), "reply two");
    let reply_three =
      message("om_reply_three", "1700000002000", CHAT_ID, Some(TOPIC_ID), Some("om_topic_root"), "reply three");
    let first = vec![reply_one.clone(), reply_two.clone()];
    let second = reply_two.clone();
    let third = reply_three.clone();
    let stub = serve(move |request: &Recorded| {
      if request.target.starts_with("/open-apis/im/v1/messages/om_topic_root") {
        response_items(vec![message("om_topic_root", "1699999999000", CHAT_ID, Some(TOPIC_ID), None, "topic root")])
      } else if request.target.starts_with("/open-apis/im/v1/messages/om_reply_two") {
        response_items(vec![second.clone()])
      } else if request.target.contains("page_token=topic_two") {
        list(vec![third.clone()], false, None)
      } else {
        list(first.clone(), true, Some("topic_two"))
      }
    })
    .await;
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());

    let readback = read_topic(&bot, &session_id, TOPIC_ID, 2, None).await.unwrap();
    assert_eq!(
      readback["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>(),
      vec!["topic root", "reply one"]
    );
    assert!(readback["messages"].as_array().unwrap().len() <= 2);
    assert_eq!(readback["messages"][0]["thread_id"], TOPIC_ID);
    let next_page = readback["next_page"].as_str().unwrap();
    assert!(next_page.starts_with(TOPIC_PAGE_TOKEN_PREFIX));
    let wrong_topic = read_topic(&bot, &session_id, "omt_wrong_topic", 2, Some(next_page)).await.unwrap_err();
    assert_eq!(wrong_topic.status, StatusCode::UNPROCESSABLE_ENTITY);

    let second_read = read_topic(&bot, &session_id, TOPIC_ID, 2, Some(next_page)).await.unwrap();
    assert_eq!(
      second_read["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>(),
      vec!["reply two", "reply three"]
    );
    assert!(second_read.get("next_page").is_none());
    {
      let requests = stub.requests.lock().unwrap();
      assert!(requests.iter().any(|request| {
        request.target.contains("container_id_type=thread") && request.target.contains("sort_type=ByCreateTimeAsc")
      }));
      assert!(requests.iter().any(|request| request.target.contains("page_size=2")));
      assert!(requests.iter().any(|request| request.target.contains("page_token=topic_two")));
    }

    assert_eq!(
      home.meta(&session_id).await.extra(crate::metadata::FEISHU_WATERMARK_ID).cloned(),
      Some(json!(watermark))
    );
  }

  #[tokio::test]
  async fn topic_reads_reject_other_groups_and_feishu_errors() {
    let home = Home::new();
    let watermark = "1700000000000-om_seen";
    let session_id = home.thread_session(SESSION_ID, origin(), Some(watermark)).await;
    let other =
      message("om_cross", "1700000000000", OTHER_CHAT_ID, Some("omt_cross"), Some("om_cross_root"), "private");
    let stub = serve(move |request: &Recorded| {
      if request.target.contains("container_id=omt_cross") {
        list(vec![other.clone()], false, None)
      } else {
        api_error(400, 230027, "permission denied")
      }
    })
    .await;
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());

    let cross_error = read_topic(&bot, &session_id, "omt_cross", 10, None).await.unwrap_err();
    assert_eq!(cross_error.status, StatusCode::NOT_FOUND);
    assert_eq!(error_text(&cross_error), "the message is not in this session's group");
    let feishu_error = read_topic(&bot, &session_id, "omt_refused", 10, None).await.unwrap_err();
    assert_eq!(feishu_error.status, StatusCode::BAD_GATEWAY);
    assert!(error_text(&feishu_error).contains("permission denied"));
    assert_eq!(
      home.meta(&session_id).await.extra(crate::metadata::FEISHU_WATERMARK_ID).cloned(),
      Some(json!(watermark))
    );
  }

  #[tokio::test]
  async fn chat_and_default_topic_reads_refuse_messages_from_another_group() {
    let home = Home::new();
    let session_id = home.thread_session(SESSION_ID, origin(), Some("1700000000000-om_seen")).await;
    let foreign = message("om_foreign", "1700000001000", OTHER_CHAT_ID, Some("omt_foreign"), None, "private");
    let foreign_list = foreign.clone();
    let stub = serve(move |request| {
      if request.target.contains("container_id_type=chat") {
        list(vec![foreign_list.clone()], false, None)
      } else {
        list(vec![foreign.clone()], false, None)
      }
    })
    .await;
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());

    assert_eq!(read_chat(&bot, &session_id, 10, None).await.unwrap_err().status, StatusCode::NOT_FOUND);
    assert_eq!(read_thread(&bot, &session_id, 10).await.unwrap_err().status, StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  async fn a_default_read_maps_feishu_refusals_to_bad_gateway() {
    let home = Home::new();
    let session_id = home.thread_session(SESSION_ID, origin(), Some("1700000000000-om_seen")).await;
    let stub = serve(move |_| api_error(400, 230027, "permission denied")).await;
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());

    let error = read_thread(&bot, &session_id, 10).await.unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    assert!(error_text(&error).contains("permission denied"));
  }

  #[tokio::test]
  async fn a_default_read_checks_the_root_group_before_acknowledging_unread_replies() {
    let home = Home::new();
    let mut foreign_origin = origin();
    foreign_origin["root_id"] = json!("om_foreign_root");
    let session_id = home.thread_session(SESSION_ID, foreign_origin, Some("1699999999000-om_seen")).await;
    let reply = message("om_reply", "1700000000000", CHAT_ID, Some("omt_summon"), Some("om_foreign_root"), "reply");
    let foreign_root = message("om_foreign_root", "1699999998000", OTHER_CHAT_ID, Some("omt_summon"), None, "foreign");
    let root = foreign_root.clone();
    let stub = serve(move |request: &Recorded| {
      if request.target.starts_with("/open-apis/im/v1/messages/om_foreign_root") {
        response_items(vec![root.clone()])
      } else {
        list(vec![reply.clone()], false, None)
      }
    })
    .await;
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());
    let before = home.meta(&session_id).await.extra(crate::metadata::FEISHU_WATERMARK_ID).cloned();

    let error = read_thread(&bot, &session_id, 10).await.unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
    assert_eq!(home.meta(&session_id).await.extra(crate::metadata::FEISHU_WATERMARK_ID).cloned(), before);
  }

  #[tokio::test]
  async fn downloads_accept_any_message_in_the_group_and_reject_another_group() {
    let home = Home::new();
    let session_id = home.thread_session(SESSION_ID, origin(), None).await;
    let session_dir = home.features.store.session_dir(&session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let image = b"synthetic-image".to_vec();
    let response_image = image.clone();
    let stub = serve(move |request| {
      if request.target == "/open-apis/im/v1/messages/om_main" {
        response_items(vec![message("om_main", "1700000000000", CHAT_ID, None, None, "image")])
      } else if request.target == "/open-apis/im/v1/messages/om_other" {
        response_items(vec![message("om_other", "1700000000000", OTHER_CHAT_ID, None, None, "image")])
      } else {
        Answer { status: 200, content_type: "application/octet-stream", body: response_image.clone() }
      }
    })
    .await;
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());
    let output = session_dir.join("image.png");

    download(&bot, &session_id, "1700000000000-om_main", "img_key", "image", &output.display().to_string())
      .await
      .unwrap();
    assert_eq!(std::fs::read(&output).unwrap(), image);
    let foreign = session_dir.join("foreign.png");
    assert_eq!(
      download(&bot, &session_id, "1700000000000-om_other", "img_key", "image", &foreign.display().to_string())
        .await
        .unwrap_err()
        .status,
      StatusCode::NOT_FOUND
    );
    assert!(!foreign.exists());
    assert_eq!(
      stub.requests.lock().unwrap().iter().filter(|request| request.target.contains("/resources/")).count(),
      1
    );
  }

  #[tokio::test]
  async fn default_read_keeps_its_existing_fields_and_adds_thread_id() {
    let home = Home::new();
    let watermark = "1700000000000-om_plain";
    let mut plain_origin = origin();
    plain_origin["root_id"] = json!("om_plain");
    let session_id = home.thread_session(SESSION_ID, plain_origin, Some(watermark)).await;
    let item = message("om_plain", "1700000000000", CHAT_ID, Some("omt_summon"), None, "existing output");
    let response = item.clone();
    let stub = serve(move |_| list(vec![response.clone()], false, None)).await;
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());

    let readback = read_thread(&bot, &session_id, 50).await.unwrap();
    assert_eq!(
      readback,
      json!({
        "messages": [{
          "id": "1700000000000-om_plain",
          "message_id": "om_plain",
          "message_type": "text",
          "thread_id": "omt_summon",
          "author_id": USER_ID,
          "person": "tester",
          "timestamp": crate::adapter::rfc3339_of_millis("1700000000000").unwrap(),
          "content": "existing output",
          "unread": false,
          "summon": false,
        }],
        "watermark_id": watermark,
        "more_unread": 0,
      })
    );
  }

  fn error_text(error: &ThreadReplyError) -> &str {
    match &error.detail {
      thread_entry::ThreadDetail::Text(text) => text,
      thread_entry::ThreadDetail::Structured(_) => panic!("expected a text error"),
    }
  }
}

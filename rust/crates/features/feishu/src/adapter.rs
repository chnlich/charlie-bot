//! The Feishu side of the shared thread core: the session namespace, the composite message id, the bot handle, and the
//! `FeishuThreadAdapter` the core works through. The core's entries for the reply and the summon paths are wrapped here
//! with the bot's handle.
//!
//! A Feishu message id is `om_…` and carries no order, so the adapter names each message `<create_time ms>-<om_id>`.
//! Only this module builds and splits that composite; the core orders it through `IdOrdering::TimeThenId`.

use std::sync::Arc;

use anyhow::{Context, bail};
use charliebot_chat_threads::thread_entry::{self, Summon, ThreadAdapter, ThreadMessage, ThreadPlatform};
use charliebot_compat::http_client;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::FeatureContext;
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::client::{BASE_URL, FeishuAPIError, FeishuClient, Page};
use crate::config::FeishuConfig;
use crate::event::{self, Inbound, Message};
use crate::fields::{
  CHAT_ID, CREATE_TIME, MENTION_ID, MESSAGE_ID, MESSAGE_TYPE, ROOT_ID, SENDER, SENDER_TYPE, THREAD_ID,
};
use crate::metadata::{FEISHU_ORIGIN, FeishuOrigin};
use crate::owners;
use crate::package::NAME;
use crate::thread_contract::FEISHU;

/// The namespace of Feishu session ids. Changing it would orphan every existing Feishu-backed session.
const FEISHU_NS: Uuid = Uuid::from_u128(0x5b1e0c2a_7f4d_4a7e_9d3b_6c8f1a2e4d70);
/// The reaction the bot lights on a summon: Feishu's `OnIt` emoji, which means "received".
pub const ACK_EMOJI: &str = "OnIt";
/// The label prefix of a session created from a Feishu summon; the chat id follows it.
const SESSION_LABEL: &str = "Feishu · ";

/// The deterministic session id of the Feishu thread a summon names (`summon_session_id`). Its key is known before the
/// thread exists, because the summon's own message id stands in for the root until the first reply.
pub fn summon_session_id(tenant_key: &str, chat_id: &str, root_id: &str) -> String {
  Uuid::new_v5(&FEISHU_NS, format!("feishu:{tenant_key}:{chat_id}:{root_id}").as_bytes()).to_string()
}

/// The composite id of one message: its creation time in milliseconds, a dash, and its `om_` id.
pub fn composite_id(create_time: &str, message_id: &str) -> String {
  format!("{create_time}-{message_id}")
}

/// The `om_` id of a composite id.
pub fn message_id_of(composite: &str) -> anyhow::Result<&str> {
  match composite.split_once('-') {
    Some((millis, message_id)) if is_millis(millis) && !message_id.is_empty() => Ok(message_id),
    _ => bail!("a Feishu message id is <milliseconds>-<message id>, got {composite:?}"),
  }
}

/// Whether a composite id's millisecond part is digits only (`is_millis`).
pub fn is_millis(text: &str) -> bool {
  !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The RFC 3339 time of a millisecond string, as the core's timestamp compare reads it.
pub(super) fn rfc3339_of_millis(millis: &str) -> anyhow::Result<String> {
  let millis: i128 = millis.parse().with_context(|| format!("create_time is not milliseconds: {millis:?}"))?;
  let time = OffsetDateTime::from_unix_timestamp_nanos(millis * 1_000_000)
    .with_context(|| format!("create_time is out of range: {millis}"))?;
  Ok(time.format(&Rfc3339)?)
}

/// The bot's handle on Feishu: the feature context that reads the config and the credentials on each call, and the
/// Open API base URL (`BASE_URL` in production).
#[derive(Clone)]
pub struct Bot {
  pub(crate) features: Arc<FeatureContext>,
  base_url: String,
}

impl Bot {
  pub fn new(features: Arc<FeatureContext>, base_url: impl Into<String>) -> Self {
    Self { features, base_url: base_url.into() }
  }

  /// The app's client, built on `credentials.feishu.app_id` and `app_secret`. A missing value fails the call.
  pub fn client(&self) -> anyhow::Result<FeishuClient> {
    let credentials = self.features.credentials.get()?;
    let app_id = credentials.require(NAME, "app_id")?;
    let app_secret = credentials.require(NAME, "app_secret")?;
    let http = http_client::client(&http_client::Settings::from_env())?;
    Ok(FeishuClient::new(http, self.base_url.clone(), app_id, app_secret))
  }

  /// The app id, which is the sender id of every message the bot posts.
  pub fn app_id(&self) -> anyhow::Result<String> {
    Ok(self.features.credentials.get()?.require(NAME, "app_id")?)
  }

  /// The loaded config, read for one call.
  pub(crate) fn config(&self) -> anyhow::Result<Arc<CharlieBotConfig>> {
    Ok(self.features.config.get()?)
  }
}

/// The Feishu thread's `ThreadAdapter` (`FeishuThreadAdapter`). The client is built on the first platform call and reused.
pub struct FeishuThreadAdapter {
  bot: Bot,
  client: OnceCell<FeishuClient>,
}

impl FeishuThreadAdapter {
  pub fn new(bot: Bot) -> Self {
    Self { bot, client: OnceCell::new() }
  }

  /// The adapter over a client the listener already holds.
  pub fn with_client(bot: Bot, client: FeishuClient) -> Self {
    let cell = OnceCell::new();
    assert!(cell.set(client).is_ok(), "the Feishu client is inserted into an empty slot");
    Self { bot, client: cell }
  }

  /// The client every platform call posts through, built once.
  pub async fn client(&self) -> anyhow::Result<&FeishuClient> {
    self.client.get_or_try_init(|| async { self.bot.client() }).await
  }

  /// Every message of a thread, oldest first. The thread id comes from the origin when it holds one, else from the root
  /// message, which names its thread once the first reply has created it. A thread that does not exist yet has no
  /// messages.
  pub(crate) async fn thread_messages(
    &self,
    root_id: &str,
    thread_id: Option<&str>,
  ) -> anyhow::Result<Vec<ThreadMessage>> {
    let client = self.client().await?;
    let thread_id = match thread_id {
      Some(thread_id) => thread_id.to_string(),
      None => match client.message(root_id).await?.get(THREAD_ID).and_then(Value::as_str) {
        Some(thread_id) => thread_id.to_string(),
        None => return Ok(Vec::new()),
      },
    };
    let mut messages = Vec::new();
    let mut page_token = None;
    loop {
      let Page { items, next_page_token } = client.thread_messages(&thread_id, page_token.as_deref()).await?;
      for item in &items {
        messages.push(thread_message(item)?);
      }
      match next_page_token {
        Some(token) => page_token = Some(token),
        None => return Ok(messages),
      }
    }
  }
}

/// Whether a reaction in a message's reaction list is one this app added: its operator is the app itself.
fn is_own_app_reaction(item: &Value, app_id: &str) -> bool {
  let operator = item.get("operator");
  let operator_type = operator.and_then(|operator| operator.get("operator_type")).and_then(Value::as_str);
  let operator_id = operator.and_then(|operator| operator.get("operator_id")).and_then(Value::as_str);
  operator_type == Some("app") && operator_id == Some(app_id)
}

/// One thread message from the message list: the composite id, the sender, the text, and the eligibility the follow
/// and the readback share (`eligible_message`). Every non-deleted message from a human is eligible; the group's
/// admission is checked where the thread is read. The bot's own posts carry the app id as `bot_id`.
pub fn thread_message(item: &Value) -> anyhow::Result<ThreadMessage> {
  let text_of = |object: &Value, name: &str| -> anyhow::Result<String> {
    Ok(object.get(name).and_then(Value::as_str).with_context(|| format!("a message has no {name}"))?.to_string())
  };
  let message_id = text_of(item, MESSAGE_ID)?;
  let create_time = text_of(item, CREATE_TIME)?;
  let sender = item.get(SENDER).context("a message has no sender")?;
  let sender_type = text_of(sender, SENDER_TYPE)?;
  let sender_id = text_of(sender, "id")?;
  let thread_id = item
    .get(THREAD_ID)
    .filter(|value| !value.is_null())
    .map(|value| value.as_str().context("a message thread_id is not a string"))
    .transpose()?;
  let deleted = item.get("deleted").and_then(Value::as_bool).unwrap_or(false);
  let text = if deleted {
    String::new()
  } else {
    let message_type = message_type_of(item)?;
    let content = item.get("body").and_then(|body| body.get("content")).and_then(Value::as_str);
    let content = content.with_context(|| format!("message {message_id} has no body content"))?;
    event::render_text(message_type, content, &event::mentions_of(item)?)?
  };
  let is_user = sender_type == "user";
  Ok(ThreadMessage {
    id: composite_id(&create_time, &message_id),
    user: is_user.then(|| sender_id.clone()),
    text,
    created_at: Some(rfc3339_of_millis(&create_time)?),
    metadata: Some(json!({
      (MESSAGE_ID): message_id,
      (MESSAGE_TYPE): message_type_of(item)?,
      (CHAT_ID): item.get(CHAT_ID).and_then(Value::as_str),
      (THREAD_ID): thread_id,
    })),
    bot_id: (sender_type == "app").then_some(sender_id.clone()),
    eligible: is_user && !deleted,
  })
}

impl ThreadAdapter for FeishuThreadAdapter {
  fn platform(&self) -> &ThreadPlatform {
    &FEISHU
  }

  fn post<'a>(&'a self, address: &'a Value, text: &'a str, post_key: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let chat_id = str_field(address, CHAT_ID)?;
      let config = self.bot.config()?;
      let owners = &config.section::<FeishuConfig>(NAME).allowed_users;
      let client = self.client().await?;
      if !owners::chat_has_owner(client, owners, chat_id).await? {
        bail!("chat {chat_id} has no listed owner among its members, so the reply is refused");
      }
      let uuid = Uuid::new_v5(&FEISHU_NS, post_key.as_bytes()).to_string();
      client.reply_in_thread(str_field(address, ROOT_ID)?, text, &uuid).await?;
      Ok(())
    })
  }

  fn find_own_post_by_key<'a>(
    &'a self,
    address: &'a Value,
    _post_key: &'a str,
    text: &'a str,
    created_after: &'a str,
    messages: Option<&'a [ThreadMessage]>,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async move {
      let app_id = self.bot.app_id()?;
      let thread = match messages {
        Some(messages) => messages.to_vec(),
        None => {
          let thread_id = address.get(THREAD_ID).and_then(Value::as_str);
          self.thread_messages(str_field(address, ROOT_ID)?, thread_id).await?
        }
      };
      let created_after = UtcDateTime::parse(created_after)?;
      for message in thread {
        if message.bot_id.as_deref() == Some(app_id.as_str()) && message.text == text {
          let timestamp = message.created_at.as_deref().context("a Feishu thread message has no timestamp")?;
          if UtcDateTime::parse(timestamp)? >= created_after {
            return Ok(true);
          }
        }
      }
      Ok(false)
    })
  }

  fn add_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let message_id = message_id_of(str_field(block, FEISHU.mention_key)?)?;
      self.client().await?.add_reaction(message_id, ACK_EMOJI).await?;
      Ok(())
    })
  }

  /// The deletion of the reactions this app added to the summon. Feishu deletes a reaction by its `reaction_id`, which
  /// the list call returns. So the bot finds its own `OnIt` among the message's reactions (operator type `app`,
  /// operator id the app id) and deletes each one.
  fn remove_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let message_id = message_id_of(str_field(block, FEISHU.mention_key)?)?;
      let app_id = self.bot.app_id()?;
      let client = self.client().await?;
      let mut page_token = None;
      loop {
        let Page { items, next_page_token } = client.reactions(message_id, ACK_EMOJI, page_token.as_deref()).await?;
        for item in items.iter().filter(|item| is_own_app_reaction(item, &app_id)) {
          client.delete_reaction(message_id, str_field(item, "reaction_id")?).await?;
        }
        match next_page_token {
          Some(token) => page_token = Some(token),
          None => return Ok(()),
        }
      }
    })
  }

  fn read_thread<'a>(
    &'a self,
    origin: &'a Value,
    _config: &'a CharlieBotConfig,
  ) -> BoxFuture<'a, anyhow::Result<Vec<ThreadMessage>>> {
    Box::pin(async move {
      let origin = origin_of(origin)?;
      self.thread_messages(&origin.root_id, origin.thread_id.as_deref()).await
    })
  }

  fn address_of(&self, origin: &Value) -> Value {
    let origin = origin_of(origin).expect("a stored feishu_origin names its chat and root");
    let mut address = Map::new();
    address.insert(CHAT_ID.to_string(), json!(origin.chat_id));
    address.insert(ROOT_ID.to_string(), json!(origin.root_id));
    if let Some(thread_id) = origin.thread_id {
      address.insert(THREAD_ID.to_string(), json!(thread_id));
    }
    Value::Object(address)
  }

  fn log_fields(&self, address: &Value) -> Value {
    let thread = address.get(THREAD_ID).filter(|value| !value.is_null()).unwrap_or(&address[ROOT_ID]);
    json!({"chat": address[CHAT_ID], "thread": thread})
  }
}

/// The session's origin as the adapter reads it. The slot validator has checked its fields on every write.
fn origin_of(origin: &Value) -> anyhow::Result<FeishuOrigin> {
  serde_json::from_value(origin.clone()).context("feishu_origin is not a Feishu origin")
}

/// A string field of a stored block or address.
fn str_field<'a>(object: &'a Value, name: &str) -> anyhow::Result<&'a str> {
  object.get(name).and_then(Value::as_str).with_context(|| format!("the Feishu block names no {name}"))
}

/// The message type a list item carries (`msg_type`).
fn message_type_of(item: &Value) -> anyhow::Result<&str> {
  item.get("msg_type").and_then(Value::as_str).context("a message has no msg_type")
}

/// The adapter the wrappers hand the shared core.
fn adapter(bot: &Bot) -> Arc<dyn ThreadAdapter> {
  Arc::new(FeishuThreadAdapter::new(bot.clone()))
}

/// The `post_reply` entry point: it posts `text` into the session's Feishu thread and returns the readback the CLI
/// prints.
pub async fn post_reply_with_caller_context(
  bot: &Bot,
  session_id: &str,
  text: &str,
  request_id: Option<&str>,
  run_started_at: Option<&str>,
) -> Result<Value, thread_entry::ThreadReplyError> {
  let readback = thread_entry::post_reply_with_caller_context(
    &adapter(bot),
    &bot.features,
    session_id,
    text,
    request_id,
    run_started_at,
  )
  .await?;
  record_thread_id(bot, session_id).await?;
  Ok(readback)
}

/// The recording of the thread id on the session's origin once the thread exists. The first reply creates the thread,
/// and the origin carries no thread id until then. A session that already holds one is left as it is.
async fn record_thread_id(bot: &Bot, session_id: &str) -> Result<(), thread_entry::ThreadReplyError> {
  let session = thread_entry::require_thread_session(&FEISHU, &bot.features, session_id).await?;
  let origin = origin_of(&session.origin)?;
  if origin.thread_id.is_some() {
    return Ok(());
  }
  let root = bot.client()?.message(&origin.root_id).await.map_err(api_failure)?;
  let thread_id = root
    .get(THREAD_ID)
    .and_then(Value::as_str)
    .with_context(|| format!("the root message {} has no thread after the reply", origin.root_id))?;
  let origin = FeishuOrigin { thread_id: Some(thread_id.to_string()), ..origin };
  let value = serde_json::to_value(&origin).map_err(thread_entry::ThreadReplyError::failure)?;
  bot
    .features
    .store
    .update_slot_fields_at(
      session_id,
      FEISHU.name,
      Map::from_iter([(FEISHU_ORIGIN.to_string(), value)]),
      UtcDateTime::now(),
    )
    .await?
    .ok_or_else(|| {
      thread_entry::ThreadReplyError::failure(format!("session {session_id} vanished during the reply"))
    })?;
  Ok(())
}

/// A failed Feishu call as a refusal: 502 with Feishu's message, or the unhandled failure for any other error.
pub fn api_failure(error: anyhow::Error) -> thread_entry::ThreadReplyError {
  match error.downcast::<FeishuAPIError>() {
    Ok(api) => thread_entry::ThreadReplyError::refused(axum::http::StatusCode::BAD_GATEWAY, api.to_string()),
    Err(other) => thread_entry::ThreadReplyError::failure(other),
  }
}

/// The round-end audit for one `master_done` (`deliver_done`).
pub async fn deliver_done(features: &Arc<FeatureContext>, session_id: &str, done: &Value) -> anyhow::Result<bool> {
  let bot = Bot::new(Arc::clone(features), BASE_URL);
  Ok(thread_entry::deliver_done(&adapter(&bot), features, session_id, done).await?)
}

/// The report of queued Feishu summons lost across a restart (`backfill_lost_summons`).
pub async fn backfill_lost_summons(features: &Arc<FeatureContext>) -> anyhow::Result<usize> {
  let bot = Bot::new(Arc::clone(features), BASE_URL);
  Ok(thread_entry::backfill_lost_summons(&adapter(&bot), features).await?)
}

/// The shared thread follow backfill (`backfill_followed_threads`), re-run over the adapter `client` serves.
pub async fn backfill_followed_threads(features: &Arc<FeatureContext>, client: FeishuClient) -> anyhow::Result<usize> {
  let adapter: Arc<dyn ThreadAdapter> =
    Arc::new(FeishuThreadAdapter::with_client(Bot::new(Arc::clone(features), BASE_URL), client));
  Ok(thread_entry::backfill_followed_threads(&adapter, features).await?)
}

/// The summon's arguments, built from a parsed message and dispatched to the shared core (`accept_summon`).
pub async fn accept_summon(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  message: &Message,
) -> anyhow::Result<Option<String>> {
  let session_id = summon_session_id(&message.tenant_key, &message.chat_id, message.thread_root()?);
  let origin = FeishuOrigin {
    tenant_key: message.tenant_key.clone(),
    chat_id: message.chat_id.clone(),
    root_id: message.thread_root()?.to_string(),
    thread_id: message.thread_id.clone(),
  };
  let origin = serde_json::to_value(&origin)?;
  let block = json!({
    (CHAT_ID): message.chat_id,
    (ROOT_ID): message.thread_root()?,
    (MENTION_ID): composite_id(&message.create_time, &message.message_id),
  });
  let label = format!("{SESSION_LABEL}{}", message.chat_id);
  let accepted = thread_entry::accept_summon(
    adapter,
    features,
    Summon {
      session_id: &session_id,
      label: &label,
      origin: &origin,
      block: &block,
      content: FEISHU.summon_line,
      user: Some(message.sender_open_id.as_str()),
    },
  )
  .await?;
  Ok(Some(accepted))
}

/// The routing of one parsed event to the shared core: a summon opens or reattaches the session, a follow arms the
/// thread's follow trigger. Returns the session the event reached, or None when no session took it.
pub async fn route_inbound(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  inbound: Inbound,
) -> anyhow::Result<Option<String>> {
  match inbound {
    Inbound::Drop(_) => Ok(None),
    Inbound::Summon(message) => accept_summon(adapter, features, &message).await,
    Inbound::Follow(message) => {
      let root = message.thread_root()?.to_string();
      let session_id = summon_session_id(&message.tenant_key, &message.chat_id, &root);
      let composite = composite_id(&message.create_time, &message.message_id);
      let origin_matches = |origin: &Value| {
        origin.get(CHAT_ID).and_then(Value::as_str) == Some(message.chat_id.as_str())
          && origin.get(ROOT_ID).and_then(Value::as_str) == Some(root.as_str())
      };
      Ok(thread_entry::follow_message(adapter, features, &session_id, &composite, origin_matches).await?)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::{Answer, Home, serve};
  use charliebot_compat::http_client;

  #[test]
  fn a_composite_id_splits_into_its_message_id() {
    let composite = composite_id("1700000000000", "om_abc-1");
    assert_eq!(composite, "1700000000000-om_abc-1");
    assert_eq!(message_id_of(&composite).unwrap(), "om_abc-1");
    assert!(message_id_of("om_100").is_err());
    assert!(message_id_of("abc-om_100").is_err());
  }

  #[test]
  fn composite_ids_order_by_milliseconds_then_message_id() {
    assert!(
      thread_entry::IdOrdering::TimeThenId.key("1700000000000-om_b")
        < thread_entry::IdOrdering::TimeThenId.key("1700000000001-om_a")
    );
    assert!(
      thread_entry::IdOrdering::TimeThenId.key("1700000000000-om_a")
        < thread_entry::IdOrdering::TimeThenId.key("1700000000000-om_b")
    );
  }

  #[test]
  fn only_a_reaction_the_app_added_counts_as_its_ack() {
    let own = json!({"reaction_id": "r1", "operator": {"operator_type": "app", "operator_id": "cli_me"}});
    let user = json!({"reaction_id": "r2", "operator": {"operator_type": "user", "operator_id": {"open_id": "ou_1"}}});
    let other_app = json!({"reaction_id": "r3", "operator": {"operator_type": "app", "operator_id": "cli_other"}});
    assert!(is_own_app_reaction(&own, "cli_me"));
    assert!(!is_own_app_reaction(&user, "cli_me"));
    assert!(!is_own_app_reaction(&other_app, "cli_me"));
  }

  #[test]
  fn the_session_id_derives_from_tenant_chat_and_root() {
    let first = summon_session_id("tk_1", "oc_1", "om_root");
    assert_eq!(first, summon_session_id("tk_1", "oc_1", "om_root"));
    assert_ne!(first, summon_session_id("tk_1", "oc_1", "om_other"));
    assert_ne!(first, summon_session_id("tk_2", "oc_1", "om_root"));
    assert_ne!(first, summon_session_id("tk_1", "oc_2", "om_root"));
  }

  #[tokio::test]
  async fn a_reply_into_a_chat_with_no_listed_person_is_refused_before_the_post() {
    let members = json!({
      "code": 0,
      "data": {"items": [{"member_id": "ou_synthetic_member", "member_id_type": "open_id"}], "has_more": false},
    });
    let stub = serve(move |_| Answer::json(members.clone())).await;
    let home = Home::new();
    let http = http_client::client(&http_client::Settings::from_env()).expect("the HTTP client builds");
    let client = FeishuClient::new(http, stub.base_url.clone(), "cli_adapter_post", "synthetic-secret");
    let bot = Bot::new(Arc::clone(&home.features), stub.base_url.clone());
    let adapter = FeishuThreadAdapter::with_client(bot, client);
    let address = json!({"chat_id": "oc_synthetic_post", "root_id": "om_root"});
    let error = adapter.post(&address, "hello", "post-key-1").await.expect_err("the reply is refused");
    assert!(error.to_string().contains("oc_synthetic_post"), "the error names the chat: {error}");
    let requests = stub.requests.lock().unwrap();
    assert!(requests.iter().all(|request| !request.target.contains("/reply")), "no reply was posted");
  }

  #[test]
  fn any_human_message_in_a_thread_is_eligible() {
    let item = json!({
      "message_id": "om_human",
      "create_time": "1700000000000",
      "sender": {"id": "ou_synthetic_stranger", "sender_type": "user"},
      "msg_type": "text",
      "body": {"content": "{\"text\":\"hi\"}"},
    });
    let message = thread_message(&item).unwrap();
    assert!(message.eligible);
    assert_eq!(message.user.as_deref(), Some("ou_synthetic_stranger"));
  }
}

//! The Slack colleague gate. `colleague_admitted` decides whether one colleague message may become agent input. The gate
//! applies that check to a follow-timer wake, and refuses agent-work requests that a colleague-started run makes in a
//! thread a colleague posted in.

use std::sync::Arc;

use anyhow::{Context, Result};
use charliebot_chat_threads::thread_entry::ThreadEventData;
use charliebot_core::event_types;
use charliebot_registry::FeatureContext;
use charliebot_registry::caller::CallerIdentity;
use charliebot_registry::colleague_gate::ColleagueGate;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::config::{NAME, SlackConfig};
use crate::slack_client::SlackClient;
use crate::thread_contract::SLACK;

/// The refusal detail of an agent-work request that a colleague-started run makes in a colleague's thread.
pub(crate) const COLLEAGUE_REFUSAL: &str =
  "refused: a Slack colleague has posted in this thread; the operator starts this work from a web message";

/// The key of the summon block that holds the Slack id of the message's sender.
pub(crate) const SENDER_USER_ID: &str = "sender_user_id";

/// Whether `message` is a human message from one of `ids`.
pub(crate) fn sent_by(message: &Value, ids: &[String]) -> bool {
  message.get("subtype").is_none_or(Value::is_null)
    && message.get("bot_id").is_none_or(Value::is_null)
    && message.get("user").and_then(Value::as_str).is_some_and(|user| ids.iter().any(|id| id == user))
}

/// The admission check: whether a colleague message in `channel`'s thread `thread_ts` may become agent input. The sender
/// must be listed; an operator must be a member of the channel, read page by page; and the thread must hold an
/// operator message or a message that mentions an operator. A failed Slack read logs an error and drops the message.
pub(crate) async fn colleague_admitted(
  client: &SlackClient,
  config: &SlackConfig,
  channel: &str,
  thread_ts: &str,
  sender: &str,
) -> bool {
  if !config.colleague_user_ids.iter().any(|id| id == sender) {
    return false;
  }
  match admission_reads(client, config, channel, thread_ts).await {
    Ok(true) => true,
    Ok(false) => {
      tracing::info!(event = "slack_colleague_message_not_admitted", channel, thread_ts, sender);
      false
    }
    Err(error) => {
      tracing::error!(
        event = "slack_colleague_admission_failed",
        channel,
        thread_ts,
        sender,
        error = %format!("{error:#}"),
      );
      false
    }
  }
}

async fn admission_reads(client: &SlackClient, config: &SlackConfig, channel: &str, thread_ts: &str) -> Result<bool> {
  if !client.channel_has_member(channel, &config.allowed_user_ids).await? {
    return Ok(false);
  }
  let messages = client.get_thread_replies(channel, thread_ts).await?;
  Ok(messages.iter().any(|message| speaks_to_operator(message, &config.allowed_user_ids)))
}

/// Whether `message` is from an operator or mentions one by `<@id>`.
fn speaks_to_operator(message: &Value, operators: &[String]) -> bool {
  let from_operator =
    message.get("user").and_then(Value::as_str).is_some_and(|user| operators.iter().any(|id| id == user));
  let text = message.get("text").and_then(Value::as_str).unwrap_or_default();
  from_operator || operators.iter().any(|id| text.contains(&format!("<@{id}>")))
}

/// Whether a run input is the operator's: a web user message, or a Slack summon sent by an operator.
fn operator_input(event: &Value, operators: &[String]) -> bool {
  match event.get("type").and_then(Value::as_str) {
    Some(event_types::USER) => true,
    Some(event_types::AGENT_MESSAGE) => event
      .get(SLACK.name)
      .and_then(|block| block.get(SENDER_USER_ID))
      .and_then(Value::as_str)
      .is_some_and(|sender| operators.iter().any(|id| id == sender)),
    _ => false,
  }
}

/// The channel and thread timestamp of a Slack thread origin.
fn thread_of(origin: &Value) -> Result<(&str, &str)> {
  let channel = origin.get("channel_id").and_then(Value::as_str).context("the Slack origin has no channel_id")?;
  let thread_ts = origin.get("thread_ts").and_then(Value::as_str).context("the Slack origin has no thread_ts")?;
  Ok((channel, thread_ts))
}

/// Whether the request of run `run_id` must be refused: its inputs are not all the operator's, its session is a Slack
/// thread, and a colleague has posted in that thread.
pub(crate) async fn colleague_thread_refuses(
  features: &FeatureContext,
  client: Option<&SlackClient>,
  session_id: &str,
  run_id: &str,
) -> Result<bool> {
  let config = features.config.get().context("load config")?;
  let slack = config.section::<SlackConfig>(NAME);
  if slack.colleague_user_ids.is_empty() {
    return Ok(false);
  }
  let run = features.store.read_run_sync(session_id, run_id)?.context("the run record is missing")?;
  let events: Vec<Value> =
    features.store.load_all_facts_view_sync(session_id)?.iter().map(|event| event.event_value()).collect();
  let operator_only = run.input_event_ids.iter().all(|input| {
    events
      .iter()
      .find(|event| event.get("id").and_then(Value::as_str) == Some(input.as_str()))
      .is_some_and(|event| operator_input(event, &slack.allowed_user_ids))
  });
  if operator_only {
    return Ok(false);
  }
  let Some(origin) = features
    .store
    .get_session(session_id)
    .await?
    .and_then(|meta| meta.extra(SLACK.origin_field).filter(|origin| !origin.is_null()).cloned())
  else {
    return Ok(false);
  };
  let (channel, thread_ts) = thread_of(&origin)?;
  let owned_client = match client {
    Some(_) => None,
    None => Some(SlackClient::from_features(features).context("load Slack client")?),
  };
  let client = client.or(owned_client.as_ref()).expect("the Slack client is available");
  let messages = client.get_thread_replies(channel, thread_ts).await?;
  Ok(messages.iter().any(|message| sent_by(message, &slack.colleague_user_ids)))
}

/// Whether a follow wake of `session_id` may start a run: the first unread colleague message above the watermark must
/// pass the admission check. A wake without an unread colleague message passes.
async fn follow_wake_admitted(features: &FeatureContext, client: &SlackClient, session_id: &str) -> Result<bool> {
  let Some(meta) = features.store.get_session(session_id).await? else {
    return Ok(true);
  };
  let Some(origin) = meta.extra(SLACK.origin_field).filter(|origin| !origin.is_null()) else {
    return Ok(true);
  };
  let (channel, thread_ts) = thread_of(origin)?;
  let config = features.config.get().context("load config")?;
  let slack = config.section::<SlackConfig>(NAME);
  let watermark = meta.extra(SLACK.watermark_field).and_then(Value::as_str);
  let messages = client.get_thread_replies(channel, thread_ts).await?;
  let unread_colleague = messages.iter().find(|message| {
    sent_by(message, &slack.colleague_user_ids)
      && watermark.is_none_or(|watermark| {
        SLACK.id_key.key(message.get("ts").and_then(Value::as_str).unwrap_or_default()) > SLACK.id_key.key(watermark)
      })
  });
  let Some(colleague) = unread_colleague else {
    return Ok(true);
  };
  let sender = colleague.get("user").and_then(Value::as_str).context("the colleague message has no user")?;
  Ok(colleague_admitted(client, slack, channel, thread_ts, sender).await)
}

/// The refusal detail of an agent-work request, or None when the request may proceed. Operator callers always proceed;
/// a failed read refuses and logs an error.
pub(crate) async fn refuse_work_with(
  features: &FeatureContext,
  client: Option<&SlackClient>,
  caller: &CallerIdentity,
) -> Option<&'static str> {
  let CallerIdentity::Agent { claims } = caller else {
    return None;
  };
  match colleague_thread_refuses(features, client, &claims.session_id, &claims.run_id).await {
    Ok(true) => Some(COLLEAGUE_REFUSAL),
    Ok(false) => None,
    Err(error) => {
      tracing::error!(
        event = "slack_colleague_work_check_failed",
        session = %claims.session_id,
        run = %claims.run_id,
        error = %format!("{error:#}"),
      );
      Some(COLLEAGUE_REFUSAL)
    }
  }
}

/// Whether run `run_id` of `session_id` is colleague-started, read by the predicate that refuses its requests. A failed
/// read answers true, as a refused request does, so a turn-end check and a request decide one run the same way.
pub(crate) async fn run_read_only_with(
  features: &FeatureContext,
  client: Option<&SlackClient>,
  session_id: &str,
  run_id: &str,
) -> bool {
  match colleague_thread_refuses(features, client, session_id, run_id).await {
    Ok(read_only) => read_only,
    Err(error) => {
      tracing::error!(
        event = "slack_colleague_run_check_failed",
        session = session_id,
        run = run_id,
        error = %format!("{error:#}"),
      );
      true
    }
  }
}

/// Whether a follow wake of `session_id` may start a run. A failed read drops the wake and logs an error.
pub(crate) async fn admit_follow_wake_with(features: &FeatureContext, client: &SlackClient, session_id: &str) -> bool {
  match follow_wake_admitted(features, client, session_id).await {
    Ok(admitted) => admitted,
    Err(error) => {
      tracing::error!(
        event = "slack_colleague_wake_check_failed",
        session = session_id,
        error = %format!("{error:#}"),
      );
      false
    }
  }
}

/// The Slack colleague gate, registered in startup state by `gate_step`. Each check reads Slack with a client built from
/// the current credentials.
pub(crate) struct SlackColleagueGate;

impl ColleagueGate for SlackColleagueGate {
  fn refuse_agent_work<'a>(
    &'a self,
    features: &'a FeatureContext,
    caller: &'a CallerIdentity,
  ) -> BoxFuture<'a, Option<&'static str>> {
    Box::pin(async move { refuse_work_with(features, None, caller).await })
  }

  fn run_read_only<'a>(
    &'a self,
    features: &'a FeatureContext,
    session_id: &'a str,
    run_id: &'a str,
  ) -> BoxFuture<'a, bool> {
    Box::pin(async move { run_read_only_with(features, None, session_id, run_id).await })
  }

  fn admits_wake<'a>(
    &'a self,
    features: &'a FeatureContext,
    session_id: &'a str,
    label: &'a str,
  ) -> BoxFuture<'a, bool> {
    Box::pin(async move {
      if !label.starts_with(SLACK.follow_trigger_prefix) {
        return true;
      }
      match SlackClient::from_features(features) {
        Ok(client) => admit_follow_wake_with(features, &client, session_id).await,
        Err(error) => {
          tracing::error!(event = "slack_colleague_client_failed", error = %format!("{error:#}"));
          false
        }
      }
    })
  }
}

/// The startup step that registers the Slack colleague gate.
pub(crate) fn gate_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let gate: Arc<dyn ColleagueGate> = Arc::new(SlackColleagueGate);
    features.startup.insert(gate)?;
    Ok(())
  })
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use charliebot_core::run_token::RunTokenClaims;
  use charliebot_registry::colleague_gate::ColleagueGate;
  use serde_json::{Value, json};

  use super::*;
  use crate::test_support::{FakeApi, FixedClientGate, Home};

  const CHANNEL: &str = "C_TEST";
  const THREAD: &str = "1700000000.000100";

  fn config() -> SlackConfig {
    SlackConfig { allowed_user_ids: vec!["U_ALLOWED".to_string()], colleague_user_ids: vec!["U_COLLEAGUE".to_string()] }
  }

  fn message(ts: &str, user: &str, text: &str) -> Value {
    json!({"ts": ts, "user": user, "text": text})
  }

  async fn admitted(api: &FakeApi, sender: &str, thread_ts: &str) -> bool {
    colleague_admitted(&api.client, &config(), CHANNEL, thread_ts, sender).await
  }

  #[tokio::test]
  async fn colleague_message_is_admitted_when_the_operator_is_a_member_and_has_spoken() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_OTHER", "U_ALLOWED"]]);
    api.set_thread_messages(vec![
      message(THREAD, "U_ALLOWED", "status?"),
      message("1700000000.000200", "U_COLLEAGUE", "why?"),
    ]);
    assert!(admitted(&api, "U_COLLEAGUE", THREAD).await);
  }

  #[tokio::test]
  async fn a_top_level_colleague_message_that_mentions_the_operator_admits_itself() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_ALLOWED"]]);
    api.set_thread_messages(vec![message(THREAD, "U_COLLEAGUE", "<@U_ALLOWED> help")]);
    assert!(admitted(&api, "U_COLLEAGUE", THREAD).await);
  }

  #[tokio::test]
  async fn a_top_level_colleague_message_without_a_mention_is_dropped() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_ALLOWED"]]);
    api.set_thread_messages(vec![message(THREAD, "U_COLLEAGUE", "help")]);
    assert!(!admitted(&api, "U_COLLEAGUE", THREAD).await);
  }

  #[tokio::test]
  async fn the_operator_on_the_second_member_page_is_admitted() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_OTHER_ONE"], &["U_OTHER_TWO", "U_ALLOWED"]]);
    api.set_thread_messages(vec![message(THREAD, "U_ALLOWED", "status?")]);
    assert!(admitted(&api, "U_COLLEAGUE", THREAD).await);
  }

  #[tokio::test]
  async fn a_channel_without_the_operator_drops_the_message_even_with_the_operator_in_the_thread() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_OTHER_ONE"], &["U_OTHER_TWO"]]);
    api.set_thread_messages(vec![message(THREAD, "U_ALLOWED", "status?")]);
    assert!(!admitted(&api, "U_COLLEAGUE", THREAD).await);
  }

  #[tokio::test]
  async fn an_unreadable_member_list_drops_the_message() {
    let api = FakeApi::start(&[]).await;
    api.set_members_unreadable();
    api.set_thread_messages(vec![message(THREAD, "U_ALLOWED", "status?")]);
    assert!(!admitted(&api, "U_COLLEAGUE", THREAD).await);
  }

  #[tokio::test]
  async fn an_unreadable_multi_person_dm_thread_drops_the_message() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_ALLOWED"]]);
    api.set_replies_unreadable();

    assert!(!colleague_admitted(&api.client, &config(), "G_TEST", THREAD, "U_COLLEAGUE").await);
  }

  #[tokio::test]
  async fn an_unlisted_sender_is_dropped_without_a_slack_read() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_ALLOWED"]]);
    api.set_thread_messages(vec![message(THREAD, "U_ALLOWED", "status?")]);
    assert!(!admitted(&api, "U_STRANGER", THREAD).await);
    assert!(api.calls().is_empty(), "no Slack read happens for an unlisted sender");
  }

  async fn thread_home(api: &FakeApi, messages: Vec<Value>) -> (Home, String) {
    let home = Home::new();
    let session_id = crate::slack_listener::summon_session_id("T_TEST", CHANNEL, THREAD);
    home.thread_session(&session_id, THREAD).await;
    api.set_thread_messages(messages);
    home.features.startup.insert(Arc::new(FixedClientGate(Arc::clone(&api.client))) as Arc<dyn ColleagueGate>).unwrap();
    (home, session_id)
  }

  #[tokio::test]
  async fn a_follow_wake_drops_an_unread_colleague_message_that_is_not_admitted() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_OTHER"]]);
    let (home, session_id) = thread_home(&api, vec![message(THREAD, "U_COLLEAGUE", "help")]).await;
    let label = format!("slack-thread-follow floor={THREAD}");
    assert!(
      !home
        .features
        .startup
        .get::<Arc<dyn ColleagueGate>>()
        .unwrap()
        .admits_wake(&home.features, &session_id, &label)
        .await
    );
  }

  #[tokio::test]
  async fn a_follow_wake_passes_when_the_unread_colleague_message_is_admitted() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_ALLOWED"]]);
    let (home, session_id) = thread_home(
      &api,
      vec![message(THREAD, "U_ALLOWED", "status?"), message("1700000000.000200", "U_COLLEAGUE", "why?")],
    )
    .await;
    let label = format!("slack-thread-follow floor={THREAD}");
    assert!(
      home
        .features
        .startup
        .get::<Arc<dyn ColleagueGate>>()
        .unwrap()
        .admits_wake(&home.features, &session_id, &label)
        .await
    );
  }

  #[tokio::test]
  async fn a_follow_wake_rechecks_membership_after_the_operator_leaves() {
    let api = FakeApi::start(&[]).await;
    api.set_member_pages(&[&["U_ALLOWED"]]);
    let (home, session_id) = thread_home(
      &api,
      vec![message(THREAD, "U_ALLOWED", "status?"), message("1700000000.000200", "U_COLLEAGUE", "why?")],
    )
    .await;
    let label = format!("slack-thread-follow floor={THREAD}");
    let gate = home.features.startup.get::<Arc<dyn ColleagueGate>>().unwrap();
    assert!(gate.admits_wake(&home.features, &session_id, &label).await);

    api.set_member_pages(&[&["U_OTHER"]]);
    assert!(!gate.admits_wake(&home.features, &session_id, &label).await);
  }

  /// Records a run of `session_id` whose inputs are the events `input_ids`, and returns the caller that run's token names.
  fn run_caller(home: &Home, session_id: &str, input_ids: &[&str]) -> CallerIdentity {
    let run_id = "run-synthetic";
    let run_path = home.features.store.run_metadata_path(session_id, run_id);
    std::fs::create_dir_all(run_path.parent().expect("a run directory has a parent")).expect("the run directory");
    let record = json!({"id": run_id, "session_id": session_id, "kind": "manager_turn", "input_event_ids": input_ids});
    std::fs::write(&run_path, record.to_string()).expect("the run record");
    let claims =
      RunTokenClaims { session_id: session_id.to_string(), run_id: run_id.to_string(), agent: "synthetic".to_string() };
    CallerIdentity::Agent { claims }
  }

  async fn append(home: &Home, session_id: &str, event: Value) {
    home.features.store.append_chat_event(session_id, event).await.expect("the event appends");
  }

  fn operator_web_message() -> Value {
    json!({"id": "evt-operator-web", "type": "user", "text": "status?"})
  }

  fn summon(id: &str, sender: &str) -> Value {
    json!({"id": id, "type": "agent_message", "slack": {"sender_user_id": sender}})
  }

  #[tokio::test]
  async fn a_run_fed_by_a_colleague_summon_in_a_colleague_thread_is_refused() {
    let api = FakeApi::start(&[]).await;
    let (home, session_id) = thread_home(
      &api,
      vec![message(THREAD, "U_ALLOWED", "status?"), message("1700000000.000200", "U_COLLEAGUE", "help")],
    )
    .await;
    append(&home, &session_id, summon("evt-colleague-summon", "U_COLLEAGUE")).await;
    let caller = run_caller(&home, &session_id, &["evt-colleague-summon"]);
    assert_eq!(refuse_work_with(&home.features, Some(&api.client), &caller).await, Some(COLLEAGUE_REFUSAL));
  }

  #[tokio::test]
  async fn a_run_fed_by_an_agent_trigger_in_a_colleague_thread_is_refused() {
    let api = FakeApi::start(&[]).await;
    let (home, session_id) = thread_home(
      &api,
      vec![message(THREAD, "U_ALLOWED", "status?"), message("1700000000.000200", "U_COLLEAGUE", "help")],
    )
    .await;
    append(
      &home,
      &session_id,
      json!({"id": "evt-agent-trigger", "type": event_types::SCHEDULED_TRIGGER, "message": "synthetic timer"}),
    )
    .await;
    let caller = run_caller(&home, &session_id, &["evt-agent-trigger"]);

    assert_eq!(refuse_work_with(&home.features, Some(&api.client), &caller).await, Some(COLLEAGUE_REFUSAL));
  }

  #[tokio::test]
  async fn a_run_fed_only_by_the_operator_passes_in_a_colleague_thread() {
    let api = FakeApi::start(&[]).await;
    let (home, session_id) = thread_home(
      &api,
      vec![message(THREAD, "U_ALLOWED", "status?"), message("1700000000.000200", "U_COLLEAGUE", "help")],
    )
    .await;
    append(&home, &session_id, operator_web_message()).await;
    append(&home, &session_id, summon("evt-operator-summon", "U_ALLOWED")).await;
    let caller = run_caller(&home, &session_id, &["evt-operator-web", "evt-operator-summon"]);
    assert_eq!(refuse_work_with(&home.features, Some(&api.client), &caller).await, None);
  }

  #[tokio::test]
  async fn a_run_mixing_an_operator_input_with_a_colleague_summon_is_refused() {
    let api = FakeApi::start(&[]).await;
    let (home, session_id) = thread_home(
      &api,
      vec![message(THREAD, "U_ALLOWED", "status?"), message("1700000000.000200", "U_COLLEAGUE", "help")],
    )
    .await;
    append(&home, &session_id, operator_web_message()).await;
    append(&home, &session_id, summon("evt-colleague-summon", "U_COLLEAGUE")).await;
    let caller = run_caller(&home, &session_id, &["evt-operator-web", "evt-colleague-summon"]);
    assert_eq!(refuse_work_with(&home.features, Some(&api.client), &caller).await, Some(COLLEAGUE_REFUSAL));
  }

  #[tokio::test]
  async fn a_run_with_a_non_operator_input_passes_when_the_thread_holds_no_colleague_message() {
    let api = FakeApi::start(&[]).await;
    let (home, session_id) = thread_home(&api, vec![message(THREAD, "U_ALLOWED", "status?")]).await;
    append(&home, &session_id, summon("evt-stranger-summon", "U_STRANGER")).await;
    let caller = run_caller(&home, &session_id, &["evt-stranger-summon"]);
    assert_eq!(refuse_work_with(&home.features, Some(&api.client), &caller).await, None);
  }

  #[tokio::test]
  async fn a_failed_thread_read_refuses_a_run_that_is_not_fed_only_by_the_operator() {
    let api = FakeApi::start(&[]).await;
    let (home, session_id) = thread_home(&api, vec![message(THREAD, "U_ALLOWED", "status?")]).await;
    api.set_replies_unreadable();
    append(&home, &session_id, summon("evt-colleague-summon", "U_COLLEAGUE")).await;
    let caller = run_caller(&home, &session_id, &["evt-colleague-summon"]);
    assert_eq!(refuse_work_with(&home.features, Some(&api.client), &caller).await, Some(COLLEAGUE_REFUSAL));
  }

  #[tokio::test]
  async fn an_operator_caller_is_never_refused() {
    let home = Home::new();
    let caller = CallerIdentity::Operator { session_id: None };
    assert_eq!(refuse_work_with(&home.features, None, &caller).await, None);
  }

  #[tokio::test]
  async fn an_unconfigured_slack_feature_does_not_refuse_work_in_a_plain_session() {
    let home = Home::new();
    home.clear_slack_credentials();
    home.plain_session("plain-session").await;
    let caller = run_caller(&home, "plain-session", &["evt-nonoperator"]);

    let refusal = SlackColleagueGate.refuse_agent_work(&home.features, &caller).await;

    assert_eq!(refusal, None);
  }

  /// The run-level predicate that turn-end checks read and the refusal of a request both answer through
  /// `colleague_thread_refuses`: one answer per run shape, a colleague-fed run, a mixed run and an operator-only run.
  #[tokio::test]
  async fn the_run_predicate_and_the_refusal_answer_alike_for_each_run_shape() {
    let api = FakeApi::start(&[]).await;
    let (home, session_id) = thread_home(
      &api,
      vec![message(THREAD, "U_ALLOWED", "status?"), message("1700000000.000200", "U_COLLEAGUE", "help")],
    )
    .await;
    append(&home, &session_id, summon("evt-colleague-summon", "U_COLLEAGUE")).await;
    append(&home, &session_id, operator_web_message()).await;
    let shapes: [(&[&str], bool); 3] = [
      (&["evt-colleague-summon"], true),
      (&["evt-operator-web", "evt-colleague-summon"], true),
      (&["evt-operator-web"], false),
    ];
    for (input_ids, read_only) in shapes {
      let caller = run_caller(&home, &session_id, input_ids);
      let CallerIdentity::Agent { claims } = &caller else { unreachable!("a run caller is an agent") };
      let predicate = colleague_thread_refuses(&home.features, Some(&api.client), &session_id, &claims.run_id)
        .await
        .expect("the predicate answers");
      let refused = refuse_work_with(&home.features, Some(&api.client), &caller).await.is_some();
      assert_eq!(predicate, read_only, "{input_ids:?}");
      assert_eq!(refused, read_only, "{input_ids:?}");
    }
  }
}

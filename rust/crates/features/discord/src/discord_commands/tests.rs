//! The cases of the Discord server-side commands, run through the thread core and the internal routes against the
//! loopback Discord fake in `test_support`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use charliebot_core::event_types;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::discord_client::REQUIRED_PERMISSIONS;
use crate::discord_listener::summon_session_id;
use crate::test_support::{ALLOWED_PERSON, ALLOWED_USER, Home, Msg, TEST_TOKEN, snowflake};

const GUILD: &str = "800000000000000001";
const PARENT: &str = "800000000000000002";
const THREAD: &str = "800000000000000003";
const OTHER_CHANNEL: &str = "800000000000000004";
const OTHER_GUILD: &str = "800000000000000005";
const BOT: &str = "600000000000000001";
const UNLISTED: &str = "700000000000000003";

fn bot(home: &Home) -> Bot {
  Bot::new(Arc::clone(&home.features), home.api.clone())
}

fn origin() -> Value {
  json!({"guild_id": GUILD, "parent_channel_id": PARENT, "thread_id": THREAD})
}

/// A Discord-backed session like a summon leaves it, with an optional read watermark.
async fn make_session(home: &Home, watermark: Option<String>) -> String {
  home.thread_session(&summon_session_id(GUILD, THREAD), origin(), watermark.as_deref()).await
}

/// Set the thread's messages, oldest first.
fn thread_messages(home: &Home, messages: &[Msg]) {
  home.fake().channels.insert(THREAD.to_string(), messages.iter().map(Msg::value).collect());
}

fn messages(read: &Value) -> &[Value] {
  read["messages"].as_array().unwrap()
}

fn ids(read: &Value) -> Vec<String> {
  messages(read).iter().map(|message| message["id"].as_str().unwrap().to_string()).collect()
}

/// One field of each message in a readback, in order.
fn fields(read: &Value, key: &str) -> Vec<Value> {
  messages(read).iter().map(|message| message[key].clone()).collect()
}

/// The discord_ack events the session logged, in order.
fn acks(home: &Home, session_id: &str) -> Vec<Value> {
  home.events(session_id).into_iter().filter(|event| event["type"] == "discord_ack").collect()
}

/// A summon injection as the log holds it: the discord block and the one-line content.
fn summon_event(mention_id: &str) -> Value {
  json!({
    "type": event_types::AGENT_MESSAGE,
    "content": DISCORD.summon_line,
    "discord": {"guild_id": GUILD, "channel_id": THREAD, "thread_id": THREAD, "mention_id": mention_id},
  })
}

/// Post `body` to `path` on the router; returns the status and the JSON answer.
async fn send(router: Router, path: &str, body: Value) -> (StatusCode, Value) {
  let request = Request::builder()
    .method(Method::POST)
    .uri(path)
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(body.to_string()))
    .expect("the request builds");
  let response = router.oneshot(request).await.expect("the router answers");
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
  (status, serde_json::from_slice(&bytes).expect("the answer is JSON"))
}

/// The window starts at the oldest unread; the ack covers exactly its unread ids.
#[tokio::test]
async fn read_marks_only_returned_unread_and_reports_more_unread() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(
    &home,
    &[
      Msg::new(1, "hello"),
      Msg::new(2, "first follow up").attachment("https://cdn.discordapp.com/attachments/shot.png"),
      Msg::new(3, "bot noise").by(BOT).bot(),
      Msg::new(4, "third follow up"),
      Msg::new(5, "fourth follow up"),
    ],
  );
  let session_id = make_session(&home, Some(snowflake(1))).await;

  let (status, result) =
    send(home.router(), "/api/internal/discord/read", json!({"session_id": session_id, "limit": 3})).await;

  assert_eq!(status, StatusCode::OK);
  assert_eq!(ids(&result), [2, 3, 4].map(snowflake));
  assert_eq!(fields(&result, "unread"), [true, false, true].map(Value::Bool));
  assert_eq!(messages(&result)[0]["attachments"], json!(["https://cdn.discordapp.com/attachments/shot.png"]));
  assert_eq!(result["watermark_id"], snowflake(4));
  assert_eq!(result["more_unread"], 1);
  let meta = home.meta(&session_id).await;
  assert_eq!(meta.extra("discord_watermark_id"), Some(&json!(snowflake(4))));
  let acked = acks(&home, &session_id);
  assert_eq!(acked.len(), 1);
  assert_eq!(
    acked[0]["discord_ack"],
    json!({"message_ids": [snowflake(2), snowflake(4)], "watermark_id": snowflake(4)})
  );
}

/// A read after an earlier window resumes after that window's watermark.
#[tokio::test]
async fn read_continues_after_the_previous_window() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(
    &home,
    &[Msg::new(8, "fourth follow up"), Msg::new(9, "fifth follow up"), Msg::new(10, "sixth follow up")],
  );
  let session_id = make_session(&home, Some(snowflake(7))).await;

  let result = read_thread(&bot(&home), &session_id, None, 4).await.unwrap();

  assert_eq!(ids(&result), [8, 9, 10].map(snowflake));
  assert_eq!(fields(&result, "unread"), [true, true, true].map(Value::Bool));
  assert_eq!(result["watermark_id"], snowflake(10));
  assert_eq!(result["more_unread"], 0);
}

/// Nothing unread: the window is the newest `limit` messages and nothing is marked.
#[tokio::test]
async fn read_without_unread_returns_the_newest_limit() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &(1..=8).map(|i| Msg::new(i, &format!("m{i}"))).collect::<Vec<_>>());
  let session_id = make_session(&home, Some(snowflake(8))).await;

  let result = read_thread(&bot(&home), &session_id, None, 3).await.unwrap();

  assert_eq!(ids(&result), [6, 7, 8].map(snowflake));
  assert_eq!(fields(&result, "unread"), [false, false, false].map(Value::Bool));
  assert_eq!(result["watermark_id"], snowflake(8));
  assert_eq!(result["more_unread"], 0);
  assert!(acks(&home, &session_id).is_empty());
}

/// A message arriving after the thread read stays out of the readback and breaks nothing.
#[tokio::test]
async fn read_succeeds_when_a_message_lands_after_the_full_read() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "first"), Msg::new(2, "second")]);
  let session_id = make_session(&home, Some(snowflake(2))).await;
  // The new message lands right after the first full read, so the readback already taken does not hold it.
  home.fake().landing = Some((THREAD.to_string(), Msg::new(3, "arrived in between").value()));

  let result = read_thread(&bot(&home), &session_id, None, 50).await.unwrap();

  assert_eq!(ids(&result), [1, 2].map(snowflake));
  assert_eq!(fields(&result, "unread"), [false, false].map(Value::Bool));
  assert_eq!(result["watermark_id"], snowflake(2));
  assert_eq!(result["more_unread"], 0);
  assert!(acks(&home, &session_id).is_empty());
}

/// The summon step marked the mention read, so the window would start at the later unread message and hide the
/// question. The readback starts at the pending summon's mention instead and flags it `summon: true`.
#[tokio::test]
async fn read_flags_the_pending_summon_and_starts_the_window_at_it() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "the question"), Msg::new(2, "a later follow up")]);
  // The summon step consumed its own mention.
  let session_id = make_session(&home, Some(snowflake(1))).await;
  home.append(&session_id, summon_event(&snowflake(1))).await;

  let result = read_thread(&bot(&home), &session_id, None, 2).await.unwrap();

  assert_eq!(ids(&result), [1, 2].map(snowflake));
  assert_eq!(fields(&result, "summon"), [true, false].map(Value::Bool));
  assert_eq!(fields(&result, "unread"), [false, true].map(Value::Bool));
  // The ack keeps its meaning: the unread message inside the window is the only one it advances through.
  assert_eq!(
    acks(&home, &session_id)[0]["discord_ack"],
    json!({"message_ids": [snowflake(2)], "watermark_id": snowflake(2)})
  );
  assert_eq!(result["watermark_id"], snowflake(2));
  assert_eq!(result["more_unread"], 0);
}

/// A summon whose round finished (a master_done names it) is answered history, not a pending ask: no flag, and the
/// window start stays with the unread.
#[tokio::test]
async fn read_does_not_flag_a_summon_a_master_done_names() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "the question"), Msg::new(2, "a later follow up")]);
  let session_id = make_session(&home, Some(snowflake(1))).await;
  let summon = home.append(&session_id, summon_event(&snowflake(1))).await;
  home
    .append(
      &session_id,
      json!({
        "type": event_types::MASTER_DONE,
        "exit_code": 0,
        "still_thinking": false,
        "input_event_id": summon["id"],
      }),
    )
    .await;

  let result = read_thread(&bot(&home), &session_id, None, 2).await.unwrap();

  // Not pending: the window keeps its old start (the oldest unread), so the answered mention stays out entirely.
  assert_eq!(
    result["messages"],
    json!([{
      "id": snowflake(2),
      "author_id": ALLOWED_USER,
      "author": "plain-name",
      "person": ALLOWED_PERSON,
      "timestamp": "2026-08-26T00:00:02Z",
      "content": "a later follow up",
      "attachments": [],
      "unread": true,
      "summon": false,
    }])
  );
}

/// A summon a restart-resumed turn received is answered history even though its `master_done` carries no input ids:
/// the run record's receipts retire it, so it is not pending.
#[tokio::test]
async fn read_does_not_flag_a_summon_a_resumed_turn_received() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "the question"), Msg::new(2, "a later follow up")]);
  let session_id = make_session(&home, Some(snowflake(1))).await;
  let summon = home.append(&session_id, summon_event(&snowflake(1))).await;
  let summon_id = summon["id"].as_str().expect("an appended event carries its id");
  let started = summon["timestamp"].as_str().expect("an appended event carries its timestamp");
  home.launch_run(&session_id, "resumed-run", &[summon_id], started);
  home
    .append(
      &session_id,
      json!({
        "type": event_types::MASTER_DONE,
        "run_id": "resumed-run",
        "exit_code": 0,
        "still_thinking": false,
        "input_event_ids": [],
      }),
    )
    .await;

  let result = read_thread(&bot(&home), &session_id, None, 2).await.unwrap();

  assert_eq!(ids(&result), [snowflake(2)]);
  assert_eq!(fields(&result, "summon"), [json!(false)]);
}

/// A summon the boot backfill marked lost never gets a master_done, so the marker alone must retire it. No flag, and
/// the window start stays with the unread instead of dragging back to the dead mention on every later read.
#[tokio::test]
async fn read_does_not_flag_a_summon_a_backfill_marker_names() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "the question"), Msg::new(2, "a later follow up")]);
  let session_id = make_session(&home, Some(snowflake(1))).await;
  let summon = home.append(&session_id, summon_event(&snowflake(1))).await;
  home
    .append(
      &session_id,
      json!({
        "type": event_types::ASSISTANT_ERROR,
        "content": "这条 Discord 召唤在服务重启时还排在队列里，没有任何轮次回答它；已在对应线程里说明。",
        (DISCORD.backfill_key()): {(event_types::INPUT_EVENT_ID): summon["id"]},
      }),
    )
    .await;

  let result = read_thread(&bot(&home), &session_id, None, 2).await.unwrap();

  // Not pending: the window starts at the oldest unread, so the lost mention stays out and nothing carries the flag.
  assert_eq!(ids(&result), [snowflake(2)]);
  assert_eq!(fields(&result, "summon"), [json!(false)]);
  assert_eq!(fields(&result, "unread"), [json!(true)]);
  assert_eq!(result["more_unread"], 0);
}

/// A mention that started the thread names the starter: the prepended starter view carries the summon flag.
#[tokio::test]
async fn read_flags_a_starter_message_mention() {
  let home = Home::new_without_thread_runtime(true).await;
  home.fake().channels.insert(PARENT.to_string(), vec![Msg::new(0, "the question").id(THREAD).value()]);
  thread_messages(&home, &[Msg::new(1, "a follow up")]);
  // The summon step consumed the starter mention.
  let session_id = make_session(&home, Some(THREAD.to_string())).await;
  home.append(&session_id, summon_event(THREAD)).await;

  let result = read_thread(&bot(&home), &session_id, None, 5).await.unwrap();

  assert_eq!(ids(&result), [THREAD.to_string(), snowflake(1)]);
  assert_eq!(fields(&result, "summon"), [true, false].map(Value::Bool));
  assert_eq!(fields(&result, "unread"), [false, true].map(Value::Bool));
}

/// A nudge carries the summon block with `nudge_of`: it never flags a message.
#[tokio::test]
async fn read_flags_a_nudge_neither() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "hello")]);
  let session_id = make_session(&home, Some(snowflake(1))).await;
  let mut nudge = summon_event(&snowflake(1));
  nudge["discord"]["nudge_of"] = json!("s0");
  home.append(&session_id, nudge).await;

  let result = read_thread(&bot(&home), &session_id, None, 5).await.unwrap();

  assert_eq!(fields(&result, "summon"), [json!(false)]);
}

/// A thread started from a parent message reads its starter first, not counted in `limit`.
#[tokio::test]
async fn read_prepends_the_parent_starter_outside_the_limit() {
  let home = Home::new_without_thread_runtime(true).await;
  home.fake().channels.insert(
    PARENT.to_string(),
    vec![Msg::new(0, "please plan the release").id(THREAD).global_name("Display Name").value()],
  );
  thread_messages(&home, &[Msg::new(1, "first follow up"), Msg::new(2, "second follow up")]);
  let session_id = make_session(&home, None).await;

  let result = read_thread(&bot(&home), &session_id, None, 2).await.unwrap();

  assert_eq!(ids(&result), [THREAD.to_string(), snowflake(1), snowflake(2)]);
  // The starter rides first, unread false and not a summon; limit=2 still delivered both unread.
  assert_eq!(
    messages(&result)[0],
    json!({
      "id": THREAD,
      "author_id": ALLOWED_USER,
      "author": "Display Name",
      "person": ALLOWED_PERSON,
      "timestamp": "2026-08-26T00:00:00Z",
      "content": "please plan the release",
      "attachments": [],
      "unread": false,
      "summon": false,
    })
  );
  // A message view: global_name unset falls back to the username.
  assert_eq!(
    messages(&result)[1],
    json!({
      "id": snowflake(1),
      "author_id": ALLOWED_USER,
      "author": "plain-name",
      "person": ALLOWED_PERSON,
      "timestamp": "2026-08-26T00:00:01Z",
      "content": "first follow up",
      "attachments": [],
      "unread": true,
      "summon": false,
    })
  );
}

/// A forum post or a thread not started from a message has no starter: the read goes on without one.
#[tokio::test]
async fn read_skips_a_404_starter() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "first follow up"), Msg::new(2, "second follow up")]);
  let session_id = make_session(&home, None).await;

  let result = read_thread(&bot(&home), &session_id, None, 2).await.unwrap();

  assert_eq!(ids(&result), [snowflake(1), snowflake(2)]);
  assert_eq!(fields(&result, "unread"), [json!(true), json!(true)]);
}

/// `person` is the account's configured name; an author outside the map reads as None everywhere.
#[tokio::test]
async fn read_names_the_author_person_for_listed_and_unlisted_authors() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "mine"), Msg::new(2, "unlisted").by(UNLISTED)]);
  let session_id = make_session(&home, None).await;

  let own = read_thread(&bot(&home), &session_id, None, 5).await.unwrap();

  assert_eq!(fields(&own, "person"), [json!(ALLOWED_PERSON), Value::Null]);
}

/// The newest `limit` of the linked channel, all unread false, nothing acked.
#[tokio::test]
async fn read_with_url_reads_the_linked_channel_and_marks_nothing() {
  let home = Home::new_without_thread_runtime(true).await;
  home.fake().channels.insert(
    OTHER_CHANNEL.to_string(),
    (1..=5)
      .map(|i| {
        let message = Msg::new(i, &format!("m{i}"));
        if i == 3 { message.by(UNLISTED).value() } else { message.value() }
      })
      .collect(),
  );
  let session_id = make_session(&home, None).await;
  let url = format!("https://discord.com/channels/{GUILD}/{OTHER_CHANNEL}");

  let result = read_thread(&bot(&home), &session_id, Some(url.as_str()), 3).await.unwrap();

  assert_eq!(ids(&result), [3, 4, 5].map(snowflake));
  assert_eq!(fields(&result, "unread"), [false, false, false].map(Value::Bool));
  assert_eq!(messages(&result)[0]["person"], Value::Null);
  assert_eq!(result["watermark_id"], Value::Null);
  assert_eq!(result["more_unread"], 0);
  // Nothing was marked: the session's watermark is untouched and no ack landed.
  assert_eq!(home.meta(&session_id).await.extra("discord_watermark_id"), Some(&Value::Null));
  assert!(acks(&home, &session_id).is_empty());
  // Only the linked channel was read, newest-three shape (no `after`).
  assert_eq!(home.fake().calls, [format!("GET /api/v10/channels/{OTHER_CHANNEL}/messages?limit=3")]);
}

/// A 403 from Discord answers 404 "cannot see"; a non-discord.com link answers 422.
#[tokio::test]
async fn read_with_url_refuses_hidden_channel_and_bad_link() {
  let home = Home::new_without_thread_runtime(true).await;
  home.fake().hidden.push(OTHER_CHANNEL.to_string());
  let session_id = make_session(&home, None).await;
  let router = home.router();
  let url = format!("https://discord.com/channels/{GUILD}/{OTHER_CHANNEL}");

  let (hidden_status, hidden) =
    send(router.clone(), "/api/internal/discord/read", json!({"session_id": session_id, "url": url})).await;
  let (bad_status, bad) =
    send(router, "/api/internal/discord/read", json!({"session_id": session_id, "url": "https://x.com/no"})).await;

  assert_eq!(hidden_status, StatusCode::NOT_FOUND);
  assert_eq!(hidden["detail"], "the bot cannot see this channel");
  assert_eq!(bad_status, StatusCode::UNPROCESSABLE_ENTITY);
  assert!(bad["detail"].as_str().unwrap().contains("not a discord.com channel/message link"), "{bad}");
}

/// A session without a Discord thread refuses with 409 before any Discord call.
#[tokio::test]
async fn read_on_a_non_discord_session_answers_409() {
  let home = Home::new_without_thread_runtime(true).await;
  let session_id = home.plain_session("plain").await;

  let (status, body) = send(home.router(), "/api/internal/discord/read", json!({"session_id": session_id})).await;

  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(body["detail"], "Session has no Discord thread");
  assert!(home.fake().calls.is_empty());
}

#[tokio::test]
async fn reply_refuses_412_while_unread_then_posts_after_a_read() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "first follow up"), Msg::new(2, "second follow up")]);
  home.fake().channels.insert(PARENT.to_string(), Vec::new());
  let session_id = make_session(&home, None).await;
  let router = home.router();

  let (refused_status, refused) =
    send(router.clone(), "/api/internal/discord/reply", json!({"session_id": session_id, "text": "the answer"})).await;
  assert_eq!(refused_status, StatusCode::PRECONDITION_FAILED);
  assert_eq!(refused["detail"]["error"], "stale_thread");
  let new_messages = refused["detail"]["new_messages"].as_array().unwrap();
  assert_eq!(
    new_messages.iter().map(|message| message["id"].clone()).collect::<Vec<_>>(),
    [snowflake(1), snowflake(2)].map(Value::String)
  );
  assert!(home.fake().posts.is_empty());

  let (read_status, read) = send(router.clone(), "/api/internal/discord/read", json!({"session_id": session_id})).await;
  assert_eq!(read_status, StatusCode::OK);
  assert_eq!(read["more_unread"], 0);

  let (posted_status, posted) =
    send(router, "/api/internal/discord/reply", json!({"session_id": session_id, "text": "the answer"})).await;
  assert_eq!(posted_status, StatusCode::OK);
  assert_eq!(posted["posted"], true);
  assert_eq!(home.fake().posts, [(THREAD.to_string(), "the answer".to_string())]);
}

/// A stale reply returns the unread message and sends no post.
#[tokio::test]
async fn reply_refuses_412_while_unread() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "unread follow up")]);
  let session_id = make_session(&home, None).await;
  let router = home.router();

  let (refused_status, refused) =
    send(router, "/api/internal/discord/reply", json!({"session_id": session_id, "text": "the answer"})).await;
  assert_eq!(refused_status, StatusCode::PRECONDITION_FAILED);
  assert_eq!(refused["detail"]["error"], "stale_thread");
  let new_messages = refused["detail"]["new_messages"].as_array().unwrap();
  assert_eq!(
    new_messages.iter().map(|message| message["id"].clone()).collect::<Vec<_>>(),
    [snowflake(1)].map(Value::String)
  );
  assert!(home.fake().posts.is_empty());
}

#[tokio::test]
async fn reply_posts_after_reading_the_unread_thread() {
  let home = Home::new_without_thread_runtime(true).await;
  thread_messages(&home, &[Msg::new(1, "unread follow up")]);
  home.fake().channels.insert(PARENT.to_string(), Vec::new());
  let session_id = make_session(&home, None).await;
  let router = home.router();

  let (read_status, read) = send(router.clone(), "/api/internal/discord/read", json!({"session_id": session_id})).await;
  assert_eq!(read_status, StatusCode::OK);
  assert_eq!(read["more_unread"], 0);

  let (status, posted) =
    send(router, "/api/internal/discord/reply", json!({"session_id": session_id, "text": "the answer"})).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(posted["posted"], true);
  assert_eq!(home.fake().posts, [(THREAD.to_string(), "the answer".to_string())]);
}

/// A guild missing ADD_REACTIONS names it, the intent off reads false, and ok is false.
///
/// The pinned answer for this case names the missing permission and the intent state, so a change in either
/// read or in the answer's build order breaks this test.
#[tokio::test]
async fn check_reports_missing_permissions_and_intent_off() {
  let every_permission_but_reactions: u64 =
    REQUIRED_PERMISSIONS.iter().filter(|(name, _)| *name != "ADD_REACTIONS").map(|(_, bit)| bit).sum();
  let home = Home::new_without_thread_runtime(true).await;
  {
    let mut fake = home.fake();
    fake.user = json!({"id": BOT, "username": "charlie-bot"});
    fake.application = json!({"id": "600000000000000009", "flags": 0});
    fake.guilds = json!([
      {"id": GUILD, "name": "Research", "permissions": every_permission_but_reactions.to_string()},
      {"id": OTHER_GUILD, "name": "Ops", "permissions": (1u64 << 3).to_string()},
    ]);
  }

  let (status, body) = send(home.router(), "/api/internal/discord/check", json!({})).await;

  assert_eq!(status, StatusCode::OK);
  assert_eq!(
    body,
    json!({
      "ok": false,
      "bot_user": {"id": BOT, "username": "charlie-bot"},
      "application_id": "600000000000000009",
      "message_content_intent": false,
      "guilds": [
        {"id": GUILD, "name": "Research", "missing_permissions": ["ADD_REACTIONS"]},
        {"id": OTHER_GUILD, "name": "Ops", "missing_permissions": []},
      ],
    })
  );
  assert!(!body.to_string().contains(TEST_TOKEN));
}

/// No bot token set: 409 naming the key, before any Discord call.
#[tokio::test]
async fn check_without_a_token_answers_409() {
  let home = Home::new_without_thread_runtime(false).await;

  let (status, body) = send(home.router(), "/api/internal/discord/check", json!({})).await;

  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(body["detail"], "credentials.discord.bot_token is not set");
  assert!(home.fake().calls.is_empty());
}

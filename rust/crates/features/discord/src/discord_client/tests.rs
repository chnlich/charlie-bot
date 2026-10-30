//! The cases of `tests/test_discord_client.py`, run against a loopback server that answers as Discord's REST API does.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use charliebot_compat::http_client::{self, Settings};
use serde_json::json;

use super::*;
use crate::test_support::{Recorded, Reply, serve};

const TOKEN: &str = "synthetic-token-not-a-secret";

/// A client wired to a loopback server that answers with `responder`.
async fn client(responder: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> DiscordClient {
  let base = serve(responder).await;
  let http = http_client::client(&Settings::default()).expect("the default client builds");
  DiscordClient::new(http, base, TOKEN.to_string())
}

/// The recorded requests a responder saw, in order.
fn recorder() -> Arc<Mutex<Vec<Recorded>>> {
  Arc::new(Mutex::new(Vec::new()))
}

#[tokio::test]
async fn request_headers_carry_bot_token_and_user_agent() {
  let seen = recorder();
  let sink = Arc::clone(&seen);
  let client = client(move |request| {
    sink.lock().unwrap().push(request.clone());
    Reply::json(200, json!({"id": "111111111111111111", "username": "synthetic-bot"}))
  })
  .await;

  assert_eq!(client.get_current_user().await.unwrap().id, "111111111111111111");
  let seen = seen.lock().unwrap();
  assert_eq!(seen[0].path, "/api/v10/users/@me");
  assert_eq!(seen[0].headers["authorization"], format!("Bot {TOKEN}"));
  assert_eq!(seen[0].headers["user-agent"], "DiscordBot (https://discord.com, charlie-bot)");
}

#[tokio::test]
async fn create_message_nonce_is_first_25_hex_characters_of_post_key_sha256() {
  let seen = recorder();
  let sink = Arc::clone(&seen);
  let client = client(move |request| {
    sink.lock().unwrap().push(request.clone());
    Reply::json(200, json!({"id": "222222222222222222"}))
  })
  .await;

  client.create_message_with_post_key("333333333333333333", "hello", "reply#1").await.unwrap();

  assert_eq!(
    seen.lock().unwrap()[0].json(),
    json!({
      "content": "hello",
      "allowed_mentions": {"parse": []},
      "nonce": "ae0266ac0850de0ea0460b7d5",
      "enforce_nonce": true,
    })
  );
}

#[tokio::test]
async fn rate_limit_retries_once_with_reported_wait() {
  let calls = Arc::new(AtomicUsize::new(0));
  let counter = Arc::clone(&calls);
  let client = client(move |_| {
    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
      Reply::json(429, json!({"message": "You are being rate limited.", "retry_after": 0.05}))
    } else {
      Reply::json(200, json!({"url": "wss://gateway.example/synthetic"}))
    }
  })
  .await;

  let started = Instant::now();
  assert_eq!(client.get_gateway_url().await.unwrap(), "wss://gateway.example/synthetic");
  assert_eq!(calls.load(Ordering::SeqCst), 2);
  assert!(started.elapsed() >= Duration::from_millis(50), "the retry waited the reported 0.05 s");
}

#[tokio::test]
async fn rate_limit_three_times_raises() {
  let calls = Arc::new(AtomicUsize::new(0));
  let counter = Arc::clone(&calls);
  let client = client(move |_| {
    counter.fetch_add(1, Ordering::SeqCst);
    Reply::json(429, json!({"message": "rate limited"}))
  })
  .await;

  let error = client.get_current_user().await.unwrap_err();
  let api = error.downcast_ref::<DiscordAPIError>().expect("the failure is a Discord API error");
  assert_eq!(api.status, 429);
  assert_eq!(api.path, "/users/@me");
  assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn http_error_carries_code_and_message_without_token() {
  let client = client(|_| Reply::json(403, json!({"code": 50013, "message": "Missing Permissions"}))).await;

  let error = client.get_channel("555555555555555555").await.unwrap_err();
  let api = error.downcast_ref::<DiscordAPIError>().expect("the failure is a Discord API error");
  assert_eq!(
    (api.method.as_str(), api.path.as_str(), api.status, api.code, api.message.as_str()),
    ("GET", "/channels/555555555555555555", 403, Some(50013), "Missing Permissions")
  );
  let text = error.to_string();
  for fragment in ["GET", "/channels/555555555555555555", "403", "50013", "Missing Permissions"] {
    assert!(text.contains(fragment), "{text}");
  }
  assert!(!text.contains(TOKEN), "{text}");
}

#[tokio::test]
async fn get_message_fetches_one_message_by_id() {
  let body = json!({
    "id": "222222222222222222",
    "content": "hello",
    "author": {"id": "333333333333333333", "username": "synthetic"},
    "timestamp": "2026-08-26T00:00:00Z",
  });
  let seen = recorder();
  let sink = Arc::clone(&seen);
  let answer = body.clone();
  let client = client(move |request| {
    sink.lock().unwrap().push(request.clone());
    Reply::json(200, answer.clone())
  })
  .await;

  let message = client.get_message("111111111111111111", "222222222222222222").await.unwrap();
  assert_eq!((message.id.as_str(), message.content.as_deref()), ("222222222222222222", Some("hello")));
  assert_eq!(seen.lock().unwrap()[0].path, "/api/v10/channels/111111111111111111/messages/222222222222222222");
}

#[tokio::test]
async fn get_messages_sorts_oldest_first_by_integer_id() {
  let seen = recorder();
  let sink = Arc::clone(&seen);
  let client = client(move |request| {
    sink.lock().unwrap().push(request.clone());
    let message = |id: &str, content: &str| {
      json!({"id": id, "content": content, "author": {"id": "1", "username": "u"}, "timestamp": "t"})
    };
    Reply::json(200, json!([message("100000000000000000", "newest"), message("99999999999999999", "oldest")]))
  })
  .await;

  let messages = client.get_messages("666666666666666666", Some("888888888888888888"), 100).await.unwrap();
  let contents: Vec<&str> = messages.iter().filter_map(|message| message.content.as_deref()).collect();
  assert_eq!(contents, ["oldest", "newest"]);
  let query = &seen.lock().unwrap()[0].query;
  assert_eq!(query.get("limit").map(String::as_str), Some("100"));
  assert_eq!(query.get("after").map(String::as_str), Some("888888888888888888"));
  // The same ordering comparison, directly: string order would flip it.
  assert!(snowflake_key("99999999999999999") < snowflake_key("100000000000000000"));
}

#[tokio::test]
async fn thread_name_truncated_to_100_chars() {
  let seen = recorder();
  let sink = Arc::clone(&seen);
  let client = client(move |request| {
    sink.lock().unwrap().push(request.clone());
    Reply::json(200, json!({"id": "777777777777777777", "type": 11}))
  })
  .await;

  client.start_thread_from_message("333333333333333333", "222222222222222222", &"x".repeat(140)).await.unwrap();
  let seen = seen.lock().unwrap();
  assert_eq!(seen[0].path, "/api/v10/channels/333333333333333333/messages/222222222222222222/threads");
  assert_eq!(seen[0].json(), json!({"name": "x".repeat(100), "auto_archive_duration": 10080}));
}

#[tokio::test]
async fn reaction_emoji_is_url_encoded() {
  let seen = recorder();
  let sink = Arc::clone(&seen);
  let client = client(move |request| {
    sink.lock().unwrap().push(request.clone());
    Reply::empty(204)
  })
  .await;

  client.add_reaction("333333333333333333", "222222222222222222", "\u{1F440}").await.unwrap();
  client.remove_own_reaction("333333333333333333", "222222222222222222", "\u{1F440}").await.unwrap();
  let reaction = "/api/v10/channels/333333333333333333/messages/222222222222222222/reactions/%F0%9F%91%80/@me";
  let seen = seen.lock().unwrap();
  assert_eq!((seen[0].method.as_str(), seen[0].path.as_str()), ("PUT", reaction));
  assert_eq!((seen[1].method.as_str(), seen[1].path.as_str()), ("DELETE", reaction));
}

#[test]
fn parse_message_link_reads_channel_and_message_links() {
  assert_eq!(
    parse_message_link("https://discord.com/channels/111111111111111111/222222222222222222").unwrap(),
    MessageLink {
      guild_id: "111111111111111111".to_string(),
      channel_id: "222222222222222222".to_string(),
      message_id: None,
    }
  );
  assert_eq!(
    parse_message_link("https://discord.com/channels/111111111111111111/222222222222222222/333333333333333333")
      .unwrap()
      .message_id
      .as_deref(),
    Some("333333333333333333")
  );
}

#[test]
fn parse_message_link_rejects_non_discord_urls() {
  for bad in [
    "https://slack.com/channels/111111111111111111/222222222222222222",
    "http://discord.com/channels/111111111111111111/222222222222222222",
    "https://discord.com/channels/111111111111111111",
    "https://discord.com/channels/111111111111111111/222222222222222222/333333333333333333/444444444444444444",
    "https://discord.com/channels/guild/222222222222222222",
    "",
  ] {
    assert!(parse_message_link(bad).is_err(), "{bad}");
  }
}

#[test]
fn message_content_intent_bits() {
  assert!(!message_content_intent_enabled(0));
  assert!(message_content_intent_enabled(1 << 18));
  assert!(message_content_intent_enabled(1 << 19));
  assert!(!message_content_intent_enabled(1 << 17));
}

#[test]
fn missing_permissions_table_and_administrator() {
  let names: Vec<&str> = REQUIRED_PERMISSIONS.iter().map(|(name, _)| *name).collect();
  assert_eq!(
    names,
    [
      "VIEW_CHANNEL",
      "SEND_MESSAGES",
      "SEND_MESSAGES_IN_THREADS",
      "CREATE_PUBLIC_THREADS",
      "READ_MESSAGE_HISTORY",
      "ADD_REACTIONS",
    ]
  );
  assert_eq!(missing_permissions(0), names);
  let all = REQUIRED_PERMISSIONS.iter().fold(0, |bits, (_, bit)| bits | bit);
  assert!(missing_permissions(all).is_empty());
  assert!(missing_permissions(1 << 3).is_empty());
  let have = (1 << 10) | (1 << 11);
  assert_eq!(
    missing_permissions(have),
    ["SEND_MESSAGES_IN_THREADS", "CREATE_PUBLIC_THREADS", "READ_MESSAGE_HISTORY", "ADD_REACTIONS"]
  );
}

/// The wait a 429 asks for: the body's `retry_after`, else the `Retry-After` header, else zero.
#[test]
fn retry_after_seconds_reads_the_body_then_the_header() {
  assert_eq!(retry_after_seconds(br#"{"retry_after": 1.25}"#, Some("2")).unwrap(), 1.25);
  assert_eq!(retry_after_seconds(b"not json", Some("2")).unwrap(), 2.0);
  assert_eq!(retry_after_seconds(b"{}", None).unwrap(), 0.0);
  assert!(retry_after_seconds(b"{}", Some("soon")).is_err());
}

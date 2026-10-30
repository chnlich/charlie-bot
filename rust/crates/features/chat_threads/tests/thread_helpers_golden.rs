use std::collections::HashSet;
use std::io::Read;

use charliebot_chat_threads::thread_entry::{
  ThreadDetail, ThreadPlatform, ThreadReplyError, application_route_links, assert_no_file_server_links, chunk_text,
  follow_floor, follow_wake_label, lost_summons, newest_thread_input, noticed, nudged, operator_only_note, replied,
  unread_after,
};
use flate2::read::GzDecoder;
use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "../../slack/src/event_types.rs"]
mod slack_event_types;

#[allow(dead_code)]
#[path = "../../discord/src/event_types.rs"]
mod discord_event_types;

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

struct Message {
  id: String,
  human: bool,
}

#[test]
fn pure_thread_helpers_match_the_recorded_python_cases() {
  let mut decoder = GzDecoder::new(include_bytes!("fixtures/thread_helpers_golden.json.gz").as_slice());
  let mut fixture_text = String::new();
  decoder.read_to_string(&mut fixture_text).unwrap();
  let fixture = materialize_placeholders(serde_json::from_str(&fixture_text).unwrap());
  assert_eq!(fixture["source"], "rust/crates/features/chat_threads/tests/thread_helpers_differential.py");
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");

  let cases = fixture["cases"].as_object().unwrap();
  let expected = fixture["expected"].as_object().unwrap();
  let mut results = serde_json::Map::new();
  let mut total = 0;
  for (helper, helper_cases) in cases {
    let helper_cases = helper_cases.as_array().unwrap();
    let helper_expected = expected[helper].as_array().unwrap();
    assert_eq!(helper_cases.len(), helper_expected.len(), "{helper} fixture case count");
    let actual = helper_cases.iter().map(|case| answer(helper, case)).collect::<Vec<_>>();
    assert_eq!(actual, *helper_expected, "recorded Python helper {helper}");
    total += actual.len();
    results.insert(helper.clone(), Value::Array(actual));
  }
  assert_eq!(total, 115);

  let control = &fixture["control"];
  let helper = control["helper"].as_str().unwrap();
  let index = usize::try_from(control["case"].as_u64().unwrap()).unwrap();
  let append = control["append"].as_str().unwrap();
  let mut planted = results;
  let value = planted[helper][index][0].as_str().unwrap().to_string();
  planted[helper][index][0] = format!("{value}{append}").into();
  let differences = expected
    .iter()
    .map(|(helper, expected)| {
      let actual = planted[helper].as_array().unwrap();
      actual.iter().zip(expected.as_array().unwrap()).filter(|(actual, expected)| actual != expected).count()
        + actual.len().abs_diff(expected.as_array().unwrap().len())
    })
    .sum::<usize>();
  assert_eq!(differences, 1, "the planted control difference must be reported once");
}

fn answer(helper: &str, case: &Value) -> Value {
  match helper {
    "chunk_text" => json!(chunk_text(str_of(case, "text"), usize_of(case, "limit"))),
    "follow_wake_label" => json!(follow_wake_label(platform_of(case), str_of(case, "floor"))),
    "follow_floor" => json!(follow_floor(str_of(case, "label"))),
    "newest_thread_input" => {
      let events = array_of(case, "events");
      let event_ids = strings_of(case, "event_ids");
      json!(newest_thread_input(platform_of(case), &events, &event_ids))
    }
    "replied" => json!(replied(platform_of(case), &array_of(case, "events"), str_of(case, "summon_id"))),
    "nudged" => json!(nudged(platform_of(case), &array_of(case, "events"), str_of(case, "summon_id"))),
    "noticed" => json!(noticed(platform_of(case), &array_of(case, "events"), str_of(case, "summon_id"))),
    "lost_summons" => json!(lost_summons(
      platform_of(case),
      &array_of(case, "events"),
      &strings_of(case, "owned").into_iter().collect::<HashSet<_>>(),
      &strings_of(case, "running").into_iter().collect::<HashSet<_>>(),
    )),
    "unread_after" => {
      let messages = array_of(case, "messages")
        .iter()
        .map(|message| Message { id: str_of(message, "id").to_string(), human: message["human"].as_bool().unwrap() })
        .collect::<Vec<_>>();
      let watermark = case["watermark"].as_str().map(str::to_string);
      let platform = platform_of(case);
      json!(
        unread_after(
          &messages,
          |message| message.human,
          |message| message.id.as_str(),
          watermark.as_deref(),
          platform.id_key
        )
        .iter()
        .map(|message| message.id.clone())
        .collect::<Vec<_>>()
      )
    }
    "assert_no_file_server_links" => match assert_no_file_server_links(str_of(case, "text"), None) {
      Ok(()) => json!({"refused": false}),
      Err(error) => refusal(&error),
    },
    "application_route_links" => {
      json!(application_route_links(str_of(case, "text"), u16::try_from(usize_of(case, "port")).unwrap()))
    }
    "operator_only_note" => json!(operator_only_note(&strings_of(case, "links"))),
    other => panic!("unknown recorded helper {other:?}"),
  }
}

fn materialize_placeholders(mut value: Value) -> Value {
  match &mut value {
    Value::String(text) => {
      for (placeholder, replacement) in [
        ("$LOOPBACK_HOST", "localhost"),
        ("$NON_ASCII_PORT", "١٨٤٩٨"),
        ("$LOOPBACK_PORT", "18498"),
        ("$APP_HOST", "charliebot.example"),
        ("$PUBLISH_HOST", "publish.example"),
        ("$TEST_HOST", "host.example"),
        ("$LOOPBACK_IP", "127.0.0.1"),
        ("$ALT_PORT", "8080"),
        ("$HOME_PATH", "/home/u"),
      ] {
        *text = text.replace(placeholder, replacement);
      }
    }
    Value::Array(values) => values.iter_mut().for_each(|value| *value = materialize_placeholders(value.clone())),
    Value::Object(values) => values.values_mut().for_each(|value| *value = materialize_placeholders(value.clone())),
    _ => {}
  }
  value
}

fn refusal(error: &ThreadReplyError) -> Value {
  let detail = match &error.detail {
    ThreadDetail::Text(text) => text.clone(),
    ThreadDetail::Structured(value) => value.to_string(),
  };
  json!({"refused": true, "status": error.status.as_u16(), "detail": detail})
}

fn platform_of(case: &Value) -> &'static ThreadPlatform {
  match str_of(case, "platform") {
    "slack" => &slack_thread_contract::SLACK,
    "discord" => &discord_thread_contract::DISCORD,
    other => panic!("unknown recorded platform {other:?}"),
  }
}

fn str_of<'a>(case: &'a Value, key: &str) -> &'a str {
  case[key].as_str().unwrap()
}

fn usize_of(case: &Value, key: &str) -> usize {
  case[key].as_u64().unwrap() as usize
}

fn array_of(case: &Value, key: &str) -> Vec<Value> {
  case[key].as_array().unwrap().clone()
}

fn strings_of(case: &Value, key: &str) -> Vec<String> {
  case[key].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_string()).collect()
}

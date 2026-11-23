//! A multi-turn Claude Code stream answers with its result event's text. The assistant text before it stays out.

use charliebot_agents::one_shot::FinalResult;
use serde_json::Value;

/// A live run's stream, trimmed: three assistant text messages with their tool calls, then the run's final message and
/// its result event.
const MULTI_TURN_STREAM: &str = include_str!("fixtures/multi_turn_result_stream.ndjson");

#[tokio::test]
async fn the_answer_of_a_multi_turn_stream_is_its_result_event_text() {
  let events: Vec<Value> = MULTI_TURN_STREAM.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
  let result_text = events
    .iter()
    .rev()
    .find(|event| event["type"] == "result")
    .and_then(|event| event["result"].as_str())
    .unwrap()
    .to_string();
  assert!(events.iter().filter(|event| event["type"] == "assistant").count() > 2, "the stream has several turns");

  let final_result = FinalResult::default();
  let on_event = final_result.on_event();
  for event in events {
    on_event(event).await.unwrap();
  }
  let answer = final_result.text().unwrap();
  assert_eq!(answer, result_text.trim());
  assert!(!answer.contains("I have the finding"), "intermediate assistant text is not the answer: {answer}");
}

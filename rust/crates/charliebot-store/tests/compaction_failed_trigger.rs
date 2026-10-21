use charliebot_store::MessageAggregator;
use serde_json::json;

// Python: tests/test_message_aggregator.py:270.
#[test]
fn idle_compaction_failure_line_names_its_trigger() {
  let mut aggregator = MessageAggregator::new(0, false);
  let deltas = aggregator
    .feed(&json!({
      "type": "context_compact_failed",
      "error": "boom",
      "model": "claude-opus-5-5",
      "trigger": "idle"
    }))
    .unwrap();

  assert_eq!(deltas[0]["message"]["content"], "Compaction (idle) by Opus failed — boom");
}

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::aggregator::MessageAggregator;
use crate::hooks::EventRenderer;
use crate::message_events;
use crate::{ChatEvent, StoreError, StoreResult};

const PAGE_BODY_CACHE_LIMIT: usize = 4;

#[derive(Default)]
struct PageBodyCache {
  values: HashMap<(usize, usize), Vec<u8>>,
  order: VecDeque<(usize, usize)>,
}

impl PageBodyCache {
  fn get(&mut self, key: (usize, usize)) -> Option<Vec<u8>> {
    let body = self.values.get(&key)?.clone();
    self.order.retain(|existing| *existing != key);
    self.order.push_back(key);
    Some(body)
  }

  fn store(&mut self, key: (usize, usize), body: Vec<u8>) {
    self.order.retain(|existing| *existing != key);
    self.order.push_back(key);
    self.values.insert(key, body);
    while self.order.len() > PAGE_BODY_CACHE_LIMIT {
      let oldest = self.order.pop_front().expect("page body cache has an oldest key");
      self.values.remove(&oldest);
    }
  }
}

/// Memoized, turn-aligned message history for one live session.
pub struct MessageProjection {
  offset: usize,
  aggregator: MessageAggregator,
  committed_final: Vec<Value>,
  separators_final: Vec<usize>,
  region_events: Vec<Value>,
  committed: Vec<Value>,
  separator_ordinals: Vec<usize>,
  pending_draft: Option<Value>,
  history: Vec<Value>,
  event_count: usize,
  page_bodies: Mutex<PageBodyCache>,
  page_body_gzips: Mutex<PageBodyCache>,
}

impl MessageProjection {
  /// Build a projection from a session's live events.
  pub fn new<T: AsRef<ChatEvent>>(
    events: &[T],
    event_index_offset: usize,
    event_renderers: HashMap<String, Arc<dyn EventRenderer>>,
  ) -> StoreResult<Self> {
    let owned = events.iter().map(|event| event.as_ref().to_value()).collect::<Vec<_>>();
    Self::from_values(owned, event_index_offset, event_renderers)
  }

  /// Build a projection from already-converted live events.
  fn from_values(
    owned: Vec<Value>,
    event_index_offset: usize,
    event_renderers: HashMap<String, Arc<dyn EventRenderer>>,
  ) -> StoreResult<Self> {
    let mut projection = Self {
      offset: event_index_offset,
      aggregator: MessageAggregator::with_renderers(event_index_offset, false, event_renderers),
      committed_final: Vec::new(),
      separators_final: Vec::new(),
      region_events: Vec::new(),
      committed: Vec::new(),
      separator_ordinals: Vec::new(),
      pending_draft: None,
      history: Vec::new(),
      event_count: event_index_offset,
      page_bodies: Mutex::new(PageBodyCache::default()),
      page_body_gzips: Mutex::new(PageBodyCache::default()),
    };
    projection.ingest(owned)?;
    Ok(projection)
  }

  /// Return a copy advanced by newly appended events.
  pub fn advanced<T: AsRef<ChatEvent>>(&self, events: &[T]) -> StoreResult<Self> {
    let mut copied = Self {
      offset: self.offset,
      aggregator: self.aggregator.clone_state(),
      committed_final: self.committed_final.clone(),
      separators_final: self.separators_final.clone(),
      region_events: self.region_events.clone(),
      committed: Vec::new(),
      separator_ordinals: Vec::new(),
      pending_draft: None,
      history: Vec::new(),
      event_count: self.event_count,
      page_bodies: Mutex::new(PageBodyCache::default()),
      page_body_gzips: Mutex::new(PageBodyCache::default()),
    };
    copied.ingest(events.iter().map(|event| event.as_ref().to_value()).collect())?;
    Ok(copied)
  }

  /// Return committed messages.
  pub fn committed(&self) -> &[Value] {
    &self.committed
  }

  /// Return the pending assistant draft.
  pub fn pending_draft(&self) -> Option<&Value> {
    self.pending_draft.as_ref()
  }

  /// Return committed messages followed by the pending draft, when present.
  pub fn history(&self) -> &[Value] {
    &self.history
  }

  /// Return the event count in the global event-index space.
  pub fn event_count(&self) -> usize {
    self.event_count
  }

  /// Return the cached serialized body for one page.
  pub fn cached_page_body(&self, before: usize, limit: usize) -> Option<Vec<u8>> {
    self.page_bodies.lock().expect("page body cache mutex is not poisoned").get((before, limit))
  }

  /// Cache the serialized body for one page.
  pub fn store_page_body(&self, before: usize, limit: usize, body: Vec<u8>) {
    self.page_bodies.lock().expect("page body cache mutex is not poisoned").store((before, limit), body);
  }

  /// Return the cached gzip body for one page.
  pub fn cached_page_body_gzip(&self, before: usize, limit: usize) -> Option<Vec<u8>> {
    self.page_body_gzips.lock().expect("gzip page body cache mutex is not poisoned").get((before, limit))
  }

  /// Cache the gzip body for one page.
  pub fn store_page_body_gzip(&self, before: usize, limit: usize, body: Vec<u8>) {
    self.page_body_gzips.lock().expect("gzip page body cache mutex is not poisoned").store((before, limit), body);
  }

  /// Return event indexes of separator messages in committed-history ordinal space.
  pub fn separator_ordinals(&self) -> &[usize] {
    &self.separator_ordinals
  }

  /// Return the latest turn-aligned message page.
  pub fn tail(&self, limit: usize) -> (Vec<Value>, usize, bool) {
    let total = self.committed.len();
    let start = self.snap_to_turn_start(total.saturating_sub(limit));
    (self.committed[start..].to_vec(), start, start > 0)
  }

  /// Return the turn-aligned page before the message ordinal.
  pub fn slice_before(&self, before: usize, limit: usize) -> (Vec<Value>, usize, bool) {
    let before = before.min(self.committed.len());
    let start = self.snap_to_turn_start(before.saturating_sub(limit));
    (self.committed[start..before].to_vec(), start, start > 0)
  }

  fn ingest(&mut self, owned: Vec<Value>) -> StoreResult<()> {
    self.event_count += owned.len();
    self.region_events.extend(owned);
    let closed = message_events::stable_closed_prefix_len(&self.region_events);
    if closed > 0 {
      let prefix = self.region_events[..closed].to_vec();
      self.region_events.drain(..closed);
      let base = self.event_count - self.region_events.len() - closed - self.offset;
      let (messages, separators) = fold_messages(&mut self.aggregator, &prefix, base)?;
      let committed_len = self.committed_final.len();
      self.separators_final.extend(separators.into_iter().map(|pos| committed_len + pos));
      self.committed_final.extend(messages);
    }

    let mut view_aggregator = self.aggregator.clone_state();
    let region_base = self.event_count - self.region_events.len() - self.offset;
    let (region_messages, region_separators) = fold_messages(&mut view_aggregator, &self.region_events, region_base)?;
    self.committed = self.committed_final.clone();
    self.committed.extend(region_messages);
    self.separator_ordinals = self.separators_final.clone();
    self.separator_ordinals.extend(region_separators.into_iter().map(|pos| self.committed_final.len() + pos));
    self.pending_draft = view_aggregator.pending_draft_message();
    self.history.clone_from(&self.committed);
    if let Some(draft) = &self.pending_draft {
      self.history.push(draft.clone());
    }
    Ok(())
  }

  fn snap_to_turn_start(&self, desired: usize) -> usize {
    let preceding = self.separator_ordinals.partition_point(|ordinal| *ordinal < desired);
    preceding.checked_sub(1).map_or(0, |index| self.separator_ordinals[index] + 1)
  }
}

/// Convert an event list into committed messages, including a final draft flush.
pub fn events_to_messages<T: AsRef<ChatEvent>>(
  events: &[T],
  event_index_offset: usize,
  event_renderers: HashMap<String, Arc<dyn EventRenderer>>,
) -> StoreResult<Vec<Value>> {
  let mut aggregator = MessageAggregator::with_renderers(event_index_offset, false, event_renderers);
  let owned = events.iter().map(|event| event.as_ref().to_value()).collect::<Vec<_>>();
  let indexed = message_events::stable_history_projection(&owned)?;
  let mut messages: Vec<Value> = aggregator
    .feed_indexed(&indexed)?
    .into_iter()
    .filter_map(|delta| {
      (delta.get("type").and_then(Value::as_str) == Some("message")).then(|| delta["message"].clone())
    })
    .collect();
  messages.extend(aggregator.flush_pending().into_iter().map(|delta| delta["message"].clone()));
  Ok(messages)
}

/// Convert events into committed messages and an optional pending draft.
pub fn events_to_view<T: AsRef<ChatEvent>>(
  events: &[T],
  event_index_offset: usize,
  event_renderers: HashMap<String, Arc<dyn EventRenderer>>,
) -> StoreResult<(Vec<Value>, Option<Value>)> {
  let mut aggregator = MessageAggregator::with_renderers(event_index_offset, false, event_renderers);
  let owned = events.iter().map(|event| event.as_ref().to_value()).collect::<Vec<_>>();
  let indexed = message_events::stable_history_projection(&owned)?;
  let messages = aggregator
    .feed_indexed(&indexed)?
    .into_iter()
    .filter_map(|delta| {
      (delta.get("type").and_then(Value::as_str) == Some("message")).then(|| delta["message"].clone())
    })
    .collect();
  Ok((messages, aggregator.pending_draft_message()))
}

fn fold_messages(
  aggregator: &mut MessageAggregator,
  events: &[Value],
  base: usize,
) -> StoreResult<(Vec<Value>, Vec<usize>)> {
  let indexed = message_events::stable_history_projection(events)?
    .into_iter()
    .map(|(index, event)| (base + index, event))
    .collect::<Vec<_>>();
  let mut messages = Vec::new();
  let mut separators = Vec::new();
  for delta in aggregator.feed_indexed(&indexed)? {
    if delta.get("type").and_then(Value::as_str) != Some("message") {
      continue;
    }
    let message = delta.get("message").ok_or_else(|| StoreError::new("message delta is missing its message"))?.clone();
    if message.get("role").and_then(Value::as_str) == Some("separator") {
      separators.push(messages.len());
    }
    messages.push(message);
  }
  Ok((messages, separators))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::store::test_support::compact_events;
  use crate::store::test_support::{assistant_event, queued_user_reorder_events};
  use serde_json::json;

  #[test]
  fn projection_history_matches_events_to_messages_and_keeps_draft_separate() {
    let events = vec![
      json!({"id": "u1", "type": "user", "content": "question"}),
      json!({"id": "a1", "type": "assistant", "message": {"content": [{"type": "text", "text": "answer"}]}}),
      json!({"id": "done", "type": "master_done"}),
      json!({"id": "u2", "type": "user", "content": "next"}),
      json!({"id": "a2", "type": "assistant", "message": {"content": [{"type": "text", "text": "draft"}]}}),
    ];
    let projection =
      MessageProjection::new(&crate::store::test_support::compact_events(&events), 0, HashMap::new()).unwrap();
    assert_eq!(projection.history(), events_to_messages(&compact_events(&events), 0, HashMap::new()).unwrap());
    let (tail, _, _) = projection.tail(40);
    assert!(!tail.iter().any(|message| message["content"] == "draft"));
    assert_eq!(projection.pending_draft().unwrap()["content"], "draft");
  }

  fn pending_draft_events() -> Vec<Value> {
    vec![
      json!({"id": "user-1", "type": "user", "content": "hi", "timestamp": "t1"}),
      json!({
        "id": "assistant-1",
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": "draft response"}]},
        "timestamp": "t2",
      }),
    ]
  }

  fn many_messages_events(count: usize) -> Vec<Value> {
    (0..count)
      .map(|i| {
        json!({
          "id": format!("u{i}"),
          "type": "user",
          "content": format!("msg-{i}"),
          "timestamp": format!("2026-01-01T00:{i:02}:00Z"),
        })
      })
      .collect()
  }

  /// One separator-terminated turn per entry: a user, `length - 1` assistant texts, a master_done.
  fn turned_messages_events(turn_lengths: &[usize]) -> Vec<Value> {
    let mut events = Vec::new();
    for (turn, length) in turn_lengths.iter().enumerate() {
      events.push(json!({"id": format!("u{turn}"), "type": "user", "content": format!("q{turn}")}));
      for step in 0..length - 1 {
        events.push(assistant_event(&format!("a{turn}-{step}"), &format!("a{turn}-{step}")));
      }
      events.push(json!({"id": format!("done{turn}"), "type": "master_done", "thinking_seconds": 1}));
    }
    events
  }

  #[test]
  fn projection_history_equals_events_to_messages_for_each_fixture() {
    let fixtures = [
      ("reorder", queued_user_reorder_events()),
      ("pending_draft", pending_draft_events()),
      ("many_messages", many_messages_events(60)),
      ("empty", Vec::new()),
    ];
    for (name, events) in fixtures {
      let projection =
        MessageProjection::new(&crate::store::test_support::compact_events(&events), 0, HashMap::new()).unwrap();
      assert_eq!(
        projection.history(),
        events_to_messages(&compact_events(&events), 0, HashMap::new()).unwrap(),
        "projection history differs from events_to_messages for {name}"
      );
    }
  }

  #[test]
  fn lossless_backwards_walk_returns_full_id_set_for_each_limit() {
    let projection =
      MessageProjection::new(&compact_events(&turned_messages_events(&[3, 1, 6, 2].repeat(60))), 0, HashMap::new())
        .unwrap();
    let committed = projection.committed();
    let full_ids: Vec<&str> = committed.iter().map(|message| message["id"].as_str().unwrap()).collect();
    assert_eq!(committed.len(), 960);

    for limit in [1, 7, 40, 100] {
      let mut collected_ids: Vec<String> = Vec::new();
      let mut before = committed.len();
      while before > 0 {
        let (page, next, more) = projection.slice_before(before, limit);
        assert!(next == 0 || committed[next - 1]["role"] == "separator", "page start must be a turn start");
        assert!(
          page.len() >= limit || next == 0,
          "page must hold at least `limit` messages unless history is exhausted"
        );
        assert!(next < before, "next_before must be strictly < before for a non-empty page");
        assert_eq!(more, next > 0);
        collected_ids.extend(page.iter().map(|message| message["id"].as_str().unwrap().to_string()));
        before = next;
      }
      assert_eq!(collected_ids.len(), full_ids.len(), "limit {limit} collected a different count");
      let mut sorted_collected = collected_ids.clone();
      sorted_collected.sort();
      sorted_collected.dedup();
      assert_eq!(sorted_collected.len(), collected_ids.len(), "limit {limit} repeated an id");
      for id in &full_ids {
        assert!(collected_ids.iter().any(|collected| collected == id), "limit {limit} dropped {id}");
      }
    }
  }

  #[test]
  fn pages_start_after_a_separator_and_cover_every_message() {
    let mut events = Vec::new();
    for turn in 0..12 {
      events.push(json!({"id": format!("u{turn}"), "type": "user", "content": "q"}));
      events.push(json!({"id": format!("d{turn}"), "type": "master_done"}));
    }
    let projection =
      MessageProjection::new(&crate::store::test_support::compact_events(&events), 0, HashMap::new()).unwrap();
    let mut before = projection.committed().len();
    let mut ids = Vec::new();
    while before > 0 {
      let (page, next, more) = projection.slice_before(before, 3);
      assert!(next == 0 || projection.committed()[next - 1]["role"] == "separator");
      assert!(more == (next > 0));
      assert!(next < before);
      ids.extend(page.iter().map(|message| message["id"].as_str().unwrap().to_string()));
      before = next;
    }
    assert_eq!(ids.len(), projection.committed().len());
  }

  #[test]
  fn an_open_attach_interval_reorders_a_queued_user_after_master_done() {
    let events = vec![
      json!({"id": "attach", "type": "session_attached", "session_id": "native"}),
      json!({"id": "queued", "type": "user", "content": "queued"}),
      json!({"id": "answer", "type": "assistant", "message": {"content": [{"type": "text", "text": "answer"}]}}),
      json!({"id": "done", "type": "master_done"}),
    ];
    let messages = events_to_messages(&compact_events(&events), 0, HashMap::new()).unwrap();
    assert_eq!(
      messages.iter().map(|message| message["id"].as_str().unwrap()).collect::<Vec<_>>(),
      vec!["answer", "done", "queued"]
    );
  }

  #[test]
  fn incremental_projection_matches_the_reference_for_each_append() {
    let mut projection = MessageProjection::new(&[] as &[ChatEvent], 0, HashMap::new()).unwrap();
    let mut events = Vec::new();
    for event in [
      json!({"id": "before", "type": "user", "content": "before"}),
      json!({"id": "attach", "type": "session_attached", "session_id": "native"}),
      json!({"id": "queued", "type": "user", "content": "queued"}),
      json!({"id": "answer", "type": "assistant", "message": {"content": [{"type": "text", "text": "answer"}]}}),
      json!({"id": "done", "type": "master_done", "thinking_seconds": 2}),
      json!({"id": "draft", "type": "assistant", "message": {"content": [{"type": "text", "text": "draft"}]}}),
    ] {
      projection = projection
        .advanced(std::slice::from_ref(compact_events(std::slice::from_ref(&event)).first().expect("one event")))
        .unwrap();
      events.push(event);
      let (committed, pending) = events_to_view(&compact_events(&events), 0, HashMap::new()).unwrap();
      assert_eq!(projection.committed(), committed);
      assert_eq!(projection.pending_draft(), pending.as_ref());
      assert_eq!(projection.history(), events_to_messages(&compact_events(&events), 0, HashMap::new()).unwrap());
    }
  }

  #[test]
  fn page_body_caches_are_lru_capped_and_reset_on_advance() {
    let projection = MessageProjection::new(&[] as &[ChatEvent], 0, HashMap::new()).unwrap();
    for before in 0..4 {
      projection.store_page_body(before, 10, vec![before as u8]);
      projection.store_page_body_gzip(before, 10, vec![before as u8 + 10]);
    }
    assert_eq!(projection.cached_page_body(0, 10), Some(vec![0]));
    projection.store_page_body(4, 10, vec![4]);
    assert_eq!(projection.cached_page_body(0, 10), Some(vec![0]));
    assert_eq!(projection.cached_page_body(1, 10), None);
    assert_eq!(projection.cached_page_body_gzip(1, 10), Some(vec![11]));

    let advanced = projection.advanced(&[] as &[ChatEvent]).unwrap();
    assert_eq!(advanced.cached_page_body(0, 10), None);
    assert_eq!(advanced.cached_page_body_gzip(1, 10), None);
  }
}

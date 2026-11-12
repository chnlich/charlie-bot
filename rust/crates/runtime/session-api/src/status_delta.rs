//! Row deltas for the sidebar's status poll.
//!
//! A status poll answers the rows whose state changed since the answer a page already holds. A poll that
//! follows a task-tree change therefore carries one row instead of every row. The page passes back the `token`
//! of its last answer; the token names a sequence of published states and this process's epoch. A token from
//! another epoch (a server restart) or none at all asks for the whole set.
//!
//! A row's version is the sequence at which its payload last changed, or at which it entered a filter's set. A row
//! that leaves a filter's set gets a version too, under that filter's name, so a page that held it drops it. The first
//! request of a filter records that filter's set; the row sets live here rather than in the route because two pages
//! polling the same filter share them.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// How far behind the newest sequence a version is kept. A token older than the horizon is answered with the whole
/// set, because a pruned row's version can no longer prove the page holds the row's current state.
const RETAIN: u64 = 1 << 16;

static STATE: LazyLock<Mutex<Snapshot>> = LazyLock::new(|| Mutex::new(Snapshot::new()));

/// The answer to one status poll: the token that names the state it answers, the rows it carries, and the ids that
/// left the filter's set.
pub struct Answer {
  pub token: String,
  pub rows: Map<String, Value>,
  pub gone: Vec<String>,
}

/// One published row state and the sequence that state began at.
struct Published {
  version: u64,
  payload: Value,
}

/// The published row states and each filter's last recorded row set.
pub struct Snapshot {
  epoch: u64,
  sequence: u64,
  horizon: u64,
  rows: HashMap<String, Published>,
  filters: HashMap<String, HashSet<String>>,
  departed: HashMap<(String, String), u64>,
}

impl Snapshot {
  fn new() -> Snapshot {
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |elapsed| elapsed.as_nanos() as u64);
    Snapshot { epoch, sequence: 0, horizon: 0, rows: HashMap::new(), filters: HashMap::new(), departed: HashMap::new() }
  }

  /// Publish this request's row payloads, record the filter's set, and answer the rows that moved after `since`.
  ///
  /// `members` is the filter's own sorted row set, and `ids` the request's, which adds the sessions the page names
  /// beyond the filter. Membership follows `members` alone. Two pages on the same filter name different extra sessions
  /// (each one's open chat). A set that took the extras in would make those rows enter and leave on every
  /// alternating poll. `payloads` holds one payload per id in `ids`, in the same order. A `since` that names no state
  /// this process published, or one older than the retained horizon, asks for every row.
  pub fn answer(
    &mut self,
    filter: Option<&str>,
    members: &[String],
    ids: &[String],
    payloads: Vec<Value>,
    since: Option<u64>,
  ) -> Answer {
    let full = since.is_none_or(|since| since > self.sequence || since < self.horizon);
    for (id, payload) in ids.iter().zip(payloads) {
      match self.rows.get(id) {
        Some(row) if row.payload == payload => {}
        _ => {
          self.sequence += 1;
          self.rows.insert(id.clone(), Published { version: self.sequence, payload });
        }
      }
    }
    let mut gone = Vec::new();
    if let Some(filter) = filter {
      let current: HashSet<String> = members.iter().cloned().collect();
      if let Some(previous) = self.filters.insert(filter.to_string(), current.clone()) {
        for id in previous.difference(&current) {
          self.sequence += 1;
          self.departed.insert((filter.to_string(), id.clone()), self.sequence);
        }
        // A row entering the set is new to the pages that hold this filter, even when its payload never moved, so it
        // is published again for them.
        for id in current.difference(&previous) {
          self.sequence += 1;
          if let Some(row) = self.rows.get_mut(id) {
            row.version = self.sequence;
          }
        }
      }
      if !full {
        let since = since.expect("a delta request carries its sequence");
        let mut departed: Vec<(&String, u64)> = self
          .departed
          .iter()
          .filter(|((name, _), version)| name == filter && **version > since)
          .map(|((_, id), version)| (id, *version))
          .collect();
        departed.sort_by_key(|left| left.1);
        gone.extend(departed.into_iter().map(|(id, _)| id.clone()));
      }
    }
    let since = since.unwrap_or(u64::MAX);
    let mut rows = Map::new();
    for id in ids {
      if let Some(row) = self.rows.get(id)
        && (full || row.version > since)
      {
        rows.insert(id.clone(), row.payload.clone());
      }
    }
    if !full {
      let horizon = self.sequence.saturating_sub(RETAIN);
      if horizon > self.horizon {
        self.horizon = horizon;
        self.rows.retain(|_, row| row.version > horizon);
        self.departed.retain(|_, version| *version > horizon);
      }
    }
    Answer { token: format!("{}-{}", self.epoch, self.sequence), rows, gone }
  }
}

/// Answer one status poll against the shared snapshot. `filter` names the set whose membership is tracked, `members`
/// that set, `ids` the request's sorted row set, and `payloads` one serialized row state per id.
pub fn record(
  filter: Option<&str>,
  members: &[String],
  ids: &[String],
  payloads: Vec<Value>,
  token: Option<&str>,
) -> Answer {
  let mut state = STATE.lock().expect("status delta mutex is not poisoned");
  let since = token.and_then(|token| parse_token(token, state.epoch));
  state.answer(filter, members, ids, payloads, since)
}

/// The sequence a token names, when the token was published by the epoch `epoch`.
fn parse_token(token: &str, epoch: u64) -> Option<u64> {
  let (token_epoch, sequence) = token.split_once('-')?;
  if token_epoch.parse::<u64>().ok()? != epoch {
    return None;
  }
  sequence.parse().ok()
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn snapshot() -> Snapshot {
    Snapshot::new()
  }

  fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|id| id.to_string()).collect()
  }

  #[test]
  fn a_first_poll_answers_every_row_and_a_repeat_answers_none() {
    let mut state = snapshot();
    let full =
      state.answer(Some("all"), &ids(&["a", "b"]), &ids(&["a", "b"]), vec![json!({"n": 1}), json!({"n": 2})], None);
    assert_eq!(full.rows.len(), 2);
    assert!(full.gone.is_empty());

    let since = parse_token(&full.token, state.epoch);
    let repeat =
      state.answer(Some("all"), &ids(&["a", "b"]), &ids(&["a", "b"]), vec![json!({"n": 1}), json!({"n": 2})], since);
    assert!(repeat.rows.is_empty(), "no row moved, so the delta carries none");
  }

  #[test]
  fn a_delta_answers_only_the_row_whose_payload_moved() {
    let mut state = snapshot();
    let full =
      state.answer(Some("all"), &ids(&["a", "b"]), &ids(&["a", "b"]), vec![json!({"n": 1}), json!({"n": 2})], None);
    let since = parse_token(&full.token, state.epoch);

    let delta =
      state.answer(Some("all"), &ids(&["a", "b"]), &ids(&["a", "b"]), vec![json!({"n": 9}), json!({"n": 2})], since);
    assert_eq!(delta.rows.keys().collect::<Vec<_>>(), vec!["a"]);
  }

  #[test]
  fn a_row_that_leaves_the_filter_is_named_and_a_row_that_enters_is_published() {
    let mut state = snapshot();
    let full =
      state.answer(Some("all"), &ids(&["a", "b"]), &ids(&["a", "b"]), vec![json!({"n": 1}), json!({"n": 2})], None);
    let since = parse_token(&full.token, state.epoch);

    let leaving = state.answer(Some("all"), &ids(&["a"]), &ids(&["a"]), vec![json!({"n": 1})], since);
    assert_eq!(leaving.gone, vec!["b".to_string()]);

    let since = parse_token(&leaving.token, state.epoch);
    // `b` re-enters with the payload it held before, so its version would not move: the entry publishes it again.
    let entering =
      state.answer(Some("all"), &ids(&["a", "b"]), &ids(&["a", "b"]), vec![json!({"n": 1}), json!({"n": 2})], since);
    assert!(entering.rows.contains_key("b"), "an entering row answers its page again");
    assert!(!entering.rows.contains_key("a"), "an unchanged row that stayed in the set does not");
  }

  #[test]
  fn a_token_from_another_epoch_asks_for_the_whole_set() {
    let mut state = snapshot();
    let other = format!("{}-1", state.epoch.wrapping_add(1));
    assert_eq!(parse_token(&other, state.epoch), None);
    let answer =
      state.answer(Some("all"), &ids(&["a"]), &ids(&["a"]), vec![json!({"n": 1})], parse_token(&other, state.epoch));
    assert_eq!(answer.rows.len(), 1);
  }

  #[test]
  fn a_named_row_set_without_a_filter_answers_every_named_row() {
    let mut state = snapshot();
    let full = state.answer(None, &[], &ids(&["a"]), vec![json!({"n": 1})], None);
    let since = parse_token(&full.token, state.epoch);
    let repeat = state.answer(None, &[], &ids(&["a"]), vec![json!({"n": 1})], since);
    assert!(repeat.rows.is_empty());
  }

  /// Two pages on one filter name different extra sessions, each its own open chat. Membership follows the filter's own
  /// set, so the alternating extras do not enter and leave on every poll.
  #[test]
  fn a_filters_membership_ignores_the_extra_ids_a_page_names() {
    let mut state = snapshot();
    let members = ids(&["a", "b"]);
    let first = state.answer(
      Some("all"),
      &members,
      &ids(&["a", "b", "open-1"]),
      vec![json!({"n": 1}), json!({"n": 2}), json!({"n": 3})],
      None,
    );
    let since = parse_token(&first.token, state.epoch);
    let other = state.answer(
      Some("all"),
      &members,
      &ids(&["a", "b", "open-2"]),
      vec![json!({"n": 1}), json!({"n": 2}), json!({"n": 4})],
      since,
    );
    assert!(other.gone.is_empty(), "no row of the filter's own set left");

    let since = parse_token(&other.token, state.epoch);
    let again = state.answer(
      Some("all"),
      &members,
      &ids(&["a", "b", "open-1"]),
      vec![json!({"n": 1}), json!({"n": 2}), json!({"n": 3})],
      since,
    );
    assert!(again.rows.is_empty(), "the first page's open chat did not change, so it is not answered again");
  }
}

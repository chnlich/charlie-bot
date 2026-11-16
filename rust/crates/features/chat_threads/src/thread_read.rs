//! The read window that every chat channel's thread read shares: where the window starts, which unread messages it
//! acks, and how many it leaves out.
//!
//! A channel crate pages its own thread through its client and builds its own message views; the window itself is
//! this module's rule, so a channel's read answers the same messages and moves the same watermark.

use std::collections::HashSet;
use std::sync::Arc;

use charliebot_registry::FeatureContext;
use serde_json::{Value, json};

use crate::thread_entry::{ThreadAdapter, ThreadReplyError, ack_messages};

/// The reply field that counts the unread messages the window leaves out.
pub const MORE_UNREAD: &str = "more_unread";

/// The thread listing a read answers from, and the state that picks its window.
pub struct ThreadRead<'a, M> {
  /// The thread's messages, oldest first.
  pub messages: &'a [M],
  /// The ids of the thread's unread messages.
  pub unread_ids: &'a HashSet<String>,
  /// The ids of the messages that carry a pending summon.
  pub pending: &'a HashSet<String>,
  /// The session's watermark before the ack.
  pub watermark: Option<&'a str>,
  /// The most messages the window holds.
  pub limit: usize,
}

impl<M> ThreadRead<'_, M> {
  /// The window's first index, the index one past its last, and the unread count it leaves out. It starts at the
  /// earlier of the oldest unread message and the oldest pending summon's message, and holds the newest `limit`
  /// messages when neither exists.
  fn bounds(&self, message_id: impl Fn(&M) -> &str) -> (usize, usize, usize) {
    let mut starts = Vec::new();
    let unread = |message: &M| self.unread_ids.contains(message_id(message));
    let summoned = |message: &M| self.pending.contains(message_id(message));
    if let Some(first) = self.messages.iter().position(unread) {
      starts.push(first);
    }
    if let Some(first) = self.messages.iter().position(summoned) {
      starts.push(first);
    }
    let (first, last) = match starts.into_iter().min() {
      None => (self.messages.len().saturating_sub(self.limit), self.messages.len()),
      Some(start) => (start, self.messages.len().min(start + self.limit)),
    };
    let unread_in_window = self.messages[first..last].iter().filter(|message| unread(message)).count();
    (first, last, self.unread_ids.len() - unread_in_window)
  }
}

/// The slice of the thread's messages that a read answers with, the unread it leaves out, and the watermark the ack
/// left.
pub struct ReadWindow {
  /// The index of the window's first message.
  pub first: usize,
  /// The index one past the window's last message.
  pub last: usize,
  /// The unread messages outside the window.
  pub more_unread: usize,
  /// The session's watermark after the window's unread messages are acked, and the watermark as it stood when the
  /// window holds no unread message.
  pub watermark: Value,
}

/// Pick the window and ack its unread messages. It answers the window's bounds as indices into `read.messages`, so the
/// caller builds its own views from that slice.
pub async fn read_window<A, M>(
  adapter: &Arc<A>,
  features: &Arc<FeatureContext>,
  session_id: &str,
  read: ThreadRead<'_, M>,
  message_id: impl Fn(&M) -> &str,
) -> Result<ReadWindow, ThreadReplyError>
where
  A: ThreadAdapter + 'static,
{
  let (first, last, more_unread) = read.bounds(&message_id);
  let unread_in_window: Vec<String> = read.messages[first..last]
    .iter()
    .filter(|message| read.unread_ids.contains(message_id(message)))
    .map(|message| message_id(message).to_string())
    .collect();
  let mut watermark = json!(read.watermark);
  if !unread_in_window.is_empty() {
    let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();
    let ack = ack_messages(&dyn_adapter, features, session_id, &unread_in_window).await?;
    watermark = ack[adapter.platform().watermark_key().as_str()].clone();
  }
  Ok(ReadWindow { first, last, more_unread, watermark })
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;

  use super::ThreadRead;

  struct Message {
    id: &'static str,
  }

  fn ids(names: &[&str]) -> HashSet<String> {
    names.iter().map(|name| name.to_string()).collect()
  }

  fn bounds(message_names: &[&'static str], unread: &[&str], pending: &[&str], limit: usize) -> (usize, usize, usize) {
    let messages: Vec<Message> = message_names.iter().map(|id| Message { id }).collect();
    let unread_ids = ids(unread);
    let pending_ids = ids(pending);
    let read =
      ThreadRead { messages: &messages, unread_ids: &unread_ids, pending: &pending_ids, watermark: None, limit };
    read.bounds(|message| message.id)
  }

  #[test]
  fn the_window_starts_at_the_oldest_unread_message() {
    assert_eq!(bounds(&["a", "b", "c", "d", "e"], &["c", "d", "e"], &[], 2), (2, 4, 1));
  }

  #[test]
  fn a_pending_summon_older_than_the_oldest_unread_opens_the_window() {
    assert_eq!(bounds(&["a", "b", "c", "d", "e"], &["d"], &["b"], 2), (1, 3, 1));
  }

  #[test]
  fn the_newest_limit_messages_answer_when_nothing_is_unread_or_pending() {
    assert_eq!(bounds(&["a", "b", "c", "d", "e"], &[], &[], 2), (3, 5, 0));
  }

  #[test]
  fn a_limit_past_the_listing_holds_every_message() {
    assert_eq!(bounds(&["a", "b"], &["a"], &[], 10), (0, 2, 0));
  }
}

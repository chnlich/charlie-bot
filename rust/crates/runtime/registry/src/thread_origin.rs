//! The thread-origin fields: the session metadata fields that name the chat thread a session answers.
//!
//! A chat platform package registers its origin field once, beside its metadata slot
//! (`Registry::thread_origin_field`). The thread-session test reads this one list, so the Threads view, the
//! web-session gates, the close wait and the web client's default view all agree on which sessions are chat threads.

use std::sync::Mutex;

use charliebot_store::metadata::SessionMetadata;

static THREAD_ORIGIN_FIELDS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Record `field` as a thread-origin field. Recording a field again changes nothing.
pub fn register_origin_field(field: &'static str) {
  let mut fields = THREAD_ORIGIN_FIELDS.lock().expect("the thread-origin field list is not poisoned");
  if !fields.contains(&field) {
    fields.push(field);
  }
}

/// True when `meta` names a chat thread: a registered origin field is present and not null.
pub fn is_thread_session(meta: &SessionMetadata) -> bool {
  let fields = THREAD_ORIGIN_FIELDS.lock().expect("the thread-origin field list is not poisoned");
  fields.iter().any(|field| meta.extra(field).is_some_and(|origin| !origin.is_null()))
}

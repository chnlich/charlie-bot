use std::collections::{HashMap, HashSet};

use charliebot_core::event_types::{self, EventFields};
use serde_json::{Value, json};

use crate::{StoreError, StoreResult};

const ATTACHED_FILES_MARKER: &str = "\n\n[Attached files]\n";
const DEFAULT_ATTACHED_FILES_PROMPT: &str = "The user attached these files to this message. Before you answer, open each file.\nWhen the message has no other text, read the user's intent from the current conversation.";

/// One normalized user message and its structured uploaded files.
#[derive(Clone, Debug, PartialEq)]
pub struct NormalizedUserMessage {
  /// User-visible text after any legacy attachment footer is removed.
  pub content: Value,
  /// Uploaded-file records in source order.
  pub uploaded_files: Vec<Value>,
}

/// Normalize a persisted user event for the chat renderer.
pub fn normalize_user_message_event<E: EventFields>(event: &E) -> StoreResult<NormalizedUserMessage> {
  let files = serialize_uploaded_files(event.field("uploaded_files"))?;
  if !files.is_empty() {
    let content = event.field("content").map_or_else(|| json!(""), E::to_owned_value);
    return Ok(NormalizedUserMessage { content, uploaded_files: files });
  }
  let content_text = match event.field("content") {
    Some(content) => content.as_str_field().ok_or_else(|| StoreError::new("user event content must be a string"))?,
    None => "",
  };
  let (content, uploaded_files) = strip_attached_files_block(content_text);
  Ok(NormalizedUserMessage { content: json!(content), uploaded_files })
}

/// Convert supported upload payloads into JSON objects.
pub fn serialize_uploaded_files<E: EventFields>(files: Option<&E>) -> StoreResult<Vec<Value>> {
  let Some(files) = files else { return Ok(Vec::new()) };
  if !event_types::is_truthy(files) {
    return Ok(Vec::new());
  }
  let files = files.array_items().ok_or_else(|| StoreError::new("uploaded_files must be an array"))?;
  files
    .iter()
    .map(|file| {
      if let Some(path) = file.as_str_field() {
        return Ok(json!({"filename": filename_from_path(path), "path": path}));
      }
      if file.is_object() {
        return Ok(file.to_owned_value());
      }
      Err(StoreError::new(format!("unsupported uploaded file payload: {}", file.to_owned_value())))
    })
    .collect()
}

/// Split the final legacy attachment footer from user-visible text.
pub fn strip_attached_files_block(content: &str) -> (String, Vec<Value>) {
  let Some((body, attachments)) = content.rsplit_once(ATTACHED_FILES_MARKER) else {
    return (content.to_string(), Vec::new());
  };
  let mut files = Vec::new();
  for line in attachments.lines() {
    let Some(path) = line.strip_prefix("- ") else { return (content.to_string(), Vec::new()) };
    let path = path.trim();
    if path.is_empty() {
      return (content.to_string(), Vec::new());
    }
    files.push(json!({"filename": filename_from_path(path), "path": path}));
  }
  (body.to_string(), files)
}

/// Add the attachment instruction block to the text sent to the agent.
pub fn build_agent_input_content(content: &str, uploaded_files: &[Value]) -> String {
  let paths: Vec<&str> = uploaded_files
    .iter()
    .filter_map(|file| file.get("path").and_then(Value::as_str))
    .map(str::trim)
    .filter(|path| !path.is_empty())
    .collect();
  if paths.is_empty() {
    return content.to_string();
  }
  let mut block = String::from(DEFAULT_ATTACHED_FILES_PROMPT);
  for path in paths {
    block.push_str("\n- ");
    block.push_str(path);
  }
  if content.trim().is_empty() {
    format!("[Attached files]\n{block}")
  } else {
    format!("{content}{ATTACHED_FILES_MARKER}{block}")
  }
}

/// Move ordinary user inputs to the end of each completed attached-run interval. Each event is returned by
/// reference with its source index, so the projection reads the cached events in place.
pub fn stable_history_projection<'a, E: EventFields>(events: &[&'a E]) -> StoreResult<Vec<(usize, &'a E)>> {
  let (complete_intervals, _) = interval_scan(events.iter().copied());
  let mut deferred_indices = HashSet::new();
  let mut deferred_by_end: HashMap<usize, Vec<(usize, &'a E)>> = HashMap::new();
  for (start, end) in complete_intervals {
    let mut deferred = Vec::new();
    for (index, event) in events.iter().copied().enumerate().take(end).skip(start + 1) {
      if event.field("type").and_then(EventFields::as_str_field) != Some(event_types::USER) {
        continue;
      }
      if event.field("message").is_some() && event.field("content").is_none() {
        continue;
      }
      let normalized = normalize_user_message_event(event)?;
      let content =
        normalized.content.as_str().ok_or_else(|| StoreError::new("user event content must be a string"))?;
      if content.starts_with('/') {
        continue;
      }
      deferred_indices.insert(index);
      deferred.push((index, event));
    }
    if !deferred.is_empty() {
      deferred_by_end.insert(end, deferred);
    }
  }
  let mut projected = Vec::new();
  for (index, event) in events.iter().copied().enumerate() {
    if !deferred_indices.contains(&index) {
      projected.push((index, event));
    }
    if let Some(deferred) = deferred_by_end.get(&index) {
      projected.extend(deferred.iter().cloned());
    }
  }
  Ok(projected)
}

/// Return the length of the event prefix whose projected order cannot change after an append.
pub fn stable_closed_prefix_len<'a, E: EventFields + 'a>(events: impl IntoIterator<Item = &'a E>) -> usize {
  interval_scan(events).1
}

fn interval_scan<'a, E: EventFields + 'a>(events: impl IntoIterator<Item = &'a E>) -> (Vec<(usize, usize)>, usize) {
  let mut complete = Vec::new();
  let mut interval_start = None;
  let mut closed = 0;
  for (index, event) in events.into_iter().enumerate() {
    let type_value = event.field("type");
    let event_type = type_value.and_then(EventFields::as_str_field);
    let has_session_id = event.field("session_id").is_some_and(event_types::is_truthy);
    let no_type = type_value.is_none_or(E::is_null);
    if (no_type || event_type == Some(event_types::SESSION_ATTACHED)) && has_session_id {
      interval_start = Some(index);
    } else if event_type == Some(event_types::MASTER_DONE)
      && let Some(start) = interval_start.take()
    {
      complete.push((start, index));
    }
    if interval_start.is_none() {
      closed = index + 1;
    }
  }
  (complete, closed)
}

fn filename_from_path(path: &str) -> String {
  let normalized = path.replace('\\', "/");
  normalized.trim_end_matches('/').rsplit('/').next().filter(|name| !name.is_empty()).unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn structured_uploads_override_the_legacy_footer() {
    let event = json!({
      "type": "user",
      "content": "body\n\n[Attached files]\n- /old/report.txt",
      "uploaded_files": ["/new/data.csv"]
    });
    let normalized = normalize_user_message_event(&event).unwrap();
    assert_eq!(normalized.content, event["content"]);
    assert_eq!(normalized.uploaded_files, vec![json!({"filename": "data.csv", "path": "/new/data.csv"})]);
  }

  #[test]
  fn falsey_upload_values_match_empty_python_upload_lists() {
    for value in [json!(null), json!(false), json!(0), json!(""), json!([]), json!({})] {
      assert!(serialize_uploaded_files(Some(&value)).unwrap().is_empty());
    }
  }

  #[test]
  fn malformed_legacy_footer_stays_in_user_text() {
    let content = "body\n\n[Attached files]\n- /tmp/a\nnot a path";
    assert_eq!(strip_attached_files_block(content), (content.to_string(), Vec::new()));
  }

  #[test]
  fn attachment_paths_use_the_last_path_component() {
    assert_eq!(filename_from_path(r"C:\work\report.csv"), "report.csv");
    assert_eq!(filename_from_path("/work/"), "work");
  }

  #[test]
  fn an_open_run_keeps_the_projection_boundary_before_the_attach() {
    let events = vec![
      json!({"type": "user", "content": "before"}),
      json!({"type": "session_attached", "session_id": "native"}),
      json!({"type": "user", "content": "during"}),
    ];
    assert_eq!(stable_closed_prefix_len(&events), 1);
  }

  #[test]
  fn attachment_block_follows_the_text_per_upload_path() {
    let first = "/sessions/node/uploads/one.png";
    let second = "/sessions/node/uploads/two.png";
    let uploads = [json!({"path": first}), json!({"path": second})];
    assert_eq!(
      build_agent_input_content("look at these", &uploads),
      format!("look at these{ATTACHED_FILES_MARKER}{DEFAULT_ATTACHED_FILES_PROMPT}\n- {first}\n- {second}")
    );
  }

  #[test]
  fn upload_only_input_gets_the_block_alone() {
    let path = "/sessions/node/uploads/shot.png";
    assert_eq!(
      build_agent_input_content("", &[json!({"path": path})]),
      format!("[Attached files]\n{DEFAULT_ATTACHED_FILES_PROMPT}\n- {path}")
    );
  }
}

//! Candidate extraction for the taste extractor (`charliebot taste candidates`).
//!
//! A candidate is one piece of the user's own text that may hold a choice precedent. Four sources produce them:
//! artifact and diff comment batches, plan version notes, replies that follow a thumbs-down round rating, and every
//! other user message. Only `user` events produce chat candidates; `agent_message` events carry another session's
//! agent and never become one.
//!
//! Each candidate carries the latest web reply before it, because that reply states what the user was asked and what
//! was recommended. A plan note also carries the predecessor page's Trade-off lines, which name the options.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_core::event_types;
use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_store::chat_events::{self, LineRule};
use charliebot_store::{layout, metadata};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::SourceRef;

/// The comment-batch prefixes a user event starts with.
const COMMENT_PREFIXES: [&str; 2] = ["[Artifact comments", "[Diff comments"];

/// One source of candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateSource {
  Comments,
  Notes,
  DownvoteReplies,
  Chat,
}

impl CandidateSource {
  /// The value `--sources` takes for this source.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Comments => "comments",
      Self::Notes => "notes",
      Self::DownvoteReplies => "downvote-replies",
      Self::Chat => "chat",
    }
  }

  /// The source that one `--sources` value names.
  pub fn parse(value: &str) -> Option<Self> {
    match value.trim() {
      "comments" => Some(Self::Comments),
      "notes" => Some(Self::Notes),
      "downvote-replies" => Some(Self::DownvoteReplies),
      "chat" => Some(Self::Chat),
      _ => None,
    }
  }

  /// Every source, the default of `--sources`.
  pub fn all() -> BTreeSet<Self> {
    [Self::Comments, Self::Notes, Self::DownvoteReplies, Self::Chat].into_iter().collect()
  }
}

/// One extracted candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
  pub source: CandidateSource,
  pub text: String,
  pub previous_web_reply: Option<String>,
  pub session_id: String,
  pub reference: SourceRef,
  pub timestamp: String,
  /// A note's predecessor Trade-off lines; `None` when the predecessor page is gone. Other sources carry none.
  pub trade_off_options: Option<Vec<String>>,
}

impl Candidate {
  /// The session's link, the form the chat UI opens a session with.
  pub fn url(&self) -> String {
    session_url(&self.session_id)
  }

  /// The candidate's JSONL object.
  pub fn to_value(&self) -> Value {
    let mut value = Map::new();
    value.insert("source".to_string(), json!(self.source.as_str()));
    value.insert("text".to_string(), json!(self.text));
    value.insert("previous_web_reply".to_string(), json!(self.previous_web_reply));
    value.insert("session_id".to_string(), json!(self.session_id));
    match &self.reference {
      SourceRef::Event(event_id) => {
        value.insert("event_id".to_string(), json!(event_id));
      }
      SourceRef::PlanVersion(plan_version) => {
        value.insert("plan_version".to_string(), json!(plan_version));
      }
    }
    value.insert("timestamp".to_string(), json!(self.timestamp));
    value.insert("url".to_string(), json!(self.url()));
    if self.source == CandidateSource::Notes {
      value.insert("trade_off_options".to_string(), json!(self.trade_off_options));
    }
    Value::Object(value)
  }
}

/// The link the chat UI opens a session with.
pub fn session_url(session_id: &str) -> String {
  format!("/?session={session_id}")
}

/// One user message of a session's chat log.
struct UserMessage {
  index: usize,
  event_id: String,
  timestamp: OffsetDateTime,
  text: String,
  previous_web_reply: Option<String>,
}

/// One thumbs-down rating's event, by its position in the chat log.
#[derive(Clone)]
struct Rating {
  event_id: String,
  index: usize,
  thumbs_down: bool,
}

/// The candidates of every manager session under `sessions_root`, at or after `since`, from `sources`.
pub fn collect<F>(
  sessions_root: &Path,
  since: OffsetDateTime,
  sources: &BTreeSet<CandidateSource>,
  read_plans: &F,
) -> anyhow::Result<Vec<Candidate>>
where
  F: Fn(&Path) -> anyhow::Result<Option<Vec<Value>>>,
{
  let entries = match std::fs::read_dir(sessions_root) {
    Ok(entries) => entries,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(error).with_context(|| format!("list {}", sessions_root.display())),
  };
  let mut session_dirs: Vec<PathBuf> = entries
    .map(|entry| entry.map(|entry| entry.path()).map_err(|error| anyhow::anyhow!(error)))
    .collect::<anyhow::Result<Vec<PathBuf>>>()?;
  session_dirs.sort();
  let mut candidates = Vec::new();
  for session_dir in session_dirs {
    if !session_dir.is_dir() {
      continue;
    }
    let Some(session_id) = session_dir.file_name().map(|name| name.to_string_lossy().into_owned()) else {
      continue;
    };
    if !is_manager_session(&session_dir)? {
      continue;
    }
    let ratings = round_ratings(&session_dir)?;
    let scan = scan_chat_log(&session_dir, &ratings)?;
    candidates.extend(session_candidates(&session_id, &session_dir, since, sources, &scan, read_plans)?);
  }
  candidates.sort_by(|left, right| {
    left
      .timestamp
      .cmp(&right.timestamp)
      .then_with(|| left.session_id.cmp(&right.session_id))
      .then_with(|| reference_key(&left.reference).cmp(&reference_key(&right.reference)))
  });
  Ok(candidates)
}

fn reference_key(reference: &SourceRef) -> String {
  match reference {
    SourceRef::Event(event_id) => format!("event:{event_id}"),
    SourceRef::PlanVersion(plan_version) => format!("plan:{plan_version}"),
  }
}

/// True when the session's metadata names the manager profile. A missing metadata file names no session profile.
fn is_manager_session(session_dir: &Path) -> anyhow::Result<bool> {
  let path = session_dir.join(METADATA_FILE_NAME);
  if !path.is_file() {
    return Ok(false);
  }
  let value = metadata::read_metadata_value(&path).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
  Ok(value.get("profile").and_then(Value::as_str) == Some("manager"))
}

/// The session's round ratings: each rated event id with its thumbs-down flag.
fn round_ratings(session_dir: &Path) -> anyhow::Result<Vec<Rating>> {
  let path = session_dir.join(METADATA_FILE_NAME);
  if !path.is_file() {
    return Ok(Vec::new());
  }
  let value = metadata::read_metadata_value(&path).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
  let Some(ratings) = value.get(metadata::ROUND_RATINGS).and_then(Value::as_object) else {
    return Ok(Vec::new());
  };
  Ok(
    ratings
      .iter()
      .map(|(event_id, rating)| Rating {
        event_id: event_id.clone(),
        index: usize::MAX,
        thumbs_down: rating.as_str() == Some(metadata::THUMBS_DOWN),
      })
      .collect(),
  )
}

/// What one chat-log scan keeps: the user messages, the web replies, and each rating's event position.
struct ChatScan {
  users: Vec<UserMessage>,
  web_replies: Vec<(OffsetDateTime, String)>,
  ratings: Vec<Rating>,
}

/// The scan of one session's chat log, in file order. A line that is not JSON is skipped, as
/// `chat_events::find_in_log` skips it: an append can tear a line. A JSON value that is not an object fails the
/// scan. A real user message, as `event_types::is_real_user_message` selects it, and a web reply need the fields
/// a candidate carries, or the scan fails with the line's location. A tool-result echo is not a real user message
/// and is skipped.
fn scan_chat_log(session_dir: &Path, ratings: &[Rating]) -> anyhow::Result<ChatScan> {
  let path = layout::chat_events_path(session_dir);
  let lines = match chat_events::read_lines(&path, LineRule::Lines) {
    Ok(lines) => lines,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      return Ok(ChatScan { users: Vec::new(), web_replies: Vec::new(), ratings: Vec::new() });
    }
    Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
  };
  let rated: BTreeMap<&str, usize> =
    ratings.iter().enumerate().map(|(position, rating)| (rating.event_id.as_str(), position)).collect();
  let mut scan = ChatScan {
    users: Vec::new(),
    web_replies: Vec::new(),
    ratings: ratings.iter().map(|rating| Rating { index: usize::MAX, ..rating.clone() }).collect(),
  };
  let mut previous_web_reply: Option<String> = None;
  for (index, line) in lines.enumerate() {
    let line = line.with_context(|| format!("read {}:{}", path.display(), index + 1))?;
    let Ok(event) = line.decode() else { continue };
    let location = format!("{}:{}", path.display(), index + 1);
    anyhow::ensure!(event.is_object(), "{location}: a chat event is not a JSON object");
    let event_type = event.get("type").and_then(Value::as_str);
    if event_type == Some(event_types::WEB_REPLY) {
      let text = required_str(&event, "content", &location)?;
      previous_web_reply = Some(text.to_string());
      scan.web_replies.push((event_time(&event, &location)?, text.to_string()));
      continue;
    }
    if let Some(event_id) = event.get("id").and_then(Value::as_str)
      && let Some(&position) = rated.get(event_id)
    {
      scan.ratings[position].index = index;
    }
    if !event_types::is_real_user_message(&event) {
      continue;
    }
    scan.users.push(UserMessage {
      index,
      event_id: required_str(&event, "id", &location)?.to_string(),
      timestamp: event_time(&event, &location)?,
      text: required_str(&event, "content", &location)?.to_string(),
      previous_web_reply: previous_web_reply.clone(),
    });
  }
  Ok(scan)
}

/// The string `field` of a chat event. A missing or non-string field fails the scan, naming the line.
fn required_str<'a>(event: &'a Value, field: &str, location: &str) -> anyhow::Result<&'a str> {
  event
    .get(field)
    .and_then(Value::as_str)
    .with_context(|| format!("{location}: a chat event carries no string `{field}`"))
}

/// The event's timestamp in RFC 3339. A missing or malformed timestamp fails the scan, naming the line.
fn event_time(event: &Value, location: &str) -> anyhow::Result<OffsetDateTime> {
  let text = required_str(event, "timestamp", location)?;
  OffsetDateTime::parse(text, &Rfc3339).with_context(|| format!("{location}: the timestamp {text:?} is not RFC 3339"))
}

/// One session's user messages and plan notes, classified into candidates.
fn session_candidates<F>(
  session_id: &str,
  session_dir: &Path,
  since: OffsetDateTime,
  sources: &BTreeSet<CandidateSource>,
  scan: &ChatScan,
  read_plans: &F,
) -> anyhow::Result<Vec<Candidate>>
where
  F: Fn(&Path) -> anyhow::Result<Option<Vec<Value>>>,
{
  let downvote_replies: BTreeSet<&str> = scan
    .ratings
    .iter()
    .filter(|rating| rating.thumbs_down && rating.index != usize::MAX)
    .filter_map(|rating| scan.users.iter().find(|user| user.index > rating.index))
    .map(|user| user.event_id.as_str())
    .collect();
  let mut candidates = Vec::new();
  for user in &scan.users {
    if user.timestamp < since {
      continue;
    }
    let is_comment = COMMENT_PREFIXES.iter().any(|prefix| user.text.starts_with(prefix));
    let source = if is_comment {
      CandidateSource::Comments
    } else if downvote_replies.contains(user.event_id.as_str()) {
      CandidateSource::DownvoteReplies
    } else {
      CandidateSource::Chat
    };
    if !sources.contains(&source) {
      continue;
    }
    candidates.push(Candidate {
      source,
      text: user.text.clone(),
      previous_web_reply: user.previous_web_reply.clone(),
      session_id: session_id.to_string(),
      reference: SourceRef::Event(user.event_id.clone()),
      timestamp: user.timestamp.format(&Rfc3339).context("format a candidate timestamp")?,
      trade_off_options: None,
    });
  }
  if sources.contains(&CandidateSource::Notes) {
    candidates.extend(note_candidates(session_id, session_dir, since, &scan.web_replies, read_plans)?);
  }
  Ok(candidates)
}

/// One plan-version note as a candidate, with the predecessor page's Trade-off lines.
fn note_candidates<F>(
  session_id: &str,
  session_dir: &Path,
  since: OffsetDateTime,
  web_replies: &[(OffsetDateTime, String)],
  read_plans: &F,
) -> anyhow::Result<Vec<Candidate>>
where
  F: Fn(&Path) -> anyhow::Result<Option<Vec<Value>>>,
{
  let Some(plans) = read_plans(session_dir)? else { return Ok(Vec::new()) };
  let mut candidates = Vec::new();
  for plan in &plans {
    let plan_id = plan_id(plan).with_context(|| format!("{}: a plan carries no id", session_dir.display()))?;
    let versions = plan
      .get("versions")
      .and_then(Value::as_array)
      .with_context(|| format!("{}: plan {plan_id} carries no versions", session_dir.display()))?;
    for version in versions {
      // A version without a note has nothing for the extractor to read.
      let Some(note) = version.get("note").and_then(Value::as_str).filter(|note| !note.trim().is_empty()) else {
        continue;
      };
      let number = version
        .get("v")
        .and_then(Value::as_u64)
        .with_context(|| format!("plan {plan_id}: a version with a note carries no number"))?;
      let created_at = version
        .get("created_at")
        .and_then(Value::as_str)
        .with_context(|| format!("plan {plan_id} v{number}: no created_at"))?;
      let decided_at = OffsetDateTime::parse(created_at, &Rfc3339)
        .map_err(|error| anyhow::anyhow!("plan {plan_id} v{number}: created_at: {error}"))?;
      if decided_at < since {
        continue;
      }
      // The first version has no predecessor page, so it names no options. A later version whose predecessor record is
      // gone has unknown options, the same as a predecessor page that is gone.
      let trade_off_options = if number == 1 {
        Some(Vec::new())
      } else {
        let previous = number.checked_sub(1).and_then(|previous| {
          versions.iter().find(|candidate| candidate.get("v").and_then(Value::as_u64) == Some(previous))
        });
        match previous {
          Some(previous) => predecessor_options(session_dir, previous)?,
          None => None,
        }
      };
      candidates.push(Candidate {
        source: CandidateSource::Notes,
        text: note.to_string(),
        previous_web_reply: latest_reply_before(web_replies, decided_at),
        session_id: session_id.to_string(),
        reference: SourceRef::PlanVersion(format!("{plan_id}/v{number}")),
        timestamp: decided_at.format(&Rfc3339).context("format a plan version timestamp")?,
        trade_off_options,
      });
    }
  }
  Ok(candidates)
}

fn plan_id(plan: &Value) -> Option<String> {
  match plan.get("id")? {
    Value::Number(number) => Some(number.to_string()),
    Value::String(text) if !text.is_empty() => Some(text.clone()),
    _ => None,
  }
}

/// The predecessor page's Trade-off lines. `None` when the page is no longer in the session directory.
fn predecessor_options(session_dir: &Path, version: &Value) -> anyhow::Result<Option<Vec<String>>> {
  let relative = version
    .get("file")
    .and_then(Value::as_str)
    .with_context(|| format!("{}: a plan version names no file", session_dir.display()))?;
  let path = session_dir.join(relative);
  match std::fs::read_to_string(&path) {
    Ok(html) => Ok(Some(trade_off_lines(&html))),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
  }
}

fn latest_reply_before(web_replies: &[(OffsetDateTime, String)], moment: OffsetDateTime) -> Option<String> {
  web_replies
    .iter()
    .filter(|(timestamp, _)| *timestamp < moment)
    .max_by_key(|(timestamp, _)| *timestamp)
    .map(|(_, text)| text.clone())
}

/// The Trade-offs section's option lines: each question, recommendation, trade-off and list item, in page order.
pub fn trade_off_lines(html: &str) -> Vec<String> {
  let Some(section) = trade_offs_section(html) else { return Vec::new() };
  let mut lines = Vec::new();
  let mut cursor = 0;
  while let Some(block) = next_option_block(section, cursor) {
    cursor = block.end;
    let text = clean_text(block.inner);
    if !text.is_empty() {
      lines.push(text);
    }
  }
  lines
}

fn trade_offs_section(html: &str) -> Option<&str> {
  // ASCII case folding keeps `lowered` offsets equal to `html` offsets; every marker searched for is ASCII.
  let lowered = html.to_ascii_lowercase();
  let heading = lowered.find("trade-offs")?;
  let heading_start = lowered[..heading].rfind("<h2")?;
  let body_start = lowered[heading_start..].find("</h2>")? + heading_start + "</h2>".len();
  let body_end = lowered[body_start..].find("<h2").map_or(html.len(), |offset| body_start + offset);
  Some(&html[body_start..body_end])
}

struct OptionBlock<'a> {
  inner: &'a str,
  end: usize,
}

/// The next `<p class="q|rec|trade">` or `<li>` block in `section` at or after `cursor`.
fn next_option_block(section: &str, cursor: usize) -> Option<OptionBlock<'_>> {
  let openers: [(&str, &str); 4] =
    [("<p class=\"q\"", "</p>"), ("<p class=\"rec\"", "</p>"), ("<p class=\"trade\"", "</p>"), ("<li", "</li>")];
  let mut found: Option<(usize, &str, &str)> = None;
  for (opener, closer) in openers {
    if let Some(offset) = section[cursor..].find(opener) {
      let start = cursor + offset;
      if found.is_none_or(|(best, _, _)| start < best) {
        found = Some((start, opener, closer));
      }
    }
  }
  let (start, _opener, closer) = found?;
  let tag_end = start + section[start..].find('>')? + 1;
  let close = tag_end + section[tag_end..].find(closer)?;
  Some(OptionBlock { inner: &section[tag_end..close], end: close + closer.len() })
}

/// The visible text of one HTML fragment: tags removed, entities decoded, runs of whitespace collapsed.
fn clean_text(fragment: &str) -> String {
  let mut visible = String::with_capacity(fragment.len());
  let mut in_tag = false;
  for character in fragment.chars() {
    match character {
      '<' => in_tag = true,
      '>' if in_tag => in_tag = false,
      _ if !in_tag => visible.push(character),
      _ => {}
    }
  }
  charliebot_html::unescape(&visible).split_whitespace().collect::<Vec<&str>>().join(" ")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn trade_off_lines_read_the_section_and_skip_other_sections() {
    let html = "<h2><span class=\"n\">4</span> Design</h2><div><li>not an option</li></div>\
                <h2><span class=\"n\">5</span> Trade-offs</h2>\
                <div class=\"fork\"><p class=\"q\"><span class=\"fn\">1</span>Ship the MVP?</p>\
                <ul><li>「MVP only」: smaller</li><li>「All of it」: larger</li></ul>\
                <p class=\"rec\"><b>Recommend:</b> MVP</p></div>\
                <h2>Other Details</h2><li>ignored</li>";
    assert_eq!(
      trade_off_lines(html),
      ["1Ship the MVP?", "「MVP only」: smaller", "「All of it」: larger", "Recommend: MVP",]
    );
  }

  #[test]
  fn trade_off_lines_keep_their_offsets_past_a_character_that_lowercases_to_more_bytes() {
    // `İ` lowercases to three bytes, so a search over the lowered page would point one byte past the page's own
    // `</h2>` and split the Chinese character that follows it.
    let html =
      "<p>İ</p><h2><span class=\"n\">5</span> Trade-offs</h2>结论<div><p class=\"q\">范围？</p></div><h2>其他</h2>";
    assert_eq!(trade_off_lines(html), ["范围？"]);
  }
}

#[cfg(test)]
mod fixture_tests {
  use std::fs;

  use tempfile::TempDir;

  use super::*;

  const SESSION: &str = "sess-aaa";

  /// The plan registry for a fixture session, read straight from the file the artifacts package owns.
  fn read_plans(session_dir: &Path) -> anyhow::Result<Option<Vec<Value>>> {
    let path = session_dir.join("plans.json");
    if !path.is_file() {
      return Ok(None);
    }
    let document: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    Ok(document.get("plans").and_then(Value::as_array).cloned())
  }

  /// A manager session with one web reply, one comment batch, one thumbs-down reply, two chat messages and a plan
  /// note whose predecessor page names its options.
  fn fixture() -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    let session_dir = sessions.join(SESSION);
    fs::create_dir_all(session_dir.join("data")).unwrap();
    fs::create_dir_all(session_dir.join("artifacts")).unwrap();
    fs::write(session_dir.join("metadata.json"), r#"{"profile": "manager", "round_ratings": {"m1": "thumbs_down"}}"#)
      .unwrap();
    let events = [
      json!({"type": "web_reply", "id": "w1", "content": "Earlier reply", "timestamp": "2026-10-15T10:00:00+00:00"}),
      json!({"type": "user", "id": "u1", "content": "ordinary message", "timestamp": "2026-10-16T10:00:00+00:00"}),
      json!({"type": "user", "id": "c1", "content": "[Artifact comments · /p.html] (1)\n\n1. pick one", "timestamp": "2026-10-16T11:00:00+00:00"}),
      json!({"type": "master_done", "id": "m1", "timestamp": "2026-10-16T12:00:00+00:00"}),
      json!({"type": "agent_message", "id": "a1", "content": "agent text", "timestamp": "2026-10-16T12:30:00+00:00"}),
      json!({"type": "user", "id": "u2", "content": "reply after a thumbs down", "timestamp": "2026-10-16T13:00:00+00:00"}),
      json!({"type": "user", "id": "u3", "content": "later chat", "timestamp": "2026-10-16T14:00:00+00:00"}),
    ];
    let body: String = events.iter().map(|event| format!("{event}\n")).collect();
    fs::write(session_dir.join("data/chat_events.jsonl"), body).unwrap();
    fs::write(
      session_dir.join("plans.json"),
      json!({"plans": [{"id": 1, "versions": [
        {"v": 1, "file": "artifacts/plan_01.html", "created_at": "2026-10-15T09:00:00+00:00"},
        {"v": 2, "file": "artifacts/plan_01_v2.html", "created_at": "2026-10-16T15:00:00+00:00", "note": "the user picked the MVP"}
      ]}]})
      .to_string(),
    )
    .unwrap();
    fs::write(
      session_dir.join("artifacts/plan_01.html"),
      "<h2><span class=\"n\">5</span> Trade-offs</h2><div class=\"fork\"><p class=\"q\">Scope?</p>\
       <ul><li>「MVP only」: smaller</li><li>「All of it」: larger</li></ul></div><h2>Other</h2>",
    )
    .unwrap();
    (temp, sessions)
  }

  fn since() -> OffsetDateTime {
    OffsetDateTime::parse("2026-10-16T00:00:00+00:00", &Rfc3339).unwrap()
  }

  #[test]
  fn each_source_yields_its_candidates_and_agent_messages_stay_out() {
    let (_temp, sessions) = fixture();
    let candidates = collect(&sessions, since(), &CandidateSource::all(), &read_plans).unwrap();
    let by_source: BTreeMap<&str, Vec<&Candidate>> = candidates.iter().fold(BTreeMap::new(), |mut map, candidate| {
      map.entry(candidate.source.as_str()).or_default().push(candidate);
      map
    });

    let comments = &by_source["comments"];
    assert_eq!(comments.len(), 1);
    assert!(comments[0].text.starts_with("[Artifact comments"));
    assert_eq!(comments[0].previous_web_reply.as_deref(), Some("Earlier reply"));

    let downvotes = &by_source["downvote-replies"];
    assert_eq!(downvotes.len(), 1);
    assert_eq!(downvotes[0].text, "reply after a thumbs down");
    assert_eq!(downvotes[0].reference, SourceRef::Event("u2".into()));

    let chat = &by_source["chat"];
    let texts: Vec<&str> = chat.iter().map(|candidate| candidate.text.as_str()).collect();
    assert_eq!(texts, ["ordinary message", "later chat"]);
    assert!(!candidates.iter().any(|candidate| candidate.text == "agent text"));

    let notes = &by_source["notes"];
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].text, "the user picked the MVP");
    assert_eq!(notes[0].reference, SourceRef::PlanVersion("1/v2".into()));
    assert_eq!(notes[0].url(), format!("/?session={SESSION}"));
    assert_eq!(notes[0].previous_web_reply.as_deref(), Some("Earlier reply"));
    let options = notes[0].trade_off_options.as_ref().unwrap();
    assert!(options.iter().any(|line| line.contains("「MVP only」")), "{options:?}");
    assert!(options.iter().any(|line| line.contains("「All of it」")), "{options:?}");
  }

  #[test]
  fn since_and_sources_filter_the_output() {
    let (_temp, sessions) = fixture();
    let later = OffsetDateTime::parse("2026-10-16T13:30:00+00:00", &Rfc3339).unwrap();
    let candidates = collect(&sessions, later, &CandidateSource::all(), &read_plans).unwrap();
    let texts: Vec<&str> = candidates.iter().map(|candidate| candidate.text.as_str()).collect();
    assert_eq!(texts, ["later chat", "the user picked the MVP"]);

    let comments: BTreeSet<CandidateSource> = [CandidateSource::Comments].into_iter().collect();
    let candidates = collect(&sessions, since(), &comments, &read_plans).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].source, CandidateSource::Comments);
  }

  #[test]
  fn a_note_whose_predecessor_page_is_gone_reports_its_options_as_unknown() {
    let (_temp, sessions) = fixture();
    fs::remove_file(sessions.join(SESSION).join("artifacts/plan_01.html")).unwrap();
    let notes: BTreeSet<CandidateSource> = [CandidateSource::Notes].into_iter().collect();
    let candidates = collect(&sessions, since(), &notes, &read_plans).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].trade_off_options, None);
    assert!(candidates[0].to_value()["trade_off_options"].is_null());
  }

  #[test]
  fn a_user_event_without_an_id_fails_the_scan_and_names_its_line() {
    let (_temp, sessions) = fixture();
    let log = sessions.join(SESSION).join("data/chat_events.jsonl");
    let mut body = fs::read_to_string(&log).unwrap();
    body.push_str("{\"type\": \"user\", \"content\": \"no id\", \"timestamp\": \"2026-10-16T16:00:00+00:00\"}\n");
    fs::write(&log, body).unwrap();
    let error = collect(&sessions, since(), &CandidateSource::all(), &read_plans).expect_err("the scan fails");
    let message = format!("{error:#}");
    assert!(message.contains("chat_events.jsonl:8"), "{message}");
    assert!(message.contains("`id`"), "{message}");
  }

  #[test]
  fn a_tool_result_echo_between_user_messages_changes_nothing() {
    let (_temp, sessions) = fixture();
    let before = collect(&sessions, since(), &CandidateSource::all(), &read_plans).unwrap();
    let log = sessions.join(SESSION).join("data/chat_events.jsonl");
    let mut lines: Vec<String> = fs::read_to_string(&log).unwrap().lines().map(str::to_string).collect();
    // The backend's echo of a tool result: `type` is `user`, but the text sits under `message`, not `content`.
    let echo = json!({
      "type": "user",
      "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]},
      "session_id": SESSION,
      "uuid": "echo-1",
      "timestamp": "2026-10-16T10:30:00+00:00",
      "tool_use_result": {"stdout": "ok"},
      "id": "echo-1"
    });
    lines.insert(2, echo.to_string());
    fs::write(&log, lines.iter().map(|line| format!("{line}\n")).collect::<String>()).unwrap();
    let after = collect(&sessions, since(), &CandidateSource::all(), &read_plans).unwrap();
    assert_eq!(after, before);
  }
}

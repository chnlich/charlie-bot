//! Duplicate-comment guard for `charliebot linear request`: refuses a `commentCreate` mutation whose
//! `(issueId, body)` matches one already posted through this profile home within the dedup window.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use charliebot_cli_common::exit::CliExit;
use charliebot_core::fsutil::atomic_write_text;
use charliebot_core::home::state_dir;
use charliebot_store::control_events::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const LOCK_FILENAME: &str = "linear_recent_comments.lock";
const LEDGER_FILENAME: &str = "linear_recent_comments.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommentCreateTarget {
  pub response_field: String,
  pub issue_id: String,
  pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct RecentComment {
  pub key: String,
  pub issue_id: String,
  pub posted_at_ms: u64,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub comment_id: Option<String>,
}

/// Exclusive guard held across the duplicate check, HTTP send, and ledger write for `commentCreate` requests.
pub(super) struct PendingCommentGuard {
  _lock: File,
  ledger_path: PathBuf,
  entries: Vec<RecentComment>,
  targets: Vec<(String, CommentCreateTarget)>,
}

impl PendingCommentGuard {
  /// Acquires the home-scoped lock and refuses if `request_value` duplicates a recent `commentCreate`.
  ///
  /// Returns `Ok(None)` when the GraphQL request contains no `commentCreate` target.
  pub(super) fn acquire(
    request_value: &Value,
    home: &Path,
    dedup_window: Duration,
  ) -> Result<Option<PendingCommentGuard>, CliExit> {
    let targets = extract_comment_creates(request_value);
    if targets.is_empty() {
      return Ok(None);
    }

    let mut seen_in_request = HashSet::new();
    let mut keyed_targets = Vec::with_capacity(targets.len());
    for target in targets {
      let key = comment_key(&target.issue_id, &target.body);
      if !seen_in_request.insert(key.clone()) {
        return Err(duplicate_comment_refusal(&target.issue_id, None));
      }
      keyed_targets.push((key, target));
    }

    let dir = state_dir(home);
    std::fs::create_dir_all(&dir).map_err(|error| CliExit::error(format!("could not create state dir: {error}")))?;
    let lock_file = OpenOptions::new()
      .create(true)
      .read(true)
      .write(true)
      .truncate(false)
      .open(dir.join(LOCK_FILENAME))
      .map_err(|error| CliExit::error(format!("could not open Linear comment lock: {error}")))?;
    lock_file.lock().map_err(|error| CliExit::error(format!("could not lock Linear comment ledger: {error}")))?;

    let ledger_path = dir.join(LEDGER_FILENAME);
    let mut entries = load_ledger(&ledger_path)?;
    let now_ms = unix_millis()?;
    let window_ms = u64::try_from(dedup_window.as_millis()).unwrap_or(u64::MAX);
    entries.retain(|entry| now_ms.saturating_sub(entry.posted_at_ms) < window_ms);

    for (key, target) in &keyed_targets {
      if let Some(existing) = entries.iter().find(|entry| &entry.key == key) {
        return Err(duplicate_comment_refusal(&target.issue_id, Some(existing)));
      }
    }

    Ok(Some(PendingCommentGuard { _lock: lock_file, ledger_path, entries, targets: keyed_targets }))
  }

  /// Records any `commentCreate` field whose response object did not report `success: false`.
  pub(super) fn record_created(mut self, response_value: &Value) -> Result<(), CliExit> {
    let Some(data) = response_value.get("data").and_then(Value::as_object) else {
      return Ok(());
    };
    let now_ms = unix_millis()?;
    let mut added = false;
    for (key, target) in &self.targets {
      let Some(field_obj) = data.get(&target.response_field).and_then(Value::as_object) else {
        continue;
      };
      if field_obj.get("success").and_then(Value::as_bool) == Some(false) {
        continue;
      }
      let comment_id =
        field_obj.get("comment").and_then(|comment| comment.get("id")).and_then(Value::as_str).map(str::to_string);
      self.entries.push(RecentComment {
        key: key.clone(),
        issue_id: target.issue_id.clone(),
        posted_at_ms: now_ms,
        comment_id,
      });
      added = true;
    }
    if added {
      let serialized = serde_json::to_string(&self.entries)
        .map_err(|error| CliExit::error(format!("could not serialize Linear comment ledger: {error}")))?;
      atomic_write_text(&self.ledger_path, &format!("{serialized}\n"))
        .map_err(|error| CliExit::error(format!("could not write Linear comment ledger: {error}")))?;
    }
    Ok(())
  }
}

fn comment_key(issue_id: &str, body: &str) -> String {
  sha256_hex(&format!("{issue_id}\0{body}"))
}

fn unix_millis() -> Result<u64, CliExit> {
  let duration = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_err(|error| CliExit::error(format!("system clock precedes Unix epoch: {error}")))?;
  Ok(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

fn load_ledger(path: &Path) -> Result<Vec<RecentComment>, CliExit> {
  match std::fs::read_to_string(path) {
    Ok(raw) => {
      serde_json::from_str(&raw).map_err(|error| CliExit::error(format!("could not parse {}: {error}", path.display())))
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
    Err(error) => Err(CliExit::error(format!("could not read {}: {error}", path.display()))),
  }
}

fn duplicate_comment_refusal(issue_id: &str, existing: Option<&RecentComment>) -> CliExit {
  let mut body = json!({
    "error": "duplicate commentCreate refused: an identical comment body was already posted to this issue recently",
    "issueId": issue_id,
  });
  if let Some(entry) = existing {
    body["postedAtMs"] = json!(entry.posted_at_ms);
    if let Some(comment_id) = &entry.comment_id {
      body["commentId"] = json!(comment_id);
    }
  }
  CliExit::contract(body, 1)
}

/// Extracts all resolved `(response_field, issueId, body)` targets for `commentCreate` calls in `request_value`.
pub(super) fn extract_comment_creates(request_value: &Value) -> Vec<CommentCreateTarget> {
  let Some(query) = request_value.get("query").and_then(Value::as_str) else {
    return Vec::new();
  };
  if !query.contains("commentCreate") {
    return Vec::new();
  }
  let empty = Map::new();
  let variables = request_value.get("variables").and_then(Value::as_object).unwrap_or(&empty);
  let mut cursor = GraphqlCursor::new(query);
  let mut targets = Vec::new();
  let mut prev_name: Option<String> = None;
  let mut prev_was_colon = false;
  let mut prev_was_at = false;

  while let Some(token) = cursor.next_token() {
    match token {
      Token::Name(name) if name == "commentCreate" && !prev_was_at && cursor.peek_is_lparen() => {
        let response_field = if prev_was_colon {
          prev_name.clone().unwrap_or_else(|| "commentCreate".to_string())
        } else {
          "commentCreate".to_string()
        };
        if let Some((issue_id, body)) = cursor.parse_comment_create_args(variables) {
          targets.push(CommentCreateTarget { response_field, issue_id, body });
        }
        prev_name = None;
        prev_was_colon = false;
        prev_was_at = false;
      }
      Token::Name(name) => {
        prev_name = Some(name);
        prev_was_colon = false;
        prev_was_at = false;
      }
      Token::Colon => {
        prev_was_colon = true;
        prev_was_at = false;
      }
      Token::At => {
        prev_name = None;
        prev_was_colon = false;
        prev_was_at = true;
      }
      _ => {
        prev_name = None;
        prev_was_colon = false;
        prev_was_at = false;
      }
    }
  }
  targets
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
  Name(String),
  Var(String),
  Str(String),
  LParen,
  RParen,
  LBrace,
  RBrace,
  LBracket,
  RBracket,
  Colon,
  At,
  Other,
}

struct GraphqlCursor<'a> {
  src: &'a str,
  pos: usize,
}

impl<'a> GraphqlCursor<'a> {
  fn new(src: &'a str) -> Self {
    Self { src, pos: 0 }
  }

  fn rest(&self) -> &'a str {
    &self.src[self.pos..]
  }

  fn skip_ignored(&mut self) {
    loop {
      let bytes = self.src.as_bytes();
      while self.pos < bytes.len() && matches!(bytes[self.pos], b' ' | b'\t' | b'\r' | b'\n' | b',') {
        self.pos += 1;
      }
      if self.pos < bytes.len() && bytes[self.pos] == b'#' {
        while self.pos < bytes.len() && bytes[self.pos] != b'\n' {
          self.pos += 1;
        }
        continue;
      }
      break;
    }
  }

  fn peek_is_lparen(&mut self) -> bool {
    self.skip_ignored();
    self.src.as_bytes().get(self.pos) == Some(&b'(')
  }

  fn next_token(&mut self) -> Option<Token> {
    self.skip_ignored();
    let byte = *self.src.as_bytes().get(self.pos)?;
    if self.rest().starts_with("\"\"\"") {
      return Some(Token::Str(self.read_block_string()?));
    }
    match byte {
      b'"' => Some(Token::Str(self.read_string()?)),
      b'$' => {
        self.pos += 1;
        let name = self.read_name_str()?;
        Some(Token::Var(name))
      }
      b'(' => {
        self.pos += 1;
        Some(Token::LParen)
      }
      b')' => {
        self.pos += 1;
        Some(Token::RParen)
      }
      b'{' => {
        self.pos += 1;
        Some(Token::LBrace)
      }
      b'}' => {
        self.pos += 1;
        Some(Token::RBrace)
      }
      b'[' => {
        self.pos += 1;
        Some(Token::LBracket)
      }
      b']' => {
        self.pos += 1;
        Some(Token::RBracket)
      }
      b':' => {
        self.pos += 1;
        Some(Token::Colon)
      }
      b'@' => {
        self.pos += 1;
        Some(Token::At)
      }
      b'_' | b'a'..=b'z' | b'A'..=b'Z' => Some(Token::Name(self.read_name_str()?)),
      _ => {
        let ch = self.rest().chars().next()?;
        self.pos += ch.len_utf8();
        Some(Token::Other)
      }
    }
  }

  fn read_name_str(&mut self) -> Option<String> {
    let start = self.pos;
    let bytes = self.src.as_bytes();
    while self.pos < bytes.len() && matches!(bytes[self.pos], b'_' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9') {
      self.pos += 1;
    }
    (self.pos > start).then(|| self.src[start..self.pos].to_string())
  }

  fn read_string(&mut self) -> Option<String> {
    self.pos += 1;
    let mut out = String::new();
    while let Some(ch) = self.rest().chars().next() {
      self.pos += ch.len_utf8();
      match ch {
        '"' => return Some(out),
        '\\' => {
          let escaped = self.rest().chars().next()?;
          self.pos += escaped.len_utf8();
          match escaped {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => out.push(self.read_unicode_escape()?),
            other => out.push(other),
          }
        }
        other => out.push(other),
      }
    }
    None
  }

  fn read_unicode_escape(&mut self) -> Option<char> {
    if self.rest().starts_with('{') {
      self.pos += 1;
      let end = self.rest().find('}')?;
      let code = u32::from_str_radix(&self.rest()[..end], 16).ok()?;
      self.pos += end + 1;
      return char::from_u32(code);
    }
    let hex = self.rest().get(..4)?;
    let code = u16::from_str_radix(hex, 16).ok()?;
    self.pos += 4;
    if (0xD800..=0xDBFF).contains(&code) && self.rest().starts_with("\\u") {
      let saved = self.pos;
      self.pos += 2;
      if let Some(low_hex) = self.rest().get(..4)
        && let Ok(low) = u16::from_str_radix(low_hex, 16)
        && (0xDC00..=0xDFFF).contains(&low)
      {
        self.pos += 4;
        let combined = 0x10000 + (((u32::from(code) - 0xD800) << 10) | (u32::from(low) - 0xDC00));
        return char::from_u32(combined);
      }
      self.pos = saved;
    }
    char::from_u32(u32::from(code))
  }

  fn read_block_string(&mut self) -> Option<String> {
    self.pos += 3;
    let mut raw = String::new();
    while !self.rest().is_empty() {
      if self.rest().starts_with("\\\"\"\"") {
        raw.push_str("\"\"\"");
        self.pos += 4;
        continue;
      }
      if self.rest().starts_with("\"\"\"") {
        self.pos += 3;
        return Some(dedent_block_string(&raw));
      }
      let ch = self.rest().chars().next()?;
      raw.push(ch);
      self.pos += ch.len_utf8();
    }
    None
  }

  fn parse_comment_create_args(&mut self, variables: &Map<String, Value>) -> Option<(String, String)> {
    if self.next_token()? != Token::LParen {
      return None;
    }
    let mut issue_id = None;
    let mut body = None;
    while let Some(token) = self.next_token() {
      match token {
        Token::RParen => break,
        Token::Name(arg_name) => {
          if self.next_token() != Some(Token::Colon) {
            return None;
          }
          if arg_name == "input" {
            match self.next_token()? {
              Token::Var(var_name) => {
                if let Some(obj) = variables.get(&var_name).and_then(Value::as_object) {
                  issue_id = obj.get("issueId").and_then(Value::as_str).map(str::to_string);
                  body = obj.get("body").and_then(Value::as_str).map(str::to_string);
                }
              }
              Token::LBrace => {
                let (id, text) = self.parse_input_object(variables)?;
                issue_id = id;
                body = text;
              }
              other => self.skip_after(other),
            }
          } else {
            let val = self.next_token()?;
            self.skip_after(val);
          }
        }
        other => self.skip_after(other),
      }
    }
    Some((issue_id?, body?))
  }

  fn parse_input_object(&mut self, variables: &Map<String, Value>) -> Option<(Option<String>, Option<String>)> {
    let mut issue_id = None;
    let mut body = None;
    while let Some(token) = self.next_token() {
      match token {
        Token::RBrace => return Some((issue_id, body)),
        Token::Name(field_name) => {
          if self.next_token() != Some(Token::Colon) {
            return None;
          }
          let val = self.next_token()?;
          match field_name.as_str() {
            "issueId" => issue_id = self.resolve_string_token(val, variables),
            "body" => body = self.resolve_string_token(val, variables),
            _ => self.skip_after(val),
          }
        }
        other => self.skip_after(other),
      }
    }
    None
  }

  fn resolve_string_token(&mut self, token: Token, variables: &Map<String, Value>) -> Option<String> {
    match token {
      Token::Str(text) => Some(text),
      Token::Var(var_name) => variables.get(&var_name).and_then(Value::as_str).map(str::to_string),
      other => {
        self.skip_after(other);
        None
      }
    }
  }

  fn skip_after(&mut self, first: Token) {
    let close = match first {
      Token::LBrace => Token::RBrace,
      Token::LParen => Token::RParen,
      Token::LBracket => Token::RBracket,
      _ => return,
    };
    while let Some(token) = self.next_token() {
      if token == close {
        return;
      }
      self.skip_after(token);
    }
  }
}

/// Applies the GraphQL block-string indentation stripping algorithm.
fn dedent_block_string(raw: &str) -> String {
  let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
  let lines: Vec<&str> = normalized.split('\n').collect();
  let mut common_indent: Option<usize> = None;
  for line in lines.iter().skip(1) {
    let indent = line.bytes().take_while(|byte| matches!(byte, b' ' | b'\t')).count();
    if indent < line.len() {
      common_indent = Some(common_indent.map_or(indent, |cur| cur.min(indent)));
    }
  }
  let indent = common_indent.unwrap_or(0);
  let mut formatted: Vec<&str> = Vec::with_capacity(lines.len());
  if let Some((first, rest)) = lines.split_first() {
    formatted.push(first);
    for line in rest {
      formatted.push(if line.len() >= indent { &line[indent..] } else { line });
    }
  }
  while formatted.first().is_some_and(|line| line.trim().is_empty()) {
    formatted.remove(0);
  }
  while formatted.last().is_some_and(|line| line.trim().is_empty()) {
    formatted.pop();
  }
  formatted.join("\n")
}

//! The managed instruction snapshot: ordered blocks, the prompt hash, and the measured length (`PromptBlock`,
//! `PromptSnapshot` and `assemble_snapshot` in `src/runtime/task_prompts.py:116-202` and `:403-433`).

use std::collections::HashMap;

use charliebot_core::text_repr::{JsonStyle, float_repr, json_dumps, str_repr};
use charliebot_store::control_events::sha256_hex;
use serde_json::{Map, Value, json};

use super::{PromptSource, RuleSegment, TaskPromptError};

/// `json.dumps({"blocks": ...}, sort_keys=True, ensure_ascii=False)`: the canonical text the prompt hash covers.
const HASH_STYLE: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: true, item_separator: ", ", key_separator: ": ", indent: None };

/// One ordered managed block: its origins, the SHA-256 of its text, its delivery, and the text itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptBlock {
  pub sources: Vec<PromptSource>,
  pub body_ref: String,
  pub delivery: String,
  pub text: String,
}

/// The durable startup snapshot of one run: the blocks in injection order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptSnapshot {
  pub blocks: Vec<PromptBlock>,
}

impl PromptSnapshot {
  /// The joined instruction text the backend receives, blocks separated by a blank line.
  pub fn instructions_text(&self) -> String {
    self.blocks.iter().map(|block| block.text.as_str()).collect::<Vec<_>>().join("\n\n")
  }

  /// The code-point length of [`Self::instructions_text`].
  pub fn char_count(&self) -> usize {
    self.instructions_text().chars().count()
  }

  /// The SHA-256 of the canonical JSON of the blocks: their text, sources, delivery and body refs.
  pub fn prompt_hash(&self) -> String {
    let payload = json!({"blocks": self.blocks.iter().map(block_value).collect::<Vec<Value>>()});
    sha256_hex(&json_dumps(&payload, &HASH_STYLE))
  }

  /// The persisted form, with keys `blocks`, `prompt_hash` and `char_count` in that order.
  pub fn to_json_dict(&self) -> Value {
    json!({
      "blocks": self.blocks.iter().map(block_value).collect::<Vec<Value>>(),
      "prompt_hash": self.prompt_hash(),
      "char_count": self.char_count(),
    })
  }

  /// Rebuilds a stored snapshot. A malformed file fails, and so does one whose recorded hash, length or body ref
  /// does not match its content.
  pub fn from_json_dict(data: &Value) -> Result<Self, TaskPromptError> {
    let blocks = parse_blocks(data)
      .map_err(|reason| TaskPromptError::new(format!("malformed stored prompt snapshot: {reason}")))?;
    let snapshot = Self { blocks };
    let computed_hash = snapshot.prompt_hash();
    let stored_hash = data.get("prompt_hash");
    if stored_hash.and_then(Value::as_str) != Some(computed_hash.as_str()) {
      return Err(TaskPromptError::new(format!(
        "stored prompt snapshot hash mismatch: recorded {}, content hashes to {}",
        value_repr(stored_hash),
        str_repr(&computed_hash)
      )));
    }
    let measured = snapshot.char_count();
    let stored_count = data.get("char_count");
    if !stored_count.is_some_and(|count| equals_count(count, measured)) {
      return Err(TaskPromptError::new(format!(
        "stored prompt snapshot char_count mismatch: recorded {}, content measures {measured}",
        value_repr(stored_count)
      )));
    }
    for block in &snapshot.blocks {
      let actual = sha256_hex(&block.text);
      if block.body_ref != actual {
        return Err(TaskPromptError::new(format!(
          "stored prompt snapshot body_ref mismatch: block hashes to {actual}, recorded {}",
          block.body_ref
        )));
      }
    }
    Ok(snapshot)
  }
}

fn source_value(source: &PromptSource) -> Value {
  json!({
    "scope": source.scope,
    "source_ref": source.source_ref,
    "source_session_id": source.source_session_id,
  })
}

fn block_value(block: &PromptBlock) -> Value {
  json!({
    "sources": block.sources.iter().map(source_value).collect::<Vec<Value>>(),
    "body_ref": block.body_ref,
    "delivery": block.delivery,
    "text": block.text,
  })
}

/// Turns the ordered rule segments into a snapshot. Identical text dedups at its first position, and every source of
/// each repeat merges there in first-seen order. Only byte-identical text dedups; similar rules stay apart.
pub fn assemble_snapshot(segments: &[RuleSegment]) -> PromptSnapshot {
  let mut blocks: Vec<PromptBlock> = Vec::new();
  let mut by_text: HashMap<&str, usize> = HashMap::new();
  for segment in segments {
    if let Some(&index) = by_text.get(segment.text.as_str()) {
      let existing = &blocks[index];
      let mut sources = existing.sources.clone();
      for source in &segment.sources {
        if !sources.contains(source) {
          sources.push(source.clone());
        }
      }
      let merged = PromptBlock {
        sources,
        body_ref: existing.body_ref.clone(),
        delivery: existing.delivery.clone(),
        text: existing.text.clone(),
      };
      blocks[index] = merged;
      continue;
    }
    let mut sources: Vec<PromptSource> = Vec::new();
    for source in &segment.sources {
      if !sources.contains(source) {
        sources.push(source.clone());
      }
    }
    blocks.push(PromptBlock {
      sources,
      body_ref: sha256_hex(&segment.text),
      delivery: segment.delivery.clone(),
      text: segment.text.clone(),
    });
    by_text.insert(segment.text.as_str(), blocks.len() - 1);
  }
  PromptSnapshot { blocks }
}

/// Parses the stored blocks. A missing key reads as Python's `KeyError` text, and a value of the wrong shape names
/// its type.
fn parse_blocks(data: &Value) -> Result<Vec<PromptBlock>, String> {
  let blocks = subscript(data, "blocks")?;
  let items = blocks.as_array().ok_or_else(|| not_iterable(blocks))?;
  items.iter().map(parse_block).collect()
}

fn parse_block(value: &Value) -> Result<PromptBlock, String> {
  let sources = subscript(value, "sources")?;
  let sources = sources
    .as_array()
    .ok_or_else(|| not_iterable(sources))?
    .iter()
    .map(parse_source)
    .collect::<Result<Vec<PromptSource>, String>>()?;
  let body_ref = string_field(value, "body_ref")?;
  let delivery = string_field(value, "delivery")?;
  let text = string_field(value, "text")?;
  Ok(PromptBlock { sources, body_ref, delivery, text })
}

/// One source, built the way `PromptSource(**s)` builds it: `scope` and `source_ref` are required, and
/// `source_session_id` defaults to None.
fn parse_source(value: &Value) -> Result<PromptSource, String> {
  let members: &Map<String, Value> = value
    .as_object()
    .ok_or_else(|| format!("PromptSource.__init__() argument after ** must be a mapping, not {}", type_name(value)))?;
  if let Some(unexpected) =
    members.keys().find(|key| !matches!(key.as_str(), "scope" | "source_ref" | "source_session_id"))
  {
    return Err(format!("PromptSource.__init__() got an unexpected keyword argument {}", str_repr(unexpected)));
  }
  let missing: Vec<&str> = ["scope", "source_ref"].into_iter().filter(|key| !members.contains_key(*key)).collect();
  match missing.as_slice() {
    [] => {}
    [only] => return Err(format!("PromptSource.__init__() missing 1 required positional argument: '{only}'")),
    [first, second] => {
      return Err(format!("PromptSource.__init__() missing 2 required positional arguments: '{first}' and '{second}'"));
    }
    _ => unreachable!("two required keys"),
  }
  let session = match members.get("source_session_id") {
    None | Some(Value::Null) => None,
    Some(Value::String(session)) => Some(session.clone()),
    Some(other) => return Err(format!("source_session_id must be a string, not {}", type_name(other))),
  };
  Ok(PromptSource {
    scope: string_field(value, "scope")?,
    source_ref: string_field(value, "source_ref")?,
    source_session_id: session,
  })
}

/// `value[key]` as Python subscripts a stored JSON value: a dict by key, and the other types' own TypeError text.
fn subscript<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
  match value {
    Value::Object(members) => members.get(key).ok_or_else(|| format!("'{key}'")),
    Value::Array(_) => Err("list indices must be integers or slices, not str".to_string()),
    Value::String(_) => Err("string indices must be integers, not 'str'".to_string()),
    other => Err(format!("'{}' object is not subscriptable", type_name(other))),
  }
}

fn string_field(value: &Value, key: &str) -> Result<String, String> {
  match subscript(value, key)? {
    Value::String(text) => Ok(text.clone()),
    other => Err(format!("{key} must be a string, not {}", type_name(other))),
  }
}

fn not_iterable(value: &Value) -> String {
  format!("'{}' object is not iterable", type_name(value))
}

/// The Python type name of a stored JSON value.
fn type_name(value: &Value) -> &'static str {
  match value {
    Value::Null => "NoneType",
    Value::Bool(_) => "bool",
    Value::Number(number) if number.is_f64() => "float",
    Value::Number(_) => "int",
    Value::String(_) => "str",
    Value::Array(_) => "list",
    Value::Object(_) => "dict",
  }
}

/// `repr()` of a stored JSON value, as the mismatch messages print a recorded value. A missing key reads as `None`.
fn value_repr(value: Option<&Value>) -> String {
  match value {
    None | Some(Value::Null) => "None".to_string(),
    Some(Value::Bool(flag)) => if *flag { "True" } else { "False" }.to_string(),
    Some(Value::Number(number)) if number.is_f64() => float_repr(number.as_f64().unwrap_or_default()),
    Some(Value::Number(number)) => number.to_string(),
    Some(Value::String(text)) => str_repr(text),
    Some(Value::Array(items)) => {
      format!("[{}]", items.iter().map(|item| value_repr(Some(item))).collect::<Vec<_>>().join(", "))
    }
    Some(Value::Object(members)) => format!(
      "{{{}}}",
      members
        .iter()
        .map(|(key, item)| format!("{}: {}", str_repr(key), value_repr(Some(item))))
        .collect::<Vec<_>>()
        .join(", ")
    ),
  }
}

/// Python's `recorded != measured` for a stored count: an int, a float equal to it, or True/False as 1/0.
fn equals_count(value: &Value, measured: usize) -> bool {
  match value {
    Value::Number(number) if number.is_f64() => number.as_f64() == Some(measured as f64),
    Value::Number(number) => number.as_u64() == Some(measured as u64),
    Value::Bool(flag) => usize::from(*flag) == measured,
    _ => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::prompts::{DELIVERY_FULL, SCOPE_BASE, SCOPE_SUBTREE};

  fn source(scope: &str, source_ref: &str, owner: Option<&str>) -> PromptSource {
    PromptSource {
      scope: scope.to_string(),
      source_ref: source_ref.to_string(),
      source_session_id: owner.map(String::from),
    }
  }

  fn segment(text: &str, sources: Vec<PromptSource>) -> RuleSegment {
    RuleSegment { text: text.to_string(), sources, delivery: DELIVERY_FULL.to_string() }
  }

  #[test]
  fn identical_text_dedups_at_its_first_position_with_every_source() {
    let segments = [
      segment("shared", vec![source("base", "prompts/a.md", None)]),
      segment("other", vec![source("base", "prompts/b.md", None)]),
      segment(
        "shared",
        vec![source("subtree", "prompt_bodies/x.md", Some("root")), source("base", "prompts/a.md", None)],
      ),
    ];
    let snapshot = assemble_snapshot(&segments);
    let texts: Vec<&str> = snapshot.blocks.iter().map(|block| block.text.as_str()).collect();
    assert_eq!(texts, ["shared", "other"]);
    assert_eq!(
      snapshot.blocks[0].sources,
      [source("base", "prompts/a.md", None), source("subtree", "prompt_bodies/x.md", Some("root"))]
    );
  }

  #[test]
  fn hash_and_length_measure_code_points_and_the_joined_text() {
    let snapshot = assemble_snapshot(&[
      segment("回答", vec![source("base", "prompts/a.md", None)]),
      segment("é", vec![source("base", "prompts/b.md", None)]),
    ]);
    assert_eq!(snapshot.char_count(), "回答\n\né".chars().count());
    let rebuilt = PromptSnapshot::from_json_dict(&snapshot.to_json_dict()).expect("a fresh snapshot round-trips");
    assert_eq!(rebuilt, snapshot);
  }

  /// The hash is the SHA-256 of the exact canonical bytes and every stored snapshot rechecks it, so the canonical
  /// text is pinned here byte for byte. The expected values are CPython's
  /// `json.dumps(payload, sort_keys=True, ensure_ascii=False)` (`src/runtime/task_prompts.py:153-154`), which leaves
  /// non-ASCII and DEL raw, escapes the control set, and sorts keys at every level.
  #[test]
  fn prompt_hash_pins_the_canonical_bytes() {
    let snapshot = assemble_snapshot(&[
      segment("Manage the greeter. — ✅\n\nSecond line\u{7f}", vec![source(SCOPE_BASE, "prompts/a.md", None)]),
      segment(
        "回答",
        vec![source(SCOPE_SUBTREE, "prompt_bodies/x.md", Some("root")), source(SCOPE_BASE, "prompts/b.md", None)],
      ),
    ]);
    assert_eq!(
      json_dumps(&snapshot.to_json_dict(), &HASH_STYLE),
      concat!(
        r#"{"blocks": [{"body_ref": "b86e9a446933130a90bce54bb0c257590530f0c85af6f636f2563fecce5853ef", "#,
        r#""delivery": "full", "sources": [{"scope": "base", "source_ref": "prompts/a.md", "#,
        r#""source_session_id": null}], "text": "Manage the greeter. — ✅\n\nSecond line"#,
        "\u{7f}",
        r#""}, "#,
        r#"{"body_ref": "f3f512648742020fbd700fa8f0576babd8f7ec8e56d3dcd38e28be5b99287db3", "#,
        r#""delivery": "full", "sources": [{"scope": "subtree", "source_ref": "prompt_bodies/x.md", "#,
        r#""source_session_id": "root"}, {"scope": "base", "source_ref": "prompts/b.md", "#,
        r#""source_session_id": null}], "text": "回答"}], "char_count": 41, "prompt_hash": "#,
        r#""0184dbde7b1ef7ea17f64a8f3f9e3f2bc79666f3e9c4d212622cab146b6165ad"}"#,
      )
    );
    assert_eq!(snapshot.prompt_hash(), "0184dbde7b1ef7ea17f64a8f3f9e3f2bc79666f3e9c4d212622cab146b6165ad");
  }
}

//! The differential helper for `tests/differential.py`: one JSON request per stdin line, one JSON answer per stdout
//! line. A `select` request runs one audience's selection on a store, and an `ensure` request runs `ensure_store`. A
//! memory error is answered as `{"error": ...}`; a malformed request exits 1.

use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{Context, bail};
use charliebot_memory::memory::{self, MemorySelection};
use serde_json::{Value, json};

fn main() -> anyhow::Result<()> {
  let mut stdout = io::stdout().lock();
  for line in io::stdin().lock().lines() {
    let request: Value = serde_json::from_str(&line?).context("parse a request line")?;
    writeln!(stdout, "{}", answer(&request)?)?;
  }
  Ok(())
}

fn answer(request: &Value) -> anyhow::Result<Value> {
  match request["op"].as_str() {
    Some("select") => select(request),
    Some("ensure") => ensure(request),
    _ => bail!("unknown request: {request}"),
  }
}

fn select(request: &Value) -> anyhow::Result<Value> {
  let store = Path::new(required_str(request, "store")?);
  let selection = match required_str(request, "audience")? {
    "master" => memory::select_master_memory(store),
    "worker" => memory::select_worker_memory(store, required_str(request, "repo_basename")?).map(Some),
    other => bail!("unknown audience: {other}"),
  };
  Ok(match selection {
    Ok(selection) => json!({"selection": selection.as_ref().map(selection_json)}),
    Err(error) => json!({"error": format!("{error}")}),
  })
}

fn ensure(request: &Value) -> anyhow::Result<Value> {
  let dir = Path::new(required_str(request, "dir")?);
  Ok(match memory::ensure_store(dir) {
    Ok(()) => json!({"ok": true}),
    Err(error) => json!({"error": format!("{error}")}),
  })
}

fn required_str<'a>(request: &'a Value, key: &str) -> anyhow::Result<&'a str> {
  request[key].as_str().with_context(|| format!("request has no string {key:?}: {request}"))
}

/// The selection as the differential compares it: every field of `MemorySelection`, and each segment's sources.
fn selection_json(selection: &MemorySelection) -> Value {
  let segments: Vec<Value> = selection
    .segments
    .iter()
    .map(|segment| {
      let sources: Vec<Value> = segment
        .sources
        .iter()
        .map(|source| json!({"source_ref": source.source_ref, "delivery": source.delivery, "text": source.text}))
        .collect();
      json!({"delivery": segment.delivery, "text": segment.text, "sources": sources})
    })
    .collect();
  json!({
    "audience": selection.audience,
    "repo_basename": selection.repo_basename,
    "segments": segments,
    "usage_line": selection.usage_line,
    "text": selection.text,
  })
}

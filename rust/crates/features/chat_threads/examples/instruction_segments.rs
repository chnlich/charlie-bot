//! The Rust side of `tests/differential.py`: one JSON request per stdin line, one JSON answer per stdout line.
//!
//! A request names a synthetic checkout, a session and a run kind. The answer holds, for each of the web_reply, slack
//! and discord packages, the segments its registry returns, or the text of the error that the build fails with.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};

use anyhow::{Context, bail};
use charliebot_core::RepoRoot;
use charliebot_core::config::{
  BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_registry::Registry;
use charliebot_registry::turn::RuleSegment;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use serde_json::{Value, json};

/// The three packages whose contributions the differential compares, by answer key.
const PACKAGES: [(&str, &str, charliebot_registry::RegisterFn); 3] = [
  ("web_reply", "src.features.web_reply", charliebot_web_reply::register),
  ("slack", "src.features.slack", charliebot_slack::register),
  ("discord", "src.features.discord", charliebot_discord::register),
];

fn main() -> anyhow::Result<()> {
  let mut stdout = io::stdout().lock();
  for line in io::stdin().lock().lines() {
    let request: Value = serde_json::from_str(&line?).context("parse a request line")?;
    writeln!(stdout, "{}", answer(&request)?)?;
  }
  Ok(())
}

fn answer(request: &Value) -> anyhow::Result<Value> {
  let repo = RepoRoot::new(Some(OsString::from(required_str(request, "repo")?)))?;
  let meta = session(&request["meta"])?;
  let kind = required_str(request, "kind")?;
  let cfg = config(&meta.id);
  let mut answer = serde_json::Map::new();
  for (key, package, register) in PACKAGES {
    let mut registry = Registry::new();
    registry.register_package(package, register)?;
    let result = match registry.instruction_segments(&meta, kind, &cfg, &repo) {
      Ok(segments) => json!({"segments": segments.iter().map(segment_json).collect::<Vec<_>>()}),
      Err(error) => json!({"error": format!("{error}")}),
    };
    answer.insert(key.to_string(), result);
  }
  Ok(Value::Object(answer))
}

/// The session the request describes: its id, name, profile, parent and origin fields.
fn session(meta: &Value) -> anyhow::Result<SessionMetadata> {
  let profile = match required_str(meta, "profile")? {
    "manager" => TaskProfile::Manager,
    "worker" => TaskProfile::Worker,
    other => bail!("unknown profile: {other}"),
  };
  let id = required_str(meta, "id")?;
  let mut session = SessionMetadata::new(id, required_str(meta, "name")?, profile);
  session.parent_session_id = meta["parent_session_id"].as_str().map(str::to_string);
  if let Some(extra) = meta["extra"].as_object() {
    session.extra.extend(extra.clone());
  }
  Ok(session)
}

fn segment_json(segment: &RuleSegment) -> Value {
  json!({
    "text": segment.text,
    "sources": segment.sources.iter().map(|source| json!({
      "scope": source.scope,
      "source_ref": source.source_ref,
      "source_session_id": source.source_session_id,
    })).collect::<Vec<_>>(),
    "delivery": segment.delivery,
  })
}

fn required_str<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
  value[key].as_str().with_context(|| format!("request field {key} must be a string"))
}

/// The config the contributions take. They read no field of it, so a synthetic home is enough.
fn config(session_id: &str) -> CharlieBotConfig {
  CharlieBotConfig {
    charliebot_home: format!("/synthetic/home/{session_id}").into(),
    headless_chrome_bin: "/synthetic/bin/chrome".to_string(),
    server: ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18650,
      subprocess_buffer_limit_mb: 1,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      session_tasks_max: 0,
      min_free_disk_gib: 0,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    sections: Sections::default(),
  }
}

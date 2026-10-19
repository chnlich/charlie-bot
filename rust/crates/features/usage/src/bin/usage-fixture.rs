use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use charliebot_core::config::{self, ConfigSchema};
use charliebot_registry::Registry;
use charliebot_registry::usage::{RecordKind, UsageOps, UsageRecord, UsageSource, UsageSourceImpl};
use charliebot_usage_page::{UsageLedger, capture_usage};
use serde_json::{Value, json};

struct FixtureSource {
  codex_rule: bool,
}

impl UsageSourceImpl for FixtureSource {
  fn ops(&self) -> UsageOps {
    UsageOps { logs: true, read: true, live_cli_sessions: self.codex_rule, ..UsageOps::default() }
  }

  fn logs(&self, _config: &charliebot_core::config::CharlieBotConfig) -> anyhow::Result<Vec<(PathBuf, String)>> {
    Ok(Vec::new())
  }

  fn read(&self, path: &Path, _account: &str, _previous: Option<&str>) -> anyhow::Result<(String, Vec<UsageRecord>)> {
    bail!("fixture source has no log files: {}", path.display())
  }

  fn live_cli_sessions(&self) -> anyhow::Result<HashSet<String>> {
    if !self.codex_rule {
      bail!("fixture source does not declare live_cli_sessions");
    }
    let path = env::var_os("FIXTURE_LIVE_SESSIONS")
      .map(PathBuf::from)
      .ok_or_else(|| anyhow!("FIXTURE_LIVE_SESSIONS is required"))?;
    let content = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(content.lines().map(str::to_string).collect())
  }
}

fn register_fixture_sources(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  registry.usage_source(UsageSource {
    name: "CLC",
    id_prefixes: &["charlie-code-", "clc-"],
    run_logs_only: true,
    implementation: None,
  })?;
  registry.attribute_backend_type("charlie-code", "CLC")?;
  registry.usage_source(UsageSource {
    name: "Codex",
    id_prefixes: &["codex-"],
    run_logs_only: false,
    implementation: Some(Arc::new(FixtureSource { codex_rule: true })),
  })?;
  registry.attribute_backend_type("codex", "Codex")?;
  registry.usage_source(UsageSource {
    name: "Claude Code",
    id_prefixes: &["claude-"],
    run_logs_only: false,
    implementation: Some(Arc::new(FixtureSource { codex_rule: false })),
  })?;
  registry.attribute_backend_type("cc-claude", "Claude Code")?;
  Ok(())
}

fn main() -> anyhow::Result<()> {
  let mut args = env::args_os().skip(1);
  let command = args.next().ok_or_else(|| anyhow!("usage: usage-fixture <open|fill|capture|rows> <path> [ledger]"))?;
  let command = command.to_str().ok_or_else(|| anyhow!("command is not UTF-8"))?;
  let path = args.next().map(PathBuf::from).ok_or_else(|| anyhow!("a path is required"))?;
  match command {
    "open" => UsageLedger::open(path)?.close()?,
    "fill" => fill_ledger(&path)?,
    "capture" => {
      let ledger_path = args.next().map(PathBuf::from).ok_or_else(|| anyhow!("capture requires a ledger path"))?;
      let value = capture(&path, &ledger_path)?;
      println!("{}", serde_json::to_string(&value)?);
    }
    "rows" => {
      let mut ledger = UsageLedger::open(path)?;
      let (rows, starts) = ledger.model_rows_with_native_starts()?;
      let value = rows_value(rows, starts);
      println!("{}", serde_json::to_string(&value)?);
    }
    _ => bail!("unknown command {command:?}"),
  }
  Ok(())
}

fn fill_ledger(path: &Path) -> anyhow::Result<()> {
  let mut ledger = UsageLedger::open(path)?;
  ledger.record_file(
    "fixture-host",
    "/home/fixture/sessions/native/log.jsonl",
    "native-sig",
    &[UsageRecord {
      record_id: "native-call".to_string(),
      kind: RecordKind::Native,
      source: "Codex".to_string(),
      model: "gpt-fixture".to_string(),
      account: "fixture-codex".to_string(),
      ts: "2026-10-18T10:00:00+00:00".to_string(),
      in_fresh: 11,
      cache_write: 0,
      cache_read: 2,
      in_unsplit: 0,
      output: 5,
      sessions: vec!["fixture-session".to_string()],
    }],
  )?;
  ledger.record_file(
    "fixture-host",
    "/home/fixture/sessions/fallback/log.jsonl",
    "fallback-sig",
    &[UsageRecord {
      record_id: "fallback-call".to_string(),
      kind: RecordKind::Fallback,
      source: "Claude Code".to_string(),
      model: "claude-fixture".to_string(),
      account: "fixture-claude".to_string(),
      ts: "2026-10-17T10:00:00+00:00".to_string(),
      in_fresh: 7,
      cache_write: 3,
      cache_read: 1,
      in_unsplit: 0,
      output: 4,
      sessions: vec!["fallback-session".to_string()],
    }],
  )?;
  ledger.mark_capture_finished("fixture-host")?;
  Ok(())
}

fn capture(home: &Path, ledger_path: &Path) -> anyhow::Result<Value> {
  let schema = ConfigSchema { backend_types: vec!["charlie-code", "codex", "cc-claude"], ..ConfigSchema::default() };
  let config = config::load_config(home, &schema)?;
  let mut registry = Registry::new();
  registry.register_package("usage-fixture", register_fixture_sources)?;
  let mut ledger = UsageLedger::open(ledger_path)?;
  let sources = capture_usage(&mut ledger, "fixture-host", &config.sessions_dir(), &config, &registry)?;
  let (rows, starts) = ledger.model_rows_with_native_starts()?;
  Ok(json!({"sources": sources, "rows": rows_value(rows, starts)}))
}

fn rows_value(
  rows: Vec<charliebot_usage_page::LedgerRow>,
  starts: std::collections::BTreeMap<String, String>,
) -> Value {
  let rows = rows
    .into_iter()
    .map(|row| {
      let accounts = row
        .accounts
        .into_iter()
        .map(|account| {
          json!({
            "name": account.name,
            "calls": account.calls,
            "output": Value::from(account.output),
            "total": Value::from(account.total),
          })
        })
        .collect::<Vec<_>>();
      json!({
        "source": row.source,
        "model": row.model,
        "in_fresh": Value::from(row.in_fresh),
        "cache_write": Value::from(row.cache_write),
        "cache_read": Value::from(row.cache_read),
        "in_unsplit": Value::from(row.in_unsplit),
        "output": Value::from(row.output),
        "calls": row.calls,
        "total": Value::from(row.total),
        "first": row.first,
        "last": row.last,
        "fallback_calls": row.fallback_calls,
        "fallback_output": Value::from(row.fallback_output),
        "accounts": accounts,
      })
    })
    .collect::<Vec<_>>();
  json!({"rows": rows, "native_starts": starts})
}

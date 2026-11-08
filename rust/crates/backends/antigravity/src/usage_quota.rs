//! Antigravity's CLI quota account and the shared model-group readings used by Auto.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use charliebot_agents::cli_base::CliHost;
use charliebot_core::config::{BackendOption, CharlieBotConfig, PoolBilling};
use charliebot_registry::backend::{MemberReading, MemberReadingFn};
use charliebot_registry::usage::{
  DEFAULT_ACCOUNT_LABEL, PANEL_FETCHED_AT, PANEL_PROVIDER, PANEL_RESETS_AT, PANEL_SCOPE_LABEL, PANEL_UTILIZATION,
  PANEL_WINDOW_MINUTES, PANEL_WINDOW_NAME, PANEL_WINDOWS, QuotaAccount, UsageOps, UsageSourceImpl,
  mark_expired_windows, member_reading_from_windows,
};
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::process::Command;

const PROVIDER: &str = "antigravity";
const PROVIDER_LABEL: &str = "Antigravity";
type ModelReadings = HashMap<String, Vec<Value>>;
type ParsedUsage = (Map<String, Value>, ModelReadings);

/// The state shared by the panel account and the registered model readers.
#[derive(Default)]
pub struct AntigravityQuotaState {
  current: Mutex<QuotaState>,
}

#[derive(Default)]
struct QuotaState {
  last_error: String,
  configured_models: Vec<String>,
  readings: ModelReadings,
  panel_entry: Option<Map<String, Value>>,
}

/// The CLI account that reads `/usage` without opening a model turn.
pub struct AntigravityQuotaAccount {
  host: CliHost,
  state: Arc<AntigravityQuotaState>,
}

impl AntigravityQuotaAccount {
  pub fn new(host: CliHost, state: Arc<AntigravityQuotaState>) -> Self {
    Self { host, state }
  }

  fn set_models(&self, cfg: &CharlieBotConfig) {
    let mut models: Vec<String> = cfg
      .backends
      .options
      .iter()
      .filter(|option| option.backend_type == "antigravity" && option.billing == Some(PoolBilling::Subscription))
      .map(|option| option.model.clone())
      .collect();
    models.sort();
    models.dedup();
    self.state.current.lock().expect("the Antigravity quota state is not poisoned").configured_models = models;
  }

  async fn fetch_round(&self) -> anyhow::Result<Option<Map<String, Value>>> {
    let models =
      self.state.current.lock().expect("the Antigravity quota state is not poisoned").configured_models.clone();
    if models.is_empty() {
      return Ok(None);
    }
    let result = self.fetch_usage(&models).await;
    match result {
      Ok((entry, readings)) => {
        let mut current = self.state.current.lock().expect("the Antigravity quota state is not poisoned");
        current.panel_entry = Some(entry.clone());
        current.readings = readings;
        current.last_error.clear();
        Ok(Some(entry))
      }
      Err(error) => {
        self.state.current.lock().expect("the Antigravity quota state is not poisoned").last_error =
          format!("{error:#}");
        Err(error)
      }
    }
  }

  async fn fetch_usage(&self, models: &[String]) -> anyhow::Result<ParsedUsage> {
    let agy = self.host.resolve_binary("agy")?;
    let output = Command::new(agy)
      .args(["-p", "/usage", "--output-format", "json"])
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::null())
      .output()
      .await
      .context("running the Antigravity usage command")?;
    if !output.status.success() {
      bail!("Antigravity usage command exited with {}", output.status);
    }
    let response: Value = serde_json::from_slice(&output.stdout).context("parsing the Antigravity usage response")?;
    let (entry, readings) = parse_usage(&response, models, UtcDateTime::now())?;
    Ok((entry, readings))
  }

  fn mark_expired(&self, entry: &Map<String, Value>, now: UtcDateTime) -> anyhow::Result<Map<String, Value>> {
    let sampled = entry.get(PANEL_FETCHED_AT).and_then(Value::as_str).map(UtcDateTime::parse).transpose()?;
    Ok(mark_expired_windows(entry, sampled, now))
  }
}

impl QuotaAccount for AntigravityQuotaAccount {
  fn provider(&self) -> &str {
    PROVIDER
  }

  fn provider_label(&self) -> &str {
    PROVIDER_LABEL
  }

  fn label(&self) -> &str {
    DEFAULT_ACCOUNT_LABEL
  }

  fn last_error(&self) -> String {
    self.state.current.lock().expect("the Antigravity quota state is not poisoned").last_error.clone()
  }

  fn fetch<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Option<Map<String, Value>>>> {
    Box::pin(self.fetch_round())
  }

  fn mark_expired(&self, entry: &Map<String, Value>, now: UtcDateTime) -> anyhow::Result<Map<String, Value>> {
    AntigravityQuotaAccount::mark_expired(self, entry, now)
  }
}

impl AntigravityQuotaState {
  /// The Auto member reader backed only by the last successful `/usage` response.
  pub fn member_reading(state: Arc<Self>) -> MemberReadingFn {
    Arc::new(move |_cfg, option, now| read_member(&state, option, now))
  }
}

/// The source implementation registers the CLI quota account.
pub struct AntigravityUsageSource {
  account: Arc<AntigravityQuotaAccount>,
}

impl AntigravityUsageSource {
  pub fn new(account: Arc<AntigravityQuotaAccount>) -> Self {
    Self { account }
  }
}

impl UsageSourceImpl for AntigravityUsageSource {
  fn ops(&self) -> UsageOps {
    UsageOps { quota_accounts: true, ..UsageOps::default() }
  }

  fn quota_accounts(&self, cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    self.account.set_models(cfg);
    if self
      .account
      .state
      .current
      .lock()
      .expect("the Antigravity quota state is not poisoned")
      .configured_models
      .is_empty()
    {
      return Ok(Vec::new());
    }
    Ok(vec![Arc::clone(&self.account) as Arc<dyn QuotaAccount>])
  }
}

fn read_member(
  state: &AntigravityQuotaState,
  option: &BackendOption,
  now: UtcDateTime,
) -> anyhow::Result<Option<MemberReading>> {
  if option.billing != Some(PoolBilling::Subscription) {
    return Ok(None);
  }
  let current = state.current.lock().expect("the Antigravity quota state is not poisoned");
  let Some(windows) = current.readings.get(&option.model) else { return Ok(None) };
  let sampled = current
    .panel_entry
    .as_ref()
    .and_then(|entry| entry.get(PANEL_FETCHED_AT))
    .and_then(Value::as_str)
    .map(UtcDateTime::parse)
    .transpose()?
    .expect("a successful Antigravity reading carries its sample time");
  member_reading_from_windows(windows, sampled, now)
}

fn parse_usage(response: &Value, models: &[String], fetched_at: UtcDateTime) -> anyhow::Result<ParsedUsage> {
  let status =
    response.get("status").and_then(Value::as_str).ok_or_else(|| anyhow!("Antigravity response has no status"))?;
  if status != "SUCCESS" {
    bail!("Antigravity usage status is {status}");
  }
  let groups = response
    .get("command")
    .and_then(|value| value.get("data"))
    .and_then(|value| value.get("groups"))
    .and_then(Value::as_array)
    .ok_or_else(|| anyhow!("Antigravity response has no command.data.groups array"))?;

  let mut by_model = HashMap::new();
  let mut panel_windows = Vec::new();
  let mut seen_groups = HashSet::new();
  for model in models {
    let mut best_group = None;
    let mut best_score = 0;
    let mut ambiguous = false;
    for group in groups {
      let score = group_match_score(group, model);
      if score > best_score {
        best_group = Some(group);
        best_score = score;
        ambiguous = false;
      } else if score > 0 && score == best_score {
        ambiguous = true;
      }
    }
    if best_score == 0 {
      bail!("Antigravity model {model:?} matched no quota group");
    }
    if ambiguous {
      bail!("Antigravity model {model:?} matched multiple quota groups");
    }
    let group = best_group.expect("a positive group score has a matching group");
    let group_name = group.get("name").and_then(Value::as_str).unwrap_or_default();
    let buckets = group
      .get("buckets")
      .and_then(Value::as_array)
      .ok_or_else(|| anyhow!("Antigravity quota group for model {model:?} has no buckets array"))?;
    let windows = buckets.iter().map(|bucket| parse_bucket(bucket, group_name)).collect::<anyhow::Result<Vec<_>>>()?;
    if windows.is_empty() {
      bail!("Antigravity quota group for model {model:?} has no quota windows");
    }
    let description = group.get("description").and_then(Value::as_str).unwrap_or_default();
    if seen_groups.insert((group_name.to_string(), description.to_string())) {
      panel_windows.extend(windows.iter().cloned());
    }
    by_model.insert(model.clone(), windows);
  }

  let entry = Map::from_iter([
    (PANEL_PROVIDER.to_string(), Value::String(PROVIDER.to_string())),
    (PANEL_FETCHED_AT.to_string(), Value::String(fetched_at.to_str())),
    (PANEL_WINDOWS.to_string(), Value::Array(panel_windows)),
  ]);
  Ok((entry, by_model))
}

fn group_match_score(group: &Value, model: &str) -> usize {
  let model_parts: HashSet<String> = model_tokens(model).into_iter().collect();
  if model_parts.is_empty() {
    return 0;
  }
  let group_text = ["name", "description"]
    .into_iter()
    .filter_map(|key| group.get(key).and_then(Value::as_str))
    .collect::<Vec<_>>()
    .join(" ");
  let group_parts: HashSet<String> = model_tokens(&group_text).into_iter().collect();
  model_parts.intersection(&group_parts).count()
}

fn model_tokens(value: &str) -> Vec<String> {
  value
    .rsplit('/')
    .next()
    .unwrap_or(value)
    .split(|character: char| !character.is_ascii_alphanumeric())
    .filter(|token| !token.is_empty() && !token.bytes().all(|byte| byte.is_ascii_digit()))
    .map(str::to_ascii_lowercase)
    .collect()
}

fn parse_bucket(bucket: &Value, group_name: &str) -> anyhow::Result<Value> {
  let window_name = bucket
    .get("window")
    .and_then(Value::as_str)
    .ok_or_else(|| anyhow!("Antigravity quota bucket has no window name"))?;
  let (label, minutes) = match window_name {
    "5h" => ("5 小时", 300),
    "weekly" => ("每周", 10_080),
    other => bail!("Antigravity quota window {other:?} is not a supported model-group window"),
  };
  let remaining = bucket
    .get("remaining_fraction")
    .and_then(Value::as_f64)
    .ok_or_else(|| anyhow!("Antigravity quota bucket has no numeric remaining_fraction"))?;
  let reset = bucket
    .get("reset_time")
    .and_then(Value::as_str)
    .filter(|value| !value.is_empty())
    .map(UtcDateTime::parse)
    .transpose()?;
  let mut window = Map::from_iter([
    (PANEL_WINDOW_NAME.to_string(), Value::String(label.to_string())),
    (PANEL_WINDOW_MINUTES.to_string(), Value::Number(minutes.into())),
    (PANEL_UTILIZATION.to_string(), json!(100.0 - remaining * 100.0)),
    (PANEL_SCOPE_LABEL.to_string(), Value::String(group_name.to_string())),
  ]);
  if let Some(reset) = reset {
    window.insert(PANEL_RESETS_AT.to_string(), Value::String(reset.to_str()));
  }
  Ok(Value::Object(window))
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn parses_the_usage_envelope_and_selects_the_configured_group_windows() {
    let response = json!({"status": "SUCCESS", "command": {"data": {"groups": [
      {"name": "Series 1", "description": "Models within this group: Series 1", "buckets": [
        {"window": "weekly", "remaining_fraction": 0.75, "reset_time": "2026-11-14T08:19:00Z"},
        {"window": "5h", "remaining_fraction": 0.92, "reset_time": "2026-11-07T13:19:00Z"}
      ]}
    ]}}});
    let sampled = UtcDateTime::parse("2026-11-07T08:19:00Z").unwrap();

    let (entry, readings) = parse_usage(&response, &["series-1".to_string()], sampled).unwrap();

    assert_eq!(entry[PANEL_WINDOWS].as_array().unwrap().len(), 2);
    assert_eq!(entry[PANEL_WINDOWS][0][PANEL_WINDOW_NAME], "每周");
    assert_eq!(entry[PANEL_WINDOWS][0][PANEL_UTILIZATION], 25.0);
    assert_eq!(entry[PANEL_WINDOWS][1][PANEL_WINDOW_NAME], "5 小时");
    assert_eq!(entry[PANEL_WINDOWS][1][PANEL_UTILIZATION], 8.0);
    assert_eq!(readings.get("series-1").unwrap().len(), 2);
  }

  #[test]
  fn resolves_a_model_variant_to_its_unique_best_matching_group() {
    let response = json!({"status": "SUCCESS", "command": {"data": {"groups": [
      {"name": "Model Alpha", "description": "Model family Alpha", "buckets": [
        {"window": "weekly", "remaining_fraction": 0.95, "reset_time": "2026-11-14T08:19:00Z"}
      ]},
      {"name": "Model Beta", "description": "Model family Beta", "buckets": [
        {"window": "weekly", "remaining_fraction": 0.85, "reset_time": "2026-11-14T08:19:00Z"}
      ]}
    ]}}});

    let (entry, readings) =
      parse_usage(&response, &["model-alpha-1-high".to_string()], UtcDateTime::parse("2026-11-07T08:19:00Z").unwrap())
        .unwrap();

    assert_eq!(entry[PANEL_WINDOWS][0][PANEL_SCOPE_LABEL], "Model Alpha");
    let utilization = readings["model-alpha-1-high"][0][PANEL_UTILIZATION].as_f64().unwrap();
    assert!((utilization - 5.0).abs() < 1e-9);
  }

  #[test]
  fn rejects_zero_and_multiple_group_matches_with_the_model_name() {
    let group = json!({"name": "Series 1", "description": "Series 1", "buckets": []});
    assert!(
      parse_usage(
        &json!({"status": "SUCCESS", "command": {"data": {"groups": []}}}),
        &["series-1".to_string()],
        UtcDateTime::now(),
      )
      .unwrap_err()
      .to_string()
      .contains("series-1")
    );
    assert!(
      parse_usage(
        &json!({"status": "SUCCESS", "command": {"data": {"groups": [group.clone(), group]}}}),
        &["series-1".to_string()],
        UtcDateTime::now(),
      )
      .unwrap_err()
      .to_string()
      .contains("series-1")
    );
  }
}

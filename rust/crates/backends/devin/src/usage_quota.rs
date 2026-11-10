//! Devin's subscription quota account and the shared state its Auto member readings use.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use charliebot_core::config::{BackendOption, CharlieBotConfig, PoolBilling};
use charliebot_registry::backend::{MemberReading, MemberReadingFn};
use charliebot_registry::usage::{
  DEFAULT_ACCOUNT_LABEL, PANEL_FETCHED_AT, PANEL_PLAN_TYPE, PANEL_PROVIDER, PANEL_RESETS_AT, PANEL_UTILIZATION,
  PANEL_WINDOW_MINUTES, PANEL_WINDOWS, QuotaAccount, UsageOps, UsageSourceImpl, WEEKLY_WINDOW_MINUTES,
  mark_expired_windows, member_reading_from_windows,
};
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use tokio::process::Command;

const PROVIDER: &str = "devin";
const PROVIDER_LABEL: &str = "Devin";
const STATUS_URL_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const MODEL_CATALOG_ARGS: [&str; 4] = ["models", "list", "--format", "json"];
const DAILY_WINDOW_MINUTES: i64 = 1_440;

/// The state shared by the Devin panel account and its pool-member reader.
#[derive(Default)]
pub struct DevinQuotaState {
  current: Mutex<QuotaState>,
}

#[derive(Default)]
struct QuotaState {
  last_error: String,
  reading: Option<Map<String, Value>>,
  free_models: HashSet<String>,
}

/// The Devin CLI's plan account.
pub struct DevinQuotaAccount {
  state: Arc<DevinQuotaState>,
  credentials_path: PathBuf,
  host: charliebot_agents::cli_base::CliHost,
}

impl DevinQuotaAccount {
  pub fn new(
    state: Arc<DevinQuotaState>,
    credentials_path: PathBuf,
    host: charliebot_agents::cli_base::CliHost,
  ) -> Self {
    Self { state, credentials_path, host }
  }

  async fn fetch_round(&self) -> anyhow::Result<Option<Map<String, Value>>> {
    let result = self.fetch_sources().await;
    match result {
      Ok((entry, free_models)) => {
        let mut current = self.state.current.lock().expect("the Devin quota state is not poisoned");
        current.reading = Some(entry.clone());
        current.free_models = free_models;
        current.last_error.clear();
        Ok(Some(entry))
      }
      Err(error) => {
        self.state.current.lock().expect("the Devin quota state is not poisoned").last_error = format!("{error:#}");
        Err(error)
      }
    }
  }

  async fn fetch_sources(&self) -> anyhow::Result<(Map<String, Value>, HashSet<String>)> {
    let credentials_text = std::fs::read_to_string(&self.credentials_path).context("reading Devin CLI credentials")?;
    let credentials: DevinCredentials =
      toml::from_str(&credentials_text).map_err(|_| anyhow!("parsing Devin CLI credentials failed"))?;
    charliebot_compat::http_client::ensure_crypto_provider();
    let client = reqwest::Client::builder()
      .timeout(Duration::from_secs(30))
      .build()
      .context("building the Devin usage HTTP client")?;
    let request = json!({
      "metadata": {
        "apiKey": credentials.windsurf_api_key,
        "ideName": "devin",
        "ideVersion": "1.108.2",
        "extensionName": "devin",
        "extensionVersion": "1.108.2",
        "locale": "en"
      }
    });
    let response = client
      .post(format!("{}{}", credentials.api_server_url.trim_end_matches('/'), STATUS_URL_PATH))
      .header("Content-Type", "application/json")
      .header("Connect-Protocol-Version", "1")
      .json(&request)
      .send()
      .await
      .map_err(|_| anyhow!("Devin GetUserStatus request failed"))?;
    if !response.status().is_success() {
      bail!("Devin GetUserStatus returned HTTP {}", response.status());
    }
    let status: Value = response.json().await.map_err(|_| anyhow!("Devin GetUserStatus response was not JSON"))?;
    let entry = parse_status(&status, UtcDateTime::now())?;
    let devin_bin = self.host.resolve_binary("devin")?;
    let output = Command::new(devin_bin)
      .args(MODEL_CATALOG_ARGS)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::null())
      .output()
      .await
      .context("running the Devin model catalog")?;
    if !output.status.success() {
      bail!("Devin model catalog exited with {}", output.status);
    }
    let catalog: Value = serde_json::from_slice(&output.stdout).context("parsing the Devin model catalog")?;
    let free_models = free_models(&catalog)?;
    Ok((entry, free_models))
  }

  fn mark_expired(&self, entry: &Map<String, Value>, now: UtcDateTime) -> anyhow::Result<Map<String, Value>> {
    let sampled = entry.get(PANEL_FETCHED_AT).and_then(Value::as_str).map(UtcDateTime::parse).transpose()?;
    Ok(mark_expired_windows(entry, sampled, now))
  }
}

impl QuotaAccount for DevinQuotaAccount {
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
    self.state.current.lock().expect("the Devin quota state is not poisoned").last_error.clone()
  }

  fn fetch<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Option<Map<String, Value>>>> {
    Box::pin(self.fetch_round())
  }

  fn mark_expired(&self, entry: &Map<String, Value>, now: UtcDateTime) -> anyhow::Result<Map<String, Value>> {
    DevinQuotaAccount::mark_expired(self, entry, now)
  }
}

/// The usage source's account registration and the shared reading closure.
pub struct DevinUsageSource {
  account: Arc<DevinQuotaAccount>,
}

impl DevinUsageSource {
  pub fn new(account: Arc<DevinQuotaAccount>) -> Self {
    Self { account }
  }

  pub fn member_reading(state: Arc<DevinQuotaState>) -> MemberReadingFn {
    Arc::new(move |_cfg, option, now| read_member(&state, option, now))
  }
}

impl UsageSourceImpl for DevinUsageSource {
  fn ops(&self) -> UsageOps {
    UsageOps { quota_accounts: true, ..UsageOps::default() }
  }

  fn quota_accounts(&self, _cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    Ok(vec![Arc::clone(&self.account) as Arc<dyn QuotaAccount>])
  }
}

fn read_member(
  state: &DevinQuotaState,
  option: &BackendOption,
  now: UtcDateTime,
) -> anyhow::Result<Option<MemberReading>> {
  if option.billing != Some(PoolBilling::Subscription) {
    return Ok(None);
  }
  let current = state.current.lock().expect("the Devin quota state is not poisoned");
  let Some(entry) = current.reading.as_ref() else { return Ok(None) };
  if current.free_models.contains(&model_key(&option.model)) {
    return Ok(None);
  }
  let sampled = entry
    .get(PANEL_FETCHED_AT)
    .and_then(Value::as_str)
    .map(UtcDateTime::parse)
    .transpose()?
    .expect("a successful Devin reading carries its sample time");
  let windows =
    entry.get(PANEL_WINDOWS).and_then(Value::as_array).expect("a successful Devin reading carries its windows");
  member_reading_from_windows(windows, sampled, now)
}

#[derive(Deserialize)]
struct DevinCredentials {
  api_server_url: String,
  windsurf_api_key: String,
}

fn parse_status(status: &Value, fetched_at: UtcDateTime) -> anyhow::Result<Map<String, Value>> {
  let plan_status = status
    .get("userStatus")
    .and_then(|value| value.get("planStatus"))
    .and_then(Value::as_object)
    .ok_or_else(|| anyhow!("Devin response has no userStatus.planStatus object"))?;
  let remaining = |key: &str| -> anyhow::Result<f64> {
    let Some(value) = plan_status.get(key) else {
      return Ok(0.0);
    };
    value.as_f64().ok_or_else(|| anyhow!("Devin plan field {key} is not numeric"))
  };
  let daily_reset = reset_timestamp(plan_status.get("dailyQuotaResetAtUnix"))?;
  let weekly_reset = reset_timestamp(plan_status.get("weeklyQuotaResetAtUnix"))?;
  let plan =
    plan_status.get("planInfo").and_then(|value| value.get("planName")).and_then(Value::as_str).map(str::to_string);
  let mut entry = Map::from_iter([
    (PANEL_PROVIDER.to_string(), Value::String(PROVIDER.to_string())),
    (PANEL_FETCHED_AT.to_string(), Value::String(fetched_at.to_str())),
    (
      PANEL_WINDOWS.to_string(),
      Value::Array(vec![
        quota_window(DAILY_WINDOW_MINUTES, 100.0 - remaining("dailyQuotaRemainingPercent")?, daily_reset),
        quota_window(WEEKLY_WINDOW_MINUTES, 100.0 - remaining("weeklyQuotaRemainingPercent")?, weekly_reset),
      ]),
    ),
  ]);
  if let Some(plan) = plan {
    entry.insert(PANEL_PLAN_TYPE.to_string(), Value::String(plan));
  }
  Ok(entry)
}

/// One fixed-length window. The panel names it from its minutes, so it carries no `window_name`.
fn quota_window(minutes: i64, utilization: f64, reset: Option<UtcDateTime>) -> Value {
  let mut window = Map::from_iter([
    (PANEL_WINDOW_MINUTES.to_string(), Value::Number(minutes.into())),
    (PANEL_UTILIZATION.to_string(), json!(utilization)),
  ]);
  if let Some(reset) = reset {
    window.insert(PANEL_RESETS_AT.to_string(), Value::String(reset.to_str()));
  }
  Value::Object(window)
}

fn reset_timestamp(value: Option<&Value>) -> anyhow::Result<Option<UtcDateTime>> {
  let Some(value) = value else { return Ok(None) };
  let seconds = match value {
    Value::Number(number) => number.as_i64(),
    Value::String(text) => text.parse::<i64>().ok(),
    _ => None,
  }
  .ok_or_else(|| anyhow!("Devin quota reset is not an integer Unix timestamp"))?;
  if seconds == 0 {
    return Ok(None);
  }
  let instant = OffsetDateTime::from_unix_timestamp(seconds).context("Devin quota reset is outside the calendar")?;
  let iso = instant.to_offset(UtcOffset::UTC).format(&Rfc3339).context("formatting Devin quota reset")?;
  Ok(Some(UtcDateTime::parse(&iso)?))
}

fn free_models(catalog: &Value) -> anyhow::Result<HashSet<String>> {
  let families = catalog
    .get("families")
    .and_then(Value::as_array)
    .ok_or_else(|| anyhow!("Devin model catalog has no families array"))?;
  let mut free = HashSet::new();
  for family in families {
    let variants = family
      .get("variants")
      .and_then(Value::as_array)
      .ok_or_else(|| anyhow!("Devin model family has no variants array"))?;
    for variant in variants {
      let cost_tier = variant.get("cost_tier").and_then(Value::as_str).unwrap_or_default();
      if !cost_tier.trim().eq_ignore_ascii_case("free") {
        continue;
      }
      for field in ["model_uid", "label"] {
        if let Some(value) = variant.get(field).and_then(Value::as_str) {
          free.insert(model_key(value));
        }
      }
    }
  }
  Ok(free)
}

fn model_key(model: &str) -> String {
  model.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
  use charliebot_registry::usage::{PANEL_STATUS_ROWS, PANEL_WINDOW_NAME};
  use serde_json::json;

  use super::*;

  #[test]
  fn parses_the_official_status_shape_and_treats_omitted_quota_as_empty() {
    let fetched_at = UtcDateTime::parse("2026-11-07T08:19:00Z").unwrap();
    let status = json!({
      "userStatus": {"planStatus": {
        "dailyQuotaRemainingPercent": 82,
        "dailyQuotaResetAtUnix": "1794096060",
        "weeklyQuotaResetAtUnix": "1794697260",
        "planInfo": {"planName": "Subscription"}
      }}
    });

    let entry = parse_status(&status, fetched_at).unwrap();

    assert_eq!(entry[PANEL_WINDOWS][0][PANEL_UTILIZATION], 18.0);
    assert_eq!(entry[PANEL_WINDOWS][0][PANEL_WINDOW_MINUTES], 1440);
    assert_eq!(entry[PANEL_WINDOWS][1][PANEL_WINDOW_MINUTES], 10080);
    assert!(entry[PANEL_WINDOWS][0].get(PANEL_WINDOW_NAME).is_none());
    assert_eq!(entry[PANEL_WINDOWS][1][PANEL_UTILIZATION], 100.0);
    assert_eq!(entry[PANEL_PLAN_TYPE], "Subscription");

    let omitted = parse_status(&json!({"userStatus": {"planStatus": {}}}), fetched_at).unwrap();
    assert_eq!(omitted[PANEL_WINDOWS][0][PANEL_UTILIZATION], 100.0);
    assert_eq!(omitted[PANEL_WINDOWS][1][PANEL_UTILIZATION], 100.0);
  }

  #[test]
  fn free_catalog_marks_only_variants_named_by_the_catalog() {
    let catalog = json!({"families": [{"variants": [
      {"model_uid": "1", "label": "series-1", "cost_tier": "Free"},
      {"model_uid": "2", "label": "series-2", "cost_tier": "Paid"}
    ]}]});

    let free = free_models(&catalog).unwrap();

    assert!(free.contains("1"));
    assert!(free.contains("series-1"));
    assert!(!free.contains("2"));
  }

  #[test]
  fn the_panel_entry_carries_no_member_rows_in_any_state() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(DevinQuotaState::default());
    let host = charliebot_agents::cli_base::CliHost { path: String::new(), home: root.path().to_path_buf() };
    let account = DevinQuotaAccount::new(state, root.path().join("credentials"), host);
    let sampled = UtcDateTime::parse("2026-11-07T08:19:00Z").unwrap();
    let mut success = Map::new();
    success.insert(PANEL_FETCHED_AT.to_string(), Value::String(sampled.to_str()));
    success.insert(PANEL_WINDOWS.to_string(), Value::Array(Vec::new()));
    let error = Map::from_iter([("error".to_string(), json!("source failed"))]);
    let pending = Map::from_iter([("pending".to_string(), Value::Bool(true))]);

    for entry in [&success, &error, &pending] {
      let marked = account.mark_expired(entry, sampled).unwrap();
      assert!(!marked.contains_key(PANEL_STATUS_ROWS), "{marked:?}");
    }
  }

  #[test]
  fn an_exhausted_member_reads_as_exhausted_until_its_window_resets() {
    let state = DevinQuotaState::default();
    let sampled = "2026-11-07T08:19:00Z";
    state.current.lock().unwrap().reading = Some(Map::from_iter([
      (PANEL_FETCHED_AT.to_string(), json!(sampled)),
      (
        PANEL_WINDOWS.to_string(),
        json!([{
          PANEL_WINDOW_MINUTES: 1440,
          PANEL_UTILIZATION: 100.0,
          PANEL_RESETS_AT: "2026-11-08T00:01:00Z",
        }]),
      ),
    ]));
    let option = BackendOption {
      id: "dv".to_string(),
      label: "Devin".to_string(),
      model: "devin-model".to_string(),
      prompt_overlay: None,
      billing: Some(PoolBilling::Subscription),
      backend_type: "devin".to_string(),
      type_fields: serde_yaml::Mapping::new(),
      visible: true,
    };

    let reading = read_member(&state, &option, UtcDateTime::parse(sampled).unwrap())
      .unwrap()
      .expect("a successful Devin reading is a member reading");

    assert!(reading.exhausted);
  }
}

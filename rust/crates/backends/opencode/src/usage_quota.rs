//! OpenCode Go's quota account and the shared state its Auto member readings use.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use charliebot_core::config::{BackendOption, PoolBilling};
use charliebot_registry::backend::{MemberReading, MemberReadingFn};
use charliebot_registry::usage::{
  DEFAULT_ACCOUNT_LABEL, PANEL_FETCHED_AT, PANEL_PROVIDER, PANEL_RESETS_AT, PANEL_UTILIZATION, PANEL_WINDOW_MINUTES,
  PANEL_WINDOW_NAME, PANEL_WINDOWS, QuotaAccount, WEEKLY_WINDOW_MINUTES, member_reading_from_windows,
};
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use rusqlite::OptionalExtension;
use serde_json::{Map, Value, json};
use tokio::task;

use crate::sqlite::open_read_only;

const PROVIDER: &str = "opencode";
const PROVIDER_LABEL: &str = "OpenCode Go";
const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

/// The state shared by the OpenCode panel account and its Auto member reader.
#[derive(Default)]
pub struct OpenCodeQuotaState {
  current: Mutex<QuotaState>,
}

#[derive(Default)]
struct QuotaState {
  last_error: String,
  reading: Option<Map<String, Value>>,
}

/// The read-only credential and usage reader for OpenCode Go.
pub struct OpenCodeQuotaAccount {
  db: PathBuf,
  state: Arc<OpenCodeQuotaState>,
}

impl OpenCodeQuotaAccount {
  pub fn new(db: PathBuf, state: Arc<OpenCodeQuotaState>) -> Self {
    Self { db, state }
  }

  async fn fetch_round(&self) -> anyhow::Result<Option<Map<String, Value>>> {
    let db = self.db.clone();
    let access = task::spawn_blocking(move || read_access_token(&db))
      .await
      .context("the OpenCode credential read task failed")??;
    charliebot_cli_common::http::ensure_crypto_provider();
    let client = reqwest::Client::builder()
      .timeout(Duration::from_secs(30))
      .build()
      .context("building the OpenCode Go usage HTTP client")?;
    let response = client
      .get(USAGE_URL)
      .bearer_auth(access)
      .send()
      .await
      .map_err(|_| anyhow!("OpenCode Go usage request failed"))?;
    if !response.status().is_success() {
      bail!("OpenCode Go usage returned HTTP {}", response.status());
    }
    let data: Value = response.json().await.map_err(|_| anyhow!("OpenCode Go response was not JSON"))?;
    let entry = parse_usage(&data, UtcDateTime::now())?;
    let mut current = self.state.current.lock().expect("the OpenCode quota state is not poisoned");
    current.reading = Some(entry.clone());
    current.last_error.clear();
    Ok(Some(entry))
  }
}

impl QuotaAccount for OpenCodeQuotaAccount {
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
    self.state.current.lock().expect("the OpenCode quota state is not poisoned").last_error.clone()
  }

  fn fetch<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Option<Map<String, Value>>>> {
    Box::pin(async move {
      let result = self.fetch_round().await;
      if let Err(error) = &result {
        self.state.current.lock().expect("the OpenCode quota state is not poisoned").last_error = format!("{error:#}");
      }
      result
    })
  }
}

impl OpenCodeQuotaState {
  /// The Auto member reader backed only by the last official response.
  pub fn member_reading(state: Arc<Self>) -> MemberReadingFn {
    Arc::new(move |_cfg, option, now| read_member(&state, option, now))
  }
}

fn read_member(
  state: &OpenCodeQuotaState,
  option: &BackendOption,
  now: UtcDateTime,
) -> anyhow::Result<Option<MemberReading>> {
  if option.billing != Some(PoolBilling::Subscription) || !option.model.starts_with("opencode-go/") {
    return Ok(None);
  }
  let current = state.current.lock().expect("the OpenCode quota state is not poisoned");
  let Some(entry) = current.reading.as_ref() else { return Ok(None) };
  let sampled = entry
    .get(PANEL_FETCHED_AT)
    .and_then(Value::as_str)
    .map(UtcDateTime::parse)
    .transpose()?
    .expect("a successful OpenCode Go reading carries its sample time");
  let windows =
    entry.get(PANEL_WINDOWS).and_then(Value::as_array).expect("a successful OpenCode Go reading carries its windows");
  member_reading_from_windows(windows, sampled, now)
}

fn read_access_token(db: &std::path::Path) -> anyhow::Result<String> {
  let connection = open_read_only(db).context("opening the OpenCode credential store read-only")?;
  let value = connection
    .query_row("select value from credential where active", [], |row| row.get::<_, String>(0))
    .optional()
    .context("reading the active OpenCode credential")?
    .ok_or_else(|| anyhow!("the active OpenCode credential is missing"))?;
  let credential: Value = serde_json::from_str(&value).context("parsing the active OpenCode credential")?;
  credential
    .get("access")
    .and_then(Value::as_str)
    .filter(|access| !access.is_empty())
    .map(str::to_string)
    .ok_or_else(|| anyhow!("the active OpenCode credential has no access token"))
}

fn parse_usage(data: &Value, fetched_at: UtcDateTime) -> anyhow::Result<Map<String, Value>> {
  let usage =
    data.get("usage").and_then(Value::as_object).ok_or_else(|| anyhow!("OpenCode Go response has no usage object"))?;
  let windows =
    [("rolling", "5 小时", Some(300)), ("weekly", "每周", Some(WEEKLY_WINDOW_MINUTES)), ("monthly", "每月", None)];
  let windows = windows
    .into_iter()
    .map(|(key, name, minutes)| {
      let reading =
        usage.get(key).and_then(Value::as_object).ok_or_else(|| anyhow!("OpenCode usage has no {key} window"))?;
      let utilization = reading
        .get("percent")
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("OpenCode {key} utilization is not numeric"))?;
      let resets_at = reading
        .get("resetsAt")
        .and_then(Value::as_str)
        .filter(|reset| !reset.is_empty())
        .map(UtcDateTime::parse)
        .transpose()?;
      let mut window = Map::from_iter([
        (PANEL_WINDOW_NAME.to_string(), Value::String(name.to_string())),
        (PANEL_UTILIZATION.to_string(), json!(utilization)),
      ]);
      if let Some(minutes) = minutes {
        window.insert(PANEL_WINDOW_MINUTES.to_string(), Value::Number(minutes.into()));
      }
      if let Some(reset) = resets_at {
        window.insert(PANEL_RESETS_AT.to_string(), Value::String(reset.to_str()));
      }
      Ok(Value::Object(window))
    })
    .collect::<anyhow::Result<Vec<_>>>()?;
  Ok(Map::from_iter([
    (PANEL_PROVIDER.to_string(), Value::String(PROVIDER.to_string())),
    (PANEL_FETCHED_AT.to_string(), Value::String(fetched_at.to_str())),
    (PANEL_WINDOWS.to_string(), Value::Array(windows)),
  ]))
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn parses_the_official_usage_shape_and_keeps_month_length_unspecified() {
    let fetched_at = UtcDateTime::parse("2026-11-07T08:19:00Z").unwrap();
    let entry = parse_usage(
      &json!({"usage": {
        "rolling": {"percent": 0, "resetsAt": "2026-11-07T13:19:00Z", "status": "ok"},
        "weekly": {"percent": 3, "resetsAt": "2026-11-14T08:19:00Z", "status": "ok"},
        "monthly": {"percent": 97, "resetsAt": "2026-12-01T08:19:00Z", "status": "ok"}
      }}),
      fetched_at,
    )
    .unwrap();

    assert_eq!(entry[PANEL_WINDOWS][0][PANEL_WINDOW_MINUTES], 300);
    assert_eq!(entry[PANEL_WINDOWS][0][PANEL_WINDOW_NAME], "5 小时");
    assert_eq!(entry[PANEL_WINDOWS][1][PANEL_UTILIZATION], 3.0);
    assert!(entry[PANEL_WINDOWS][2].get(PANEL_WINDOW_MINUTES).is_none());
    assert_eq!(entry[PANEL_WINDOWS][2][PANEL_WINDOW_NAME], "每月");
  }

  #[test]
  fn a_subscription_on_another_provider_does_not_use_go_quota() {
    let now = UtcDateTime::parse("2026-11-07T08:19:00Z").unwrap();
    let state = OpenCodeQuotaState::default();
    state.current.lock().unwrap().reading = Some(
      parse_usage(
        &json!({"usage": {
          "rolling": {"percent": 0}, "weekly": {"percent": 3}, "monthly": {"percent": 97}
        }}),
        now,
      )
      .unwrap(),
    );
    let mut option = BackendOption {
      id: "member".into(),
      label: "Member".into(),
      model: "other/model".into(),
      prompt_overlay: None,
      billing: Some(PoolBilling::Subscription),
      backend_type: "opencode".into(),
      type_fields: Default::default(),
    };
    assert!(read_member(&state, &option, now).unwrap().is_none());
    option.model = "opencode-go/model".into();
    assert!(read_member(&state, &option, now).unwrap().unwrap().near_exhaustion);
  }
}

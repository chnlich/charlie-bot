//! Codex's quota account for the quota panel (`src/backends/codex/usage_quota.py`). The payload is read from the
//! rollouts under `<codex home>/sessions`. The newest plan-pool `token_count` event gives the rate-limit windows and
//! credits. The 24-hour and 7-day spend is priced from every recent rollout's per-request token counts. Nothing is
//! written, and no network call is made.

use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, bail};
use charliebot_core::str_methods;
use charliebot_core::timefmt::utc_now_iso;
use charliebot_registry::usage::{PANEL_EXPIRED, PANEL_WINDOWS, QuotaAccount};
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use lru::LruCache;
use memchr::memchr;
use serde_json::{Map, Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use super::pricing;
use super::rollout::{self, mtime_ns, mtime_secs, token_count_payload, walk_files};
use super::text::{empty_object, object_or_empty, py_splitlines, truthy};
use charliebot_agents::usage_time::utc_isoformat;

const PROVIDER: &str = "codex";
const PROVIDER_LABEL: &str = "Codex";
const LABEL: &str = "main";
/// The rate-limit slots, in the order a window list carries them before it is sorted by window length.
const LIMIT_SLOTS: [&str; 2] = ["primary", "secondary"];
/// A `token_count` event closes every turn, so the newest one sits in the rollout's trailing bytes.
const USAGE_TAIL_BYTES: u64 = 1 << 20;
/// Hours a rollout's mtime may be old and still count for the plan-pool reading.
const SCAN_WINDOW_HOURS: i64 = 6;
/// Cap for the per-account spend memo. It must stay above the 7-day window's file count, or each round re-parses the
/// corpus.
const SPEND_CACHE_LIMIT: usize = 8192;

/// Codex's one account on the quota panel, kept by its usage source between rounds.
pub struct CodexQuotaAccount {
  inner: Arc<Inner>,
}

struct Inner {
  sessions_dir: PathBuf,
  last_error: Mutex<String>,
  spend_cache: Mutex<SpendMemo>,
}

impl CodexQuotaAccount {
  pub fn new(codex_home: &Path) -> Self {
    Self {
      inner: Arc::new(Inner {
        sessions_dir: codex_home.join(rollout::CODEX_SESSIONS_DIR_NAME),
        last_error: Mutex::new("no sessions found".to_string()),
        spend_cache: Mutex::new(SpendMemo::new(SPEND_CACHE_LIMIT)),
      }),
    }
  }
}

impl QuotaAccount for CodexQuotaAccount {
  fn provider(&self) -> &str {
    PROVIDER
  }

  fn provider_label(&self) -> &str {
    PROVIDER_LABEL
  }

  fn label(&self) -> &str {
    LABEL
  }

  fn last_error(&self) -> String {
    self.inner.last_error.lock().expect("the codex quota error is not poisoned").clone()
  }

  fn fetch<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Option<Map<String, Value>>>> {
    let inner = Arc::clone(&self.inner);
    Box::pin(async move { inner.fetch().await })
  }

  fn mark_expired(&self, entry: &Map<String, Value>, now: UtcDateTime) -> Map<String, Value> {
    let sampled = text_of(entry, "token_count_observed_at")
      .and_then(parse_iso_utc)
      .or_else(|| text_of(entry, "fetched_at").and_then(parse_iso_utc));
    mark_expired_windows(entry, sampled, now.into_inner())
  }
}

impl Inner {
  fn set_last_error(&self, message: &str) {
    *self.last_error.lock().expect("the codex quota error is not poisoned") = message.to_string();
  }

  /// One reading: the usage from the newest plan-pool event, and the spend, which is best effort.
  async fn fetch(self: Arc<Self>) -> anyhow::Result<Option<Map<String, Value>>> {
    let now = OffsetDateTime::now_utc();
    let sessions_dir = self.sessions_dir.clone();
    let paths = tokio::task::spawn_blocking(move || list_rollout_files(&sessions_dir))
      .await
      .context("the codex rollout listing task failed")??;
    let usage_side = Arc::clone(&self);
    let usage_paths = paths.clone();
    let spend_side = Arc::clone(&self);
    let (usage, spend) = tokio::join!(
      tokio::task::spawn_blocking(move || usage_side.fetch_usage(&usage_paths, now)),
      tokio::task::spawn_blocking(move || spend_side.compute_spend(&paths, now)),
    );
    let mut usage = match usage {
      Err(error) => {
        tracing::error!(
          target: "charliebot_agents",
          account = LABEL,
          error = %error,
          "ext_usage_codex_usage_error"
        );
        self.set_last_error("usage read failed");
        return Ok(None);
      }
      Ok(Ok(Some(usage))) => usage,
      Ok(Ok(None)) => return Ok(None),
      Ok(Err(error)) => {
        tracing::error!(
          target: "charliebot_agents",
          account = LABEL,
          error = %format!("{error:#}"),
          "ext_usage_codex_usage_error"
        );
        self.set_last_error("usage read failed");
        return Ok(None);
      }
    };
    let spend = match spend {
      Ok(Ok(totals)) => json!({"last_24h_usd": totals.last_24h_usd, "last_7d_usd": totals.last_7d_usd}),
      Ok(Err(error)) => {
        tracing::error!(
          target: "charliebot_agents",
          account = LABEL,
          error = %format!("{error:#}"),
          "ext_usage_codex_spend_failed"
        );
        Value::Null
      }
      Err(error) => {
        tracing::error!(
          target: "charliebot_agents",
          account = LABEL,
          error = %error,
          "ext_usage_codex_spend_failed"
        );
        Value::Null
      }
    };
    usage.insert("spend".to_string(), spend);
    Ok(Some(usage))
  }

  /// The newest plan-pool quota reading across the recently written rollouts. The scan set is every rollout whose
  /// mtime is inside the window, or the single newest file when none is. A scan set without a plan-pool event is a
  /// no-reading state, with `last_error` set.
  fn fetch_usage(&self, paths: &[PathBuf], now: OffsetDateTime) -> anyhow::Result<Option<Map<String, Value>>> {
    let mut stats: Vec<(&PathBuf, Metadata)> = Vec::new();
    for path in paths {
      match fs::metadata(path) {
        Ok(metadata) => stats.push((path, metadata)),
        Err(error) => tracing::warn!(
          target: "charliebot_agents",
          path = %path.display(),
          error = %error,
          "ext_usage_codex_rollout_stat_failed"
        ),
      }
    }
    if stats.is_empty() {
      self.set_last_error("no sessions found");
      return Ok(None);
    }
    let min_mtime = unix_seconds(now) - (SCAN_WINDOW_HOURS * 3600) as f64;
    let mut scan: Vec<&(&PathBuf, Metadata)> =
      stats.iter().filter(|(_, metadata)| mtime_secs(metadata) >= min_mtime).collect();
    let newest = stats.iter().reduce(
      |best, candidate| {
        if mtime_secs(&candidate.1) > mtime_secs(&best.1) { candidate } else { best }
      },
    );
    if scan.is_empty() {
      scan = newest.into_iter().collect();
    }

    let mut events = Vec::new();
    for (path, metadata) in scan {
      if let Some(event) = read_latest_token_count(path, metadata.len(), is_plan_pool_event)? {
        events.push(event);
      }
    }
    if events.is_empty() {
      self.set_last_error(&format!("no plan-quota reading in {SCAN_WINDOW_HOURS}h"));
      return Ok(None);
    }
    let mut chosen: Option<(OffsetDateTime, Map<String, Value>)> = None;
    for event in events {
      let observed = parse_codex_timestamp(event.get("timestamp"))?;
      if chosen.as_ref().is_none_or(|(best, _)| observed > *best) {
        chosen = Some((observed, event));
      }
    }
    let (_, event) = chosen.expect("the event list is not empty");
    Ok(Some(transform_response(&event, &utc_now_iso(), LABEL)?))
  }

  /// The 24-hour and 7-day spend across the rollouts modified in the last seven days.
  fn compute_spend(&self, paths: &[PathBuf], now: OffsetDateTime) -> anyhow::Result<SpendTotals> {
    let min_mtime = unix_seconds(now - Duration::days(7));
    let mut live = HashSet::new();
    let mut by_file: Vec<Arc<Vec<SpendEvent>>> = Vec::new();
    let mut memo = self.spend_cache.lock().expect("the codex spend cache is not poisoned");
    for path in paths {
      let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
          tracing::warn!(
            target: "charliebot_agents",
            path = %path.display(),
            error = %error,
            "ext_usage_codex_spend_file_skip"
          );
          continue;
        }
      };
      if mtime_secs(&metadata) < min_mtime {
        continue;
      }
      live.insert(path.clone());
      let signature = (mtime_ns(&metadata), metadata.len());
      if let Some(events) = memo.fresh(path, signature) {
        by_file.push(events);
        continue;
      }
      // A file that cannot be read is skipped, not cached, so the next round retries it.
      if let Some(events) = extract_spend_events(path)? {
        let events = Arc::new(events);
        memo.record(path.clone(), signature, Arc::clone(&events));
        by_file.push(events);
      }
    }
    memo.retain(&live);
    Ok(sum_spend(&by_file, now))
  }
}

fn text_of<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
  map.get(key).and_then(Value::as_str)
}

fn unix_seconds(instant: OffsetDateTime) -> f64 {
  instant.unix_timestamp_nanos() as f64 / 1e9
}

fn list_rollout_files(sessions_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
  if !sessions_dir.exists() {
    return Ok(Vec::new());
  }
  walk_files(sessions_dir, rollout::is_rollout_file)
    .with_context(|| format!("listing rollouts under {}", sessions_dir.display()))
}

/// Whether a `token_count` event reports the plan quota pool: `rate_limits.limit_id` is `codex`, or it names no pool.
fn is_plan_pool_event(event: &Map<String, Value>) -> anyhow::Result<bool> {
  let payload = object_or_empty(event, "payload")?;
  let rate_limits = object_or_empty(payload, "rate_limits")?;
  let limit_id = rate_limits.get("limit_id");
  Ok(limit_id == Some(&json!("codex")) || !rate_limits.get("limit_name").is_some_and(truthy))
}

/// The newest `token_count` event of a rollout that `matches` accepts. The tail window is read first, and a miss falls
/// back to the whole file. The window starts after its first newline, so no partial line is parsed.
fn read_latest_token_count(
  path: &Path,
  size: u64,
  matches: fn(&Map<String, Value>) -> anyhow::Result<bool>,
) -> anyhow::Result<Option<Map<String, Value>>> {
  let offset = size.saturating_sub(USAGE_TAIL_BYTES);
  let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
  file.seek(SeekFrom::Start(offset))?;
  let mut blob = Vec::new();
  file.read_to_end(&mut blob)?;
  if offset > 0 {
    blob = match memchr(b'\n', &blob) {
      Some(newline) => blob[newline + 1..].to_vec(),
      None => Vec::new(),
    };
  }
  let tail = std::str::from_utf8(&blob).with_context(|| format!("decoding the tail of {}", path.display()))?;
  if let Some(event) = latest_token_count(py_splitlines(tail), matches)? {
    return Ok(Some(event));
  }
  if offset == 0 {
    return Ok(None);
  }
  let whole = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
  let text = std::str::from_utf8(&whole).with_context(|| format!("decoding {}", path.display()))?;
  latest_token_count(py_splitlines(text), matches)
}

fn latest_token_count(
  lines: Vec<&str>,
  matches: fn(&Map<String, Value>) -> anyhow::Result<bool>,
) -> anyhow::Result<Option<Map<String, Value>>> {
  for raw_line in lines.iter().rev() {
    let line = str_methods::strip(raw_line);
    if line.is_empty() {
      continue;
    }
    let event = match serde_json::from_str::<Value>(line) {
      Ok(Value::Object(event)) => event,
      Ok(other) => bail!("rollout line is not a JSON object: {other}"),
      Err(error) => {
        tracing::debug!(target: "charliebot_agents", error = %error, "codex_rollout_parse_skip");
        continue;
      }
    };
    if token_count_payload(&event)?.is_none() || !matches(&event)? {
      continue;
    }
    return Ok(Some(event));
  }
  Ok(None)
}

/// A timestamp of a rollout record: RFC 3339 with an offset, read as UTC.
fn parse_codex_timestamp(value: Option<&Value>) -> anyhow::Result<OffsetDateTime> {
  let text = value.and_then(Value::as_str).context("expected a string timestamp")?;
  let parsed = OffsetDateTime::parse(text, &Rfc3339)
    .with_context(|| format!("Codex timestamp is not RFC 3339 with a timezone: {text}"))?;
  Ok(parsed.to_offset(UtcOffset::UTC))
}

/// An ISO 8601 timestamp as a UTC instant, or `None` when it is empty or unparseable. A timestamp without an offset is
/// read as UTC, as `datetime.fromisoformat` does in the Python usage source.
pub fn parse_iso_utc(value: &str) -> Option<OffsetDateTime> {
  if value.is_empty() {
    return None;
  }
  OffsetDateTime::parse(value, &Rfc3339)
    .or_else(|_| {
      let local = value.replacen(' ', "T", 1);
      OffsetDateTime::parse(&format!("{local}+00:00"), &Rfc3339)
    })
    .ok()
    .map(|parsed| parsed.to_offset(UtcOffset::UTC))
}

/// The utilization a limit reports as a percentage, or `None` when it reports none or a non-number.
fn as_utilization(value: Option<&Value>) -> Option<f64> {
  match value {
    Some(Value::Number(number)) => number.as_f64(),
    _ => None,
  }
}

/// Whether one stored window's reading is expired at `now`: its reset has passed while the sample predates it, or the
/// sample is older than the window is long. A missing sample time reads as live.
fn panel_window_expired(window: &Map<String, Value>, sampled: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
  let Some(sampled) = sampled else {
    return false;
  };
  let resets_at = text_of(window, "resets_at").and_then(parse_iso_utc);
  if let Some(resets_at) = resets_at
    && resets_at <= now
    && sampled < resets_at
  {
    return true;
  }
  match window.get("window_minutes") {
    Some(Value::Number(number)) if number.is_i64() => now - sampled > Duration::minutes(number.as_i64().unwrap_or(0)),
    _ => false,
  }
}

/// The entry with each expired window marked `expired: true`. An entry without a window list comes back unchanged.
pub fn mark_expired_windows(
  entry: &Map<String, Value>,
  sampled: Option<OffsetDateTime>,
  now: OffsetDateTime,
) -> Map<String, Value> {
  let Some(windows) = entry.get(PANEL_WINDOWS).and_then(Value::as_array) else {
    return entry.clone();
  };
  let annotated = windows
    .iter()
    .map(|window| match window.as_object() {
      Some(object) if panel_window_expired(object, sampled, now) => {
        let mut marked = object.clone();
        marked.insert(PANEL_EXPIRED.to_string(), Value::Bool(true));
        Value::Object(marked)
      }
      _ => window.clone(),
    })
    .collect();
  let mut marked = entry.clone();
  marked.insert(PANEL_WINDOWS.to_string(), Value::Array(annotated));
  marked
}

/// One rate-limit window per non-null slot, sorted by window length. A slot that lacks its window length is dropped.
fn codex_windows(rate_limits: &Map<String, Value>, account: &str) -> anyhow::Result<Vec<Value>> {
  let mut windows: Vec<(i64, Value)> = Vec::new();
  for slot in LIMIT_SLOTS {
    let Some(limit) = rate_limits.get(slot).and_then(Value::as_object) else {
      continue;
    };
    let window_minutes = match limit.get("window_minutes") {
      Some(Value::Number(number)) if number.is_i64() => number.as_i64().unwrap_or(0),
      _ => {
        warn_unknown_limit_shape(account, slot, "missing window_minutes");
        continue;
      }
    };
    let utilization = as_utilization(limit.get("used_percent"));
    if utilization.is_none() {
      warn_unknown_limit_shape(account, slot, "missing used_percent");
    }
    let resets_at = match limit.get("resets_at") {
      Some(Value::Number(number)) => utc_isoformat(number.as_f64().context("resets_at is not a finite number")?)?,
      _ => String::new(),
    };
    windows.push((
      window_minutes,
      json!({
        "window_minutes": window_minutes,
        "utilization": utilization,
        "resets_at": resets_at,
      }),
    ));
  }
  windows.sort_by_key(|(minutes, _)| *minutes);
  Ok(windows.into_iter().map(|(_, window)| window).collect())
}

/// The credits reading to emit, or `None` when its shape is not recognized. `unlimited` must be a boolean; a balance
/// that parses as a number is included.
fn codex_credits(raw: &Value, account: &str) -> Option<Map<String, Value>> {
  let Some(credits) = raw.as_object() else {
    warn_unknown_limit_shape(account, "credits", "credits is not an object");
    return None;
  };
  let Some(unlimited) = credits.get("unlimited").and_then(Value::as_bool) else {
    warn_unknown_limit_shape(account, "credits", "unlimited is not a bool");
    return None;
  };
  let mut emitted = Map::new();
  emitted.insert("unlimited".to_string(), Value::Bool(unlimited));
  match python_float(credits.get("balance")) {
    Some(balance) => {
      emitted.insert("balance".to_string(), json!(balance));
    }
    None => warn_unknown_limit_shape(account, "credits", "missing or unparseable balance"),
  }
  Some(emitted)
}

/// `float(value)` of a JSON value, as Python reads a balance: numbers and booleans convert, and so does a numeric
/// string.
fn python_float(value: Option<&Value>) -> Option<f64> {
  match value? {
    Value::Number(number) => number.as_f64(),
    Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
    Value::String(text) => str_methods::strip(text).parse::<f64>().ok(),
    _ => None,
  }
}

/// The usage entry of one token-count event: windows, credits, the observation time, and the business label.
fn transform_response(
  event: &Map<String, Value>,
  fetched_at: &str,
  account: &str,
) -> anyhow::Result<Map<String, Value>> {
  let payload = object_or_empty(event, "payload")?;
  let rate_limits = object_or_empty(payload, "rate_limits")?;
  let credits = rate_limits.get("credits").and_then(|raw| codex_credits(raw, account));
  let mut usage = Map::new();
  usage.insert(PANEL_WINDOWS.to_string(), Value::Array(codex_windows(rate_limits, account)?));
  usage.insert("fetched_at".to_string(), json!(fetched_at));
  usage.insert("provider".to_string(), json!(PROVIDER));
  usage.insert("token_count_observed_at".to_string(), event.get("timestamp").cloned().unwrap_or_else(|| json!("")));
  let unlimited = credits.as_ref().is_some_and(|reading| reading.get("unlimited") == Some(&Value::Bool(true)));
  if let Some(reading) = credits {
    usage.insert("credits".to_string(), Value::Object(reading));
  }
  let both_slots_null =
    rate_limits.get("primary") == Some(&Value::Null) && rate_limits.get("secondary") == Some(&Value::Null);
  let business_plan = !rate_limits.contains_key("credits") && rate_limits.get("plan_type") == Some(&json!("business"));
  if both_slots_null && (unlimited || business_plan) {
    usage.insert("state_label".to_string(), json!("business / unlimited"));
  }
  Ok(usage)
}

/// Logs one unrecognized limit shape the first time this process sees it.
fn warn_unknown_limit_shape(account: &str, slot: &str, reason: &str) {
  static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
  let key = format!("{account}\u{0}{slot}\u{0}{reason}");
  if SEEN.lock().expect("the limit-shape log set is not poisoned").insert(key) {
    tracing::warn!(
      target: "charliebot_agents",
      provider = PROVIDER,
      account,
      slot,
      reason,
      "ext_usage_unknown_limit_shape"
    );
  }
}

/// One spend row: the observation time, the model in force, and the last request's token counts.
type SpendEvent = (OffsetDateTime, String, SpendUsage);

#[derive(Clone, Copy, Default)]
struct SpendUsage {
  input: i64,
  cached: i64,
  output: i64,
}

impl SpendUsage {
  fn add(&mut self, other: &SpendUsage) {
    self.input += other.input;
    self.cached += other.cached;
    self.output += other.output;
  }
}

/// Spend totals per model, in the order each model first appears, so the priced sum adds in the Python order.
#[derive(Default)]
struct ModelTotals(Vec<(String, SpendUsage)>);

impl ModelTotals {
  fn add(&mut self, model: &str, usage: &SpendUsage) {
    match self.0.iter_mut().find(|(known, _)| known == model) {
      Some((_, bucket)) => bucket.add(usage),
      None => self.0.push((model.to_string(), *usage)),
    }
  }

  fn cost(&self) -> f64 {
    self
      .0
      .iter()
      .filter_map(|(model, usage)| {
        pricing::cost_usd(model, usage.input as f64, usage.cached as f64, usage.output as f64)
      })
      .fold(0.0, |total, cost| total + cost)
  }
}

struct SpendTotals {
  last_24h_usd: f64,
  last_7d_usd: f64,
}

fn sum_spend(by_file: &[Arc<Vec<SpendEvent>>], now: OffsetDateTime) -> SpendTotals {
  let one_day_ago = now - Duration::days(1);
  let seven_days_ago = now - Duration::days(7);
  let mut last_24h = ModelTotals::default();
  let mut last_7d = ModelTotals::default();
  for events in by_file {
    for (observed, model, usage) in events.iter() {
      if *observed < seven_days_ago || *observed > now {
        continue;
      }
      last_7d.add(model, usage);
      if *observed >= one_day_ago {
        last_24h.add(model, usage);
      }
    }
  }
  SpendTotals { last_24h_usd: last_24h.cost(), last_7d_usd: last_7d.cost() }
}

/// The spend events of one rollout, or `None` when the file cannot be read. A row that fails its checks is logged and
/// skipped. Model attribution is positional: a `turn_context` line sets the model for the token-count rows after it.
fn extract_spend_events(path: &Path) -> anyhow::Result<Option<Vec<SpendEvent>>> {
  let bytes = match fs::read(path) {
    Ok(bytes) => bytes,
    Err(error) => {
      tracing::warn!(
        target: "charliebot_agents",
        path = %path.display(),
        error = %error,
        "ext_usage_codex_spend_file_skip"
      );
      return Ok(None);
    }
  };
  let text = std::str::from_utf8(&bytes).with_context(|| format!("decoding {}", path.display()))?;
  let mut events = Vec::new();
  let mut current_model = String::new();
  for (index, raw_line) in py_splitlines(text).into_iter().enumerate() {
    let line = str_methods::strip(raw_line);
    if line.is_empty() {
      continue;
    }
    match spend_row(line, &mut current_model) {
      Ok(Some(event)) => events.push(event),
      Ok(None) => {}
      Err(error) => tracing::warn!(
        target: "charliebot_agents",
        path = %path.display(),
        line_number = index + 1,
        error = %format!("{error:#}"),
        "ext_usage_codex_spend_row_skipped"
      ),
    }
  }
  Ok(Some(events))
}

/// One rollout line as a spend event. `Ok(None)` for a line that carries no spend: a model record, a non-token record,
/// or a token count with no last request.
fn spend_row(line: &str, current_model: &mut String) -> anyhow::Result<Option<SpendEvent>> {
  let event = match serde_json::from_str::<Value>(line).context("line is not JSON")? {
    Value::Object(event) => event,
    other => bail!("expected a JSON object, got {other}"),
  };
  let payload = match event.get("payload") {
    None => empty_object(),
    Some(value) if !truthy(value) => empty_object(),
    Some(Value::Object(payload)) => payload,
    Some(other) => bail!("payload must be an object, got {other}"),
  };
  if rollout::text_of(&event, "type") == Some(rollout::TURN_CONTEXT) {
    if let Some(model) = payload.get("model").and_then(Value::as_str) {
      *current_model = model.to_string();
    }
    return Ok(None);
  }
  if token_count_payload(&event)?.is_none() {
    return Ok(None);
  }
  let info = match payload.get("info") {
    None => empty_object(),
    Some(value) if !truthy(value) => empty_object(),
    Some(Value::Object(info)) => info,
    Some(other) => bail!("info must be an object, got {other}"),
  };
  let last = match info.get("last_token_usage") {
    None => return Ok(None),
    Some(value) if !truthy(value) => return Ok(None),
    Some(Value::Object(last)) => last,
    Some(other) => bail!("last_token_usage must be an object, got {other}"),
  };
  let usage = SpendUsage {
    input: integer_field(last, "input_tokens")?,
    cached: integer_field(last, "cached_input_tokens")?,
    output: integer_field(last, "output_tokens")?,
  };
  let observed = parse_codex_timestamp(event.get("timestamp"))?;
  Ok(Some((observed, current_model.clone(), usage)))
}

/// A required integer token count. A boolean counts as its integer value, as Python's `int` check accepts it.
fn integer_field(map: &Map<String, Value>, key: &str) -> anyhow::Result<i64> {
  match map.get(key) {
    Some(Value::Number(number)) if number.is_i64() => Ok(number.as_i64().unwrap_or(0)),
    Some(Value::Bool(flag)) => Ok(i64::from(*flag)),
    Some(other) => bail!("{key} must be an int, got {other}"),
    None => bail!("{key} is missing"),
  }
}

/// A bounded memo of per-file spend events, keyed by path and valid while the file's (mtime, size) stands.
struct SpendMemo {
  entries: LruCache<PathBuf, MemoEntry>,
}

struct MemoEntry {
  signature: (i128, u64),
  events: Arc<Vec<SpendEvent>>,
}

impl SpendMemo {
  fn new(limit: usize) -> Self {
    let capacity = NonZeroUsize::new(limit).expect("the spend memo limit is positive");
    Self { entries: LruCache::new(capacity) }
  }

  /// The events stored for `path` under `signature`. A hit refreshes the entry's recency, as the Python memo does.
  fn fresh(&mut self, path: &Path, signature: (i128, u64)) -> Option<Arc<Vec<SpendEvent>>> {
    let entry = self.entries.get(path)?;
    (entry.signature == signature).then(|| Arc::clone(&entry.events))
  }

  /// Stores `events` for `path` under `signature`; the cache evicts the least recently used entry past its limit.
  fn record(&mut self, path: PathBuf, signature: (i128, u64), events: Arc<Vec<SpendEvent>>) {
    self.entries.put(path, MemoEntry { signature, events });
  }

  /// Drops every entry whose path is not in `live`.
  fn retain(&mut self, live: &HashSet<PathBuf>) {
    let stale: Vec<PathBuf> =
      self.entries.iter().filter(|(path, _)| !live.contains(*path)).map(|(path, _)| path.clone()).collect();
    for path in stale {
      self.entries.pop(&path);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn naive_iso_timestamps_are_read_as_utc() {
    assert_eq!(parse_iso_utc("2026-10-18T09:00:00").unwrap().unix_timestamp(), 1_792_314_000);
    assert_eq!(parse_iso_utc("2026-10-18 09:00:00").unwrap().unix_timestamp(), 1_792_314_000);
  }

  #[test]
  fn plan_pool_events_exclude_named_model_pools() {
    let plan = json!({"payload": {"rate_limits": {"limit_id": "codex"}}});
    let model_pool = json!({"payload": {"rate_limits": {"limit_id": "o3", "limit_name": "o3"}}});
    assert!(is_plan_pool_event(plan.as_object().unwrap()).unwrap());
    assert!(!is_plan_pool_event(model_pool.as_object().unwrap()).unwrap());
  }

  #[test]
  fn spend_memo_serves_a_matching_signature_and_evicts_the_oldest() {
    let mut memo = SpendMemo::new(1);
    memo.record(PathBuf::from("a"), (1, 1), Arc::new(Vec::new()));
    assert!(memo.fresh(Path::new("a"), (1, 1)).is_some());
    assert!(memo.fresh(Path::new("a"), (2, 1)).is_none());
    memo.record(PathBuf::from("b"), (1, 1), Arc::new(Vec::new()));
    assert!(memo.fresh(Path::new("a"), (1, 1)).is_none());
  }

  #[test]
  fn a_spend_memo_hit_makes_its_entry_the_last_to_evict() {
    let mut memo = SpendMemo::new(2);
    memo.record(PathBuf::from("a"), (1, 1), Arc::new(Vec::new()));
    memo.record(PathBuf::from("b"), (1, 1), Arc::new(Vec::new()));
    assert!(memo.fresh(Path::new("a"), (1, 1)).is_some());

    memo.record(PathBuf::from("c"), (1, 1), Arc::new(Vec::new()));

    assert!(memo.fresh(Path::new("a"), (1, 1)).is_some());
    assert!(memo.fresh(Path::new("b"), (1, 1)).is_none());
  }
}

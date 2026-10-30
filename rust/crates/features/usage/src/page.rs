//! `GET /token-usage` (`src/features/usage/api.py`): per-model token usage read from the usage ledger as of the last
//! capture. Loading the page never captures. Its cards are the registered usage sources in registration order, then the
//! sources whose usage lives only in CharlieBot's own run logs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::response::Response;
use charliebot_compat::{responses, routing};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::text_repr::{self, JsonStyle};
use charliebot_registry::FeatureContext;
use charliebot_registry::Registry;
use charliebot_registry::templating::Templating;
use charliebot_registry::usage::UsageSource;
use http::request::Parts;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;

use crate::capture::{CHARLIE_BOT_SOURCE, host_name};
use crate::count::Count;
use crate::ledger::{LedgerRow, UsageLedger, default_ledger_path};
use crate::page_rows::{self, PageRow, canonical_model, compact, group_thousands};

const TEMPLATE: &str = "token_usage.html";

/// `json.dumps({"rows": ...}, ensure_ascii=False)`: the payload the page's script reads its rows from.
const PAYLOAD_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// The router of the page's one route. The feature context is the route's state.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/token-usage", routing::get_only(token_usage_viewer)).with_state(Arc::clone(features))
}

/// The paths the routes answer, for the trailing-slash redirect.
pub fn matches(path: &str) -> bool {
  path == "/token-usage"
}

/// `token_usage_viewer`: the page from the ledger's rows. The read and the render run off the async runtime, as
/// `_read_ledger` runs in a thread.
async fn token_usage_viewer(State(features): State<Arc<FeatureContext>>, request: Parts) -> Response {
  let rendered = tokio::task::spawn_blocking(move || -> Result<Response, (String, String)> {
    let read = read_ledger(&features).map_err(|error| ("Exception".to_string(), format!("{error:#}")))?;
    render(&features, &read, &request)
  })
  .await
  .expect("rendering the token usage page does not panic");
  match rendered {
    Ok(response) => response,
    Err((kind, detail)) => responses::internal_error(&kind, detail),
  }
}

/// What `_read_ledger` returns: the ledger's rows, each source's native start, the last capture of this host, the time
/// the read took, and the config the rows' charlie-bot accounts attribute against.
pub(crate) struct LedgerRead {
  pub rows: Vec<LedgerRow>,
  pub native_starts: BTreeMap<String, String>,
  pub last_capture: Option<OffsetDateTime>,
  pub elapsed: Duration,
  pub config: Arc<CharlieBotConfig>,
}

/// `_read_ledger`: the rows and the last capture from the ledger alone, so the numbers survive the deletion of the logs
/// they were parsed from. A read error fails the request instead of rendering rows it could not read.
fn read_ledger(features: &FeatureContext) -> anyhow::Result<LedgerRead> {
  let started = Instant::now();
  let config = features.config.get()?;
  let mut ledger = UsageLedger::open(default_ledger_path(&config.charliebot_home))?;
  let (rows, native_starts) = ledger.model_rows_with_native_starts()?;
  let last_capture = ledger.last_capture_at(&host_name()?)?;
  ledger.close()?;
  Ok(LedgerRead { rows, native_starts, last_capture, elapsed: started.elapsed(), config })
}

/// Render the page from one ledger read through the shared template engine.
fn render(features: &FeatureContext, read: &LedgerRead, request: &Parts) -> Result<Response, (String, String)> {
  let context = token_usage_context(read, &features.registry, OffsetDateTime::now_utc())
    .map_err(|error| ("Exception".to_string(), format!("{error:#}")))?;
  let templating =
    features.startup.require::<Templating>().map_err(|error| ("RuntimeError".to_string(), error.to_string()))?;
  templating
    .render_page(features, TEMPLATE, &context, request)
    .map_err(|error| (error.kind().to_string(), error.to_string()))
}

/// `_cards`: the registered sources in registration order, then the sources whose usage lives only in CharlieBot's run
/// logs.
pub(crate) fn cards(registry: &Registry) -> Vec<&UsageSource> {
  let (mut cards, run_logs_only): (Vec<&UsageSource>, Vec<&UsageSource>) =
    registry.usage_sources().partition(|source| !source.run_logs_only);
  cards.extend(run_logs_only);
  cards
}

/// `_token_usage_context`: the template context of one ledger read. `payload` is the JSON the page's script reads its
/// rows from, and the other keys are the figures and stamps the template prints.
pub(crate) fn token_usage_context(
  read: &LedgerRead,
  registry: &Registry,
  now: OffsetDateTime,
) -> anyhow::Result<Value> {
  let cards = cards(registry);
  let tot = Totals::of(&read.rows)?;
  let merged = page_rows::merge(&read.rows, &read.config, registry, &cards)?;
  let rows = Value::Array(merged.iter().map(PageRow::to_json).collect());
  let (window_first, window_last) = window(&read.rows)?;
  let window_str = if read.rows.is_empty() { String::new() } else { format!("{window_first} → {window_last}") };
  let top = page_rows::first_max_by(merged.as_slice(), |row| row.total);
  let top_out = page_rows::first_max_by(merged.as_slice(), |row| row.output);
  let per_src = per_source(read, registry, &cards, &tot)?;
  let ctx = json!({
    "rows": rows.clone(),
    "tot_compact": compact(tot.total),
    "in_compact": compact(tot
      .in_fresh
      .add(tot.cache_write, "page input tokens")?
      .add(tot.cache_read, "page input tokens")?
      .add(tot.in_unsplit, "page input tokens")?),
    "out_compact": compact(tot.output),
    "cr_compact": compact(tot.cache_read),
    "cw_compact": compact(tot.cache_write),
    "fresh_compact": compact(tot.in_fresh),
    "fresh_percent": percent(tot.in_fresh, tot.total),
    "out_share": ratio(tot.output, tot.total),
    "per_src": per_src,
    "usage_sources": cards.iter().map(|card| card.name).collect::<Vec<_>>(),
    "tot_calls": group_thousands(Count::Int(tot.calls)),
    "top_escaped": top.map_or("", |row| row.model.as_str()),
    "top_compact": top.map_or_else(|| "0".to_string(), |row| compact(row.total)),
    "top_out_escaped": top_out.map_or("", |row| row.model.as_str()),
    "top_out_compact": top_out.map_or_else(|| "0".to_string(), |row| compact(row.output)),
    "elapsed_s": read.elapsed.as_secs_f64(),
  });
  Ok(json!({
    "ctx": ctx,
    "payload": text_repr::json_dumps(&json!({"rows": rows}), &PAYLOAD_JSON),
    "window": [window_first, window_last],
    "window_str": window_str,
    "cache_share": percent(tot.cache_read, tot.total),
    "generated": local_stamp(now),
    "as_of": as_of(read.last_capture),
  }))
}

/// The tiles of `_token_usage_context`: each card's attributed total, output, distinct models and share of the page
/// total, and its source's native start. An account attributes to the card that ran its call, so every token lands in
/// one tile.
fn per_source(
  read: &LedgerRead,
  registry: &Registry,
  cards: &[&UsageSource],
  tot: &Totals,
) -> anyhow::Result<Map<String, Value>> {
  let mut sums: BTreeMap<&str, Bucket> = cards.iter().map(|card| (card.name, Bucket::default())).collect();
  for row in &read.rows {
    let canonical = canonical_model(&row.model);
    for account in &row.accounts {
      let (source, _) = page_rows::account_source(row, &account.name, &read.config, registry)?;
      let bucket =
        sums.get_mut(source.as_str()).with_context(|| format!("usage source {source:?} is not a card of the page"))?;
      bucket.total = bucket.total.add(account.total, "usage source total")?;
      bucket.output = bucket.output.add(account.output, "usage source output")?;
      bucket.models.insert(canonical.clone());
    }
  }
  let mut per_src = Map::new();
  for card in cards {
    let bucket = &sums[card.name];
    // CharlieBot's own rows are all native usage of the run-logs-only cards, so their native start is the charlie-bot
    // span.
    let ledger_source = if card.run_logs_only { CHARLIE_BOT_SOURCE } else { card.name };
    per_src.insert(
      card.name.to_string(),
      json!({
        "total": page_rows::count_json(bucket.total),
        "t_comp": compact(bucket.total),
        "output": page_rows::count_json(bucket.output),
        "models": bucket.models.len(),
        "share": percent(bucket.total, tot.total),
        "native_start": read.native_starts.get(ledger_source).map_or("", String::as_str),
      }),
    );
  }
  Ok(per_src)
}

/// The sums of one card's tile.
#[derive(Default)]
struct Bucket {
  total: Count,
  output: Count,
  models: BTreeSet<String>,
}

/// The totals of every row, before the rows merge.
#[derive(Default)]
struct Totals {
  in_fresh: Count,
  cache_write: Count,
  cache_read: Count,
  in_unsplit: Count,
  output: Count,
  total: Count,
  calls: i64,
}

impl Totals {
  fn of(rows: &[LedgerRow]) -> anyhow::Result<Totals> {
    let mut tot = Totals::default();
    for row in rows {
      tot.in_fresh = tot.in_fresh.add(row.in_fresh, "page total in-fresh")?;
      tot.cache_write = tot.cache_write.add(row.cache_write, "page total cache-write")?;
      tot.cache_read = tot.cache_read.add(row.cache_read, "page total cache-read")?;
      tot.in_unsplit = tot.in_unsplit.add(row.in_unsplit, "page total in-unsplit")?;
      tot.output = tot.output.add(row.output, "page total output")?;
      tot.total = tot.total.add(row.total, "page total total")?;
      tot.calls += row.calls;
    }
    Ok(tot)
  }
}

/// The page's window: the earliest first stamp and the latest last stamp, or two empty strings when no row has a first
/// stamp. Python's `max()` of no last stamps fails the page, and so does this.
fn window(rows: &[LedgerRow]) -> anyhow::Result<(String, String)> {
  if !rows.iter().any(|row| !row.first.is_empty()) {
    return Ok((String::new(), String::new()));
  }
  let first = rows.iter().map(|row| row.first.as_str()).filter(|stamp| !stamp.is_empty()).min();
  let last = rows.iter().map(|row| row.last.as_str()).filter(|stamp| !stamp.is_empty()).max();
  let first = first.expect("a row has a first stamp");
  let last = last.context("max() arg is an empty sequence")?;
  Ok((first.to_string(), last.to_string()))
}

/// `part / whole * 100`, or `0.0` when the whole is zero.
fn percent(part: Count, whole: Count) -> f64 {
  ratio(part, whole) * 100.0
}

/// `part / whole`, or `0.0` when the whole is zero.
fn ratio(part: Count, whole: Count) -> f64 {
  let whole = whole.as_f64();
  if whole == 0.0 { 0.0 } else { part.as_f64() / whole }
}

/// `_as_of`: the last capture's finish in local time, or that none has run.
fn as_of(last_capture: Option<OffsetDateTime>) -> String {
  match last_capture {
    None => "no capture yet".to_string(),
    Some(moment) => format!("as of {}", local_stamp(moment)),
  }
}

/// `strftime('%Y-%m-%d %H:%M %Z')` of an instant in the server's zone (`astimezone()`). The zone abbreviation is
/// `localtime_r`'s, which `%Z` prints (host_auth's `timestamps.rs` reads the same fields).
pub(crate) fn local_stamp(moment: OffsetDateTime) -> String {
  jiff::Timestamp::from_second(moment.unix_timestamp())
    .expect("the usage timestamp fits Jiff's range")
    .to_zoned(jiff::tz::TimeZone::system())
    .strftime("%Y-%m-%d %H:%M %Z")
    .to_string()
}

#[cfg(test)]
#[path = "page_tests.rs"]
mod tests;

//! The usage page's tests (`tests/test_token_usage_page.py`): the rows, tiles and stamps of a seeded ledger, and of
//! synthetic rows with no ledger behind them. The page never captures, so no test reads a real home or log.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use charliebot_registry::templating::Templating;
use charliebot_registry::usage::{RecordKind, UsageRecord, UsageSource};
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

use super::*;
use crate::count::Count;
use crate::ledger::{LedgerAccount, LedgerRow, UsageLedger, default_ledger_path};
use crate::test_support::{feature_context, registry};

const CC: &str = "Claude Code";
const CODEX: &str = "Codex";
const OC: &str = "opencode";
const AGY: &str = "Antigravity";
const CLC: &str = "CLC";
const CB: &str = "charlie-bot";
const CC_TS: &str = "2026-01-10T08:00:00+00:00";
const CODEX_TS: &str = "2026-01-11T09:00:00+00:00";
const OC_TS: &str = "2026-01-12T10:00:00+00:00";
const CB_TS: &str = "2026-01-13T11:00:00+00:00";

fn repo_root() -> &'static Path {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate lies under the repo")
}

/// The features of a page test: `registry`'s sources, the repo's templates, and a home at `home`.
fn page_features(home: &Path, registry: Registry) -> Arc<FeatureContext> {
  let features = feature_context(home, registry);
  features.startup.insert(Templating::new(repo_root(), &["src.features.usage"])).expect("the templating inserts once");
  features
}

fn parts(path: &str) -> http::request::Parts {
  Request::builder().method(Method::GET).uri(path).body(()).expect("the request builds").into_parts().0
}

/// A native record of `account`, with the default in-fresh and cache-read tokens.
fn record(record_id: &str, source: &str, model: &str, ts: &str, output: i64, account: &str) -> UsageRecord {
  UsageRecord {
    record_id: record_id.to_string(),
    kind: RecordKind::Native,
    source: source.to_string(),
    model: model.to_string(),
    account: account.to_string(),
    ts: ts.to_string(),
    in_fresh: 10,
    cache_write: 0,
    cache_read: 5,
    output,
    in_unsplit: 0,
    sessions: Vec::new(),
  }
}

/// A counted-fallback record of the sessions its CLI log names.
fn fallback(record_id: &str, source: &str, model: &str, ts: &str, output: i64, sessions: &[&str]) -> UsageRecord {
  UsageRecord {
    kind: RecordKind::Fallback,
    sessions: sessions.iter().map(|session| session.to_string()).collect(),
    ..record(record_id, source, model, ts, output, "acct-a")
  }
}

/// Writes records into the ledger under `home`, one file signature per call.
fn add_records(home: &Path, path: &str, signature: &str, records: &[UsageRecord]) {
  let mut ledger = UsageLedger::open(default_ledger_path(home)).expect("the ledger opens");
  ledger.record_file("host", path, signature, records).expect("the records write");
  ledger.close().expect("the ledger closes");
}

/// Seeds one native record per source plus one counted-fallback Codex record, and stamps this host's capture when
/// asked.
fn seed_ledger(home: &Path, captured: bool) {
  add_records(home, "/logs/cc.jsonl", "sig-cc", &[record("cc-1", CC, "claude-sonnet-4", CC_TS, 100, "acct-a")]);
  add_records(
    home,
    "/logs/codex.jsonl",
    "sig-codex",
    &[
      record("cx-1", CODEX, "gpt-5", CODEX_TS, 200, "acct-a"),
      // No native record ever carries sess-pruned, so this fallback is counted.
      fallback("cx-fb", CODEX, "gpt-5", "2026-01-14T09:00:00+00:00", 7000, &["sess-pruned"]),
    ],
  );
  add_records(home, "/logs/oc.jsonl", "sig-oc", &[record("oc-1", OC, "o3", OC_TS, 300, "acct-a")]);
  add_records(home, "/logs/cb.jsonl", "sig-cb", &[record("cb-1", CB, "claude-haiku-4", CB_TS, 400, "charlie-code-x")]);
  if captured {
    let mut ledger = UsageLedger::open(default_ledger_path(home)).expect("the ledger opens");
    ledger.mark_capture_finished(&host_name().expect("the host has a name")).expect("the capture stamps");
    ledger.close().expect("the ledger closes");
  }
}

/// The page's read of the ledger under the features' home.
fn read_page_ledger(features: &FeatureContext) -> LedgerRead {
  read_ledger(features).expect("the ledger reads")
}

async fn page_body(features: &FeatureContext, read: &LedgerRead) -> String {
  let response =
    render(features, read, &parts("/token-usage")).unwrap_or_else(|(kind, detail)| panic!("{kind}: {detail}"));
  assert_eq!(response.status(), StatusCode::OK);
  let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
  String::from_utf8(bytes.to_vec()).expect("the page is UTF-8")
}

/// The rows the page's script feeds its charts and table from.
fn data_rows(body: &str) -> Vec<Value> {
  let start = body.find("const DATA = ").expect("the page carries a DATA payload") + "const DATA = ".len();
  let end = start + body[start..].find(";\n").expect("the payload ends its statement");
  let payload: Value = serde_json::from_str(&body[start..end]).expect("the payload is JSON");
  payload["rows"].as_array().expect("the payload holds rows").clone()
}

fn by_model(rows: &[Value]) -> BTreeMap<String, Value> {
  rows.iter().map(|row| (row["model"].as_str().expect("a row names its model").to_string(), row.clone())).collect()
}

fn read_of(
  features: &FeatureContext,
  rows: Vec<LedgerRow>,
  native_starts: &[(&str, &str)],
  last_capture: Option<OffsetDateTime>,
) -> LedgerRead {
  let now = Timestamp::now();
  LedgerRead {
    now,
    week: Week::last_before(now).expect("the week resolves"),
    rows,
    week_rows: Vec::new(),
    previous_week_rows: Vec::new(),
    native_starts: native_starts.iter().map(|(source, start)| (source.to_string(), start.to_string())).collect(),
    last_capture,
    elapsed: Duration::ZERO,
    config: features.config.get().expect("the config loads"),
  }
}

/// The page context of synthetic rows, with no ledger behind them.
fn context_of(features: &FeatureContext, rows: Vec<LedgerRow>) -> Value {
  token_usage_context(&read_of(features, rows, &[], None), &features.registry).expect("the context builds")
}

/// The serialized rows the page's script would feed its charts and table from.
fn payload_rows(context: &Value) -> Vec<Value> {
  let payload: Value = serde_json::from_str(context["payload"].as_str().expect("the payload is text")).expect("JSON");
  payload["rows"].as_array().expect("the payload holds rows").clone()
}

/// A synthetic page row: its token fields in `[in_fresh, cache_write, cache_read, in_unsplit, output]` order, its total
/// as the ledger keeps it, and the default dates.
fn row(source: &str, model: &str, tokens: [i64; 5], accounts: Vec<LedgerAccount>) -> LedgerRow {
  let [in_fresh, cache_write, cache_read, in_unsplit, output] = tokens;
  let count = |number: i64| Count::Int(number);
  LedgerRow {
    source: source.to_string(),
    model: model.to_string(),
    in_fresh: count(in_fresh),
    cache_write: count(cache_write),
    cache_read: count(cache_read),
    in_unsplit: count(in_unsplit),
    output: count(output),
    calls: 1,
    total: count(tokens.iter().sum()),
    first: "2026-02-01".to_string(),
    last: "2026-02-02".to_string(),
    fallback_calls: 0,
    fallback_output: Count::Int(0),
    accounts,
  }
}

fn with_fallback(mut row: LedgerRow, output: i64) -> LedgerRow {
  row.fallback_calls = 1;
  row.fallback_output = Count::Int(output);
  row
}

fn account(name: &str, calls: i64, output: i64, total: i64) -> LedgerAccount {
  LedgerAccount {
    name: name.to_string(),
    calls,
    in_fresh: Count::Int(total - output),
    cache_write: Count::Int(0),
    cache_read: Count::Int(0),
    in_unsplit: Count::Int(0),
    output: Count::Int(output),
    total: Count::Int(total),
  }
}

fn names(row: &Value) -> Vec<&str> {
  row["accounts"]
    .as_array()
    .expect("a row lists its accounts")
    .iter()
    .map(|account| account["name"].as_str().unwrap())
    .collect()
}

fn slots_and_totals(row: &Value) -> Vec<(i64, i64)> {
  row["segments"]
    .as_array()
    .expect("a row lists its segments")
    .iter()
    .map(|segment| (segment["slot"].as_i64().unwrap(), segment["total"].as_i64().unwrap()))
    .collect()
}

/// Sources registered in an order other than the server's: CLC, opencode, Claude Code and Codex.
fn register_reordered(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.usage_source(UsageSource {
    name: CLC,
    id_prefixes: &["charlie-code-", "clc-"],
    run_logs_only: true,
    implementation: None,
  })?;
  registry.attribute_backend_type("charlie-code", CLC)?;
  registry.usage_source(UsageSource {
    name: OC,
    id_prefixes: &["opencode-", "oc-"],
    run_logs_only: false,
    implementation: None,
  })?;
  registry.attribute_backend_type("opencode", OC)?;
  registry.usage_source(UsageSource {
    name: CC,
    id_prefixes: &["claude-"],
    run_logs_only: false,
    implementation: None,
  })?;
  registry.attribute_backend_type("cc-claude", CC)?;
  registry.usage_source(UsageSource {
    name: CODEX,
    id_prefixes: &["codex-"],
    run_logs_only: false,
    implementation: None,
  })?;
  registry.attribute_backend_type("codex", CODEX)?;
  Ok(())
}

#[tokio::test]
async fn page_lists_seeded_models_with_output_totals() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  seed_ledger(home.path(), true);
  let body = page_body(&features, &read_page_ledger(&features)).await;
  let outputs: BTreeSet<(String, i64)> = data_rows(&body)
    .iter()
    .map(|row| (row["model"].as_str().unwrap().to_string(), row["output"].as_i64().unwrap()))
    .collect();
  let pairs: [(&str, i64); 4] = [("claude-sonnet-4", 100), ("gpt-5", 7200), ("o3", 300), ("claude-haiku-4", 400)];
  let expected: BTreeSet<(String, i64)> =
    pairs.into_iter().map(|(model, output)| (model.to_string(), output)).collect();
  assert_eq!(outputs, expected);
}

#[tokio::test]
async fn an_empty_ledger_renders_an_empty_state_without_the_charts() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let body = page_body(&features, &read_page_ledger(&features)).await;
  assert!(body.contains("id=\"usage-empty\""), "the page marks its empty state");
  assert!(!body.contains("id=\"c_week\""), "no chart renders");
  assert!(body.contains("id=\"tbl_week\"") && body.contains("id=\"tbl_all\""), "the tables keep their skeletons");
  assert!(!body.contains("Scale by platform:"), "the platform sentence needs a source with tokens");
  assert!(data_rows(&body).is_empty(), "the payload carries no rows");
}

#[tokio::test]
async fn counted_fallback_row_carries_a_lower_bound_and_native_rows_do_not() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  seed_ledger(home.path(), true);
  let body = page_body(&features, &read_page_ledger(&features)).await;
  let rows = by_model(&data_rows(&body));
  assert_eq!(rows["gpt-5"]["fallback_output"], json!(7000));
  for model in ["claude-sonnet-4", "o3", "claude-haiku-4"] {
    assert_eq!(rows[model]["fallback_output"], json!(0), "{model}");
  }
}

#[tokio::test]
async fn each_source_native_start_appears() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  seed_ledger(home.path(), true);
  let body = page_body(&features, &read_page_ledger(&features)).await;
  let (_, retention) = body.split_once("<li><b>Retention:</b>").expect("the page names its retention");
  let (retention, _) = retention.split_once("</li>").expect("the retention item closes");
  for (source, date) in [(CC, &CC_TS[..10]), (CODEX, &CODEX_TS[..10]), (OC, &OC_TS[..10]), (CLC, &CB_TS[..10])] {
    assert!(retention.contains(source), "{source} in {retention}");
    assert!(retention.contains(date), "{date} in {retention}");
  }
}

#[test]
fn spellings_of_one_model_merge_into_one_row() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![
    row(OC, "zai-org/GLM-5.3-Flash", [20, 0, 0, 0, 10], vec![account("a", 3, 10, 30)]),
    row(CB, "GLM-5.3-Flash", [0, 0, 0, 0, 10], vec![account("charlie-code-b", 1, 10, 10)]),
  ];
  let context = context_of(&features, rows);
  let payload = payload_rows(&context);
  let [glm] = payload.as_slice() else { panic!("two spellings make one row: {payload:?}") };
  assert_eq!(glm["model"], json!("GLM-5.3-Flash"));
  assert_eq!(glm["total"], json!(40));
  assert_eq!(names(glm), ["opencode · a", "CLC · charlie-code-b"]);
  let totals: Vec<i64> = slots_and_totals(glm).iter().map(|(_, total)| *total).collect();
  assert_eq!(totals, [30, 10]);
  let slots: Vec<i64> = slots_and_totals(glm).iter().map(|(slot, _)| *slot).collect();
  assert_eq!(slots, [3, 5]);
  assert_eq!(totals.iter().sum::<i64>(), glm["total"].as_i64().unwrap());
}

#[test]
fn canonical_name_merges_spellings_but_keeps_versions_apart() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![
    row(OC, "moonshotai/Kimi-K3", [0, 0, 0, 0, 5], vec![]),
    row(OC, "Kimi-K3 (amd-kimi-k3)", [0, 0, 0, 0, 7], vec![]),
    row(CODEX, "GLM-5.3-flash", [0, 0, 0, 0, 3], vec![]),
    row(OC, "zai-org/GLM-5.3-Flash", [0, 0, 0, 0, 4], vec![]),
    row(CC, "claude-fable-5", [0, 0, 0, 0, 9], vec![]),
    row(CC, "claude-fable-5-1", [0, 0, 0, 0, 11], vec![]),
  ];
  let mut got: Vec<(String, i64)> = payload_rows(&context_of(&features, rows))
    .iter()
    .map(|row| (row["model"].as_str().unwrap().to_string(), row["output"].as_i64().unwrap()))
    .collect();
  got.sort();
  let pairs: [(&str, i64); 4] =
    [("GLM-5.3-Flash", 7), ("Kimi-K3", 12), ("claude-fable-5", 9), ("claude-fable-5-1", 11)];
  let expected = pairs.map(|(model, output)| (model.to_string(), output));
  assert_eq!(got, expected);
}

#[test]
fn per_source_tiles_count_attributed_accounts() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![
    row(OC, "moonshotai/Kimi-K3", [5, 0, 0, 0, 0], vec![account("a", 1, 0, 5)]),
    row(OC, "Kimi-K3 (amd-kimi-k3)", [0, 0, 0, 0, 7], vec![account("a", 1, 7, 7)]),
    row(OC, "zai-org/GLM-5.3-Flash", [0, 0, 0, 0, 4], vec![account("a", 1, 4, 4)]),
    row(CB, "GLM-5.3-Flash", [0, 0, 0, 0, 3], vec![account("charlie-code-x", 1, 2, 2), account("codex-y", 1, 1, 1)]),
  ];
  let context = context_of(&features, rows);
  let ctx = &context["ctx"];
  assert_eq!(ctx["per_src"][OC]["models"], json!(2));
  assert_eq!(ctx["per_src"][OC]["total"], json!(16));
  assert_eq!(ctx["per_src"][CLC]["t_comp"], json!(compact(Count::Int(2))));
  assert_eq!(ctx["per_src"][CLC]["models"], json!(1));
  assert_eq!(ctx["per_src"][CODEX]["total"], json!(1));
  assert_eq!(ctx["per_src"][CC]["total"], json!(0));
}

#[test]
fn in_unsplit_carries_into_the_payload_row() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![row(CC, "claude-sonnet-4", [7, 3, 11, 5, 4], vec![])];
  let context = context_of(&features, rows);
  let payload = payload_rows(&context);
  assert_eq!(payload[0]["in_unsplit"], json!(5));
  assert_eq!(payload[0]["total"], json!(30));
}

#[tokio::test]
async fn rendered_page_has_one_row_per_canonical_model() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![
    row(OC, "zai-org/GLM-5.3-Flash", [20, 0, 0, 0, 10], vec![account("a", 3, 10, 30)]),
    row(CB, "GLM-5.3-Flash", [0, 0, 0, 0, 10], vec![account("charlie-code-b", 1, 10, 10)]),
    row(CC, "claude-sonnet-4", [0, 0, 0, 0, 100], vec![]),
  ];
  let body = page_body(&features, &read_of(&features, rows, &[], None)).await;
  let data = data_rows(&body);
  let totals: Vec<(String, i64)> =
    data.iter().map(|row| (row["model"].as_str().unwrap().to_string(), row["total"].as_i64().unwrap())).collect();
  assert_eq!(totals, [("claude-sonnet-4".to_string(), 100), ("GLM-5.3-Flash".to_string(), 40)]);
  let glm = &data[1];
  assert_eq!(names(glm), ["opencode · a", "CLC · charlie-code-b"]);
  let segment_totals: Vec<i64> = slots_and_totals(glm).iter().map(|(_, total)| *total).collect();
  assert_eq!(segment_totals, [30, 10]);
}

#[test]
fn charlie_bot_native_accounts_read_as_clc() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![row(CB, "glm-z", [0, 0, 0, 100, 40], vec![account("charlie-code-x", 2, 40, 140)])];
  let context = token_usage_context(&read_of(&features, rows, &[(CB, "2026-01-01")], None), &features.registry)
    .expect("the context builds");
  let payload = payload_rows(&context);
  assert_eq!(slots_and_totals(&payload[0]), [(5, 140)]);
  assert_eq!(names(&payload[0]), ["CLC · charlie-code-x"]);
  assert_eq!(context["ctx"]["per_src"][CLC]["t_comp"], json!(compact(Count::Int(140))));
  assert_eq!(context["ctx"]["per_src"][CLC]["native_start"], json!("2026-01-01"));
}

#[test]
fn charlie_bot_antigravity_accounts_read_as_antigravity() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![row(CB, "gem-2-5", [0, 0, 0, 0, 40], vec![account("agy-antigravity", 2, 40, 140)])];
  let context = token_usage_context(&read_of(&features, rows, &[(CB, "2026-01-01")], None), &features.registry)
    .expect("the context builds");
  let payload = payload_rows(&context);
  assert_eq!(slots_and_totals(&payload[0]), [(4, 140)]);
  assert_eq!(names(&payload[0]), ["Antigravity · agy-antigravity"]);
  assert_eq!(context["ctx"]["per_src"][AGY]["t_comp"], json!(compact(Count::Int(140))));
  assert_eq!(context["ctx"]["per_src"][AGY]["native_start"], json!("2026-01-01"));
}

#[tokio::test]
async fn page_builds_with_a_ledger_row_attributed_to_antigravity() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  seed_ledger(home.path(), true);
  add_records(
    home.path(),
    "/logs/agy.jsonl",
    "sig-agy",
    &[record("agy-1", CB, "gem-2-5", CB_TS, 50, "agy-antigravity")],
  );
  let body = page_body(&features, &read_page_ledger(&features)).await;
  let rows = by_model(&data_rows(&body));
  assert_eq!(rows["gem-2-5"]["output"], json!(50));
  assert_eq!(slots_and_totals(&rows["gem-2-5"]).iter().map(|(slot, _)| *slot).collect::<Vec<_>>(), [4]);
  assert_eq!(names(&rows["gem-2-5"]), ["Antigravity · agy-antigravity"]);
}

#[test]
fn charlie_bot_fallback_accounts_join_their_cli_s_source() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![
    row(CODEX, "gpt-z", [0, 0, 0, 0, 30], vec![account("work", 3, 30, 30)]),
    with_fallback(row(CB, "gpt-z", [0, 0, 0, 0, 10], vec![account("codex-y", 1, 10, 10)]), 10),
  ];
  let context = context_of(&features, rows);
  let payload = payload_rows(&context);
  assert_eq!(payload.len(), 1);
  assert_eq!(payload[0]["total"], json!(40));
  assert_eq!(slots_and_totals(&payload[0]), [(2, 40)]);
  assert_eq!(names(&payload[0]), ["Codex · work", "Codex · codex-y (fallback)"]);
  let accounts = payload[0]["accounts"].as_array().unwrap();
  assert_eq!(accounts[0]["total"], json!(30));
  assert_eq!(accounts[1]["total"], json!(10));
  assert_eq!(context["ctx"]["per_src"][CODEX]["total"], json!(40));
  assert_eq!(context["ctx"]["per_src"][CLC]["total"], json!(0));
}

#[test]
fn every_tile_total_sums_to_the_page_total() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![
    row(CC, "claude-sonnet-4", [7, 0, 0, 0, 4], vec![account("acct-a", 1, 4, 11)]),
    row(CODEX, "gpt-z", [0, 0, 0, 0, 30], vec![account("work", 3, 30, 30)]),
    row(CB, "gpt-z", [0, 0, 0, 0, 10], vec![account("codex-y", 1, 10, 10)]),
    row(CB, "glm-z", [0, 0, 0, 100, 40], vec![account("charlie-code-x", 2, 40, 140)]),
    row(OC, "o3", [0, 0, 0, 0, 300], vec![account("acct-a", 3, 300, 300)]),
  ];
  let page_total = rows.iter().fold(Count::Int(0), |sum, row| sum.add(row.total, "test page total").unwrap());
  let context = context_of(&features, rows);
  let per_src = &context["ctx"]["per_src"];
  let tiles: f64 = [CC, CODEX, OC, AGY, CLC].iter().map(|source| per_src[source]["total"].as_f64().unwrap()).sum();
  assert_eq!(Count::Real(tiles).compare(page_total), std::cmp::Ordering::Equal);
  assert_eq!(per_src[AGY]["total"], json!(0));
}

#[tokio::test]
async fn page_legend_names_the_cli_sources_without_charlie_bot() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  seed_ledger(home.path(), true);
  let body = page_body(&features, &read_page_ledger(&features)).await;
  let start = body.find("const LEG = ").expect("the page carries a legend") + "const LEG = ".len();
  let end = start + body[start..].find(']').expect("the legend list closes") + 1;
  let legend: Value = serde_json::from_str(&body[start..end]).expect("the legend is JSON");
  assert_eq!(legend, json!([CC, CODEX, OC, AGY, CLC]));
  assert!(!body.contains("charlie-bot"), "the page names no charlie-bot source");
}

#[tokio::test]
async fn cards_follow_registration_order_with_run_log_sources_last() {
  let home = tempfile::tempdir().expect("a temporary home");
  let mut reordered = Registry::new();
  reordered.register_package("test.usage_sources", register_reordered).expect("the sources register");
  let features = page_features(home.path(), reordered);
  seed_ledger(home.path(), true);
  let body = page_body(&features, &read_page_ledger(&features)).await;
  let start = body.find("const LEG = ").expect("the page carries a legend") + "const LEG = ".len();
  let end = start + body[start..].find(']').expect("the legend list closes") + 1;
  let legend: Value = serde_json::from_str(&body[start..end]).expect("the legend is JSON");
  assert_eq!(legend, json!([OC, CC, CODEX, CLC]));
  let rows = by_model(&data_rows(&body));
  assert_eq!(slots_and_totals(&rows["claude-sonnet-4"]).iter().map(|(slot, _)| *slot).collect::<Vec<_>>(), [2]);
  assert_eq!(slots_and_totals(&rows["o3"]).iter().map(|(slot, _)| *slot).collect::<Vec<_>>(), [1]);
}

#[tokio::test]
async fn page_says_when_the_ledger_was_last_captured() {
  let captured_home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(captured_home.path(), registry());
  seed_ledger(captured_home.path(), true);
  let body = page_body(&features, &read_page_ledger(&features)).await;
  let today = &pacific::stamp(Timestamp::now()).expect("the stamp formats")[..10];
  assert!(body.contains(&format!("as of {today}")), "the page names its capture date");
  assert!(!body.contains("no capture yet"));

  let fresh_home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(fresh_home.path(), registry());
  seed_ledger(fresh_home.path(), false);
  let body = page_body(&features, &read_page_ledger(&features)).await;
  assert!(body.contains("no capture yet"));
  assert!(!body.contains("as of 20"));
}

fn at(text: &str) -> Timestamp {
  text.parse().expect("the instant parses")
}

#[test]
fn stamps_read_in_pacific_time_whatever_the_server_zone() {
  assert_eq!(pacific::stamp(at("2026-10-26T07:00:00Z")).expect("the stamp formats"), "2026-10-26 00:00 PDT");
  assert_eq!(pacific::stamp(at("2027-01-15T08:00:00Z")).expect("the stamp formats"), "2027-01-15 00:00 PST");
}

#[test]
fn ledger_stamps_name_their_pacific_day_and_the_week_holds_them() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  // 06:30Z on 10-26 is still Sunday 10-25 in Los Angeles, the last day of last week.
  add_records(
    home.path(),
    "/logs/cc.jsonl",
    "sig-cc",
    &[record("cc-edge", CC, "claude-sonnet-4", "2026-10-26T06:30:00.123Z", 100, "acct-a")],
  );
  let read = read_ledger_at(&features, at("2026-10-30T20:00:00Z")).expect("the ledger reads");
  assert_eq!((read.rows[0].first.as_str(), read.rows[0].last.as_str()), ("2026-10-25", "2026-10-25"));
  assert_eq!(read.native_starts[CC], "2026-10-25");
  assert_eq!(read.week_rows.len(), 1);
  assert_eq!(read.week_rows[0].output, Count::Int(100));
}

#[test]
fn last_week_reads_the_same_rows_on_the_table_and_the_aggregate() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  add_records(
    home.path(),
    "/logs/cc.jsonl",
    "sig-cc",
    &[
      record("before", CC, "claude-sonnet-4", "2026-10-19T06:59:59Z", 20, "acct-a"),
      record("start", CC, "claude-sonnet-4", "2026-10-19T07:00:00Z", 10, "acct-a"),
      record("end", CC, "claude-sonnet-4", "2026-10-26T07:00:00Z", 40, "acct-a"),
    ],
  );
  let now = at("2026-10-30T20:00:00Z");
  // The first read backfills the aggregate, so it takes the table pass; the second takes the aggregate.
  let table = read_ledger_at(&features, now).expect("the ledger reads");
  let aggregate = read_ledger_at(&features, now).expect("the ledger reads");
  assert_eq!(table.week_rows[0].output, Count::Int(10), "the window starts at its first instant");
  assert_eq!(table.previous_week_rows[0].output, Count::Int(20), "the instant before it is the week before");
  assert_eq!(table.rows[0].output, Count::Int(70), "the all-time rows keep the instant after the window");
  assert_eq!(table.week_rows, aggregate.week_rows);
  assert_eq!(table.previous_week_rows, aggregate.previous_week_rows);
  assert_eq!(table.rows, aggregate.rows);
}

#[test]
fn rows_rank_by_output_then_model_not_by_total() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![
    row(CC, "claude-big-total", [1_000, 0, 0, 0, 1], vec![]),
    row(OC, "o-output", [0, 0, 0, 0, 50], vec![]),
    row(CODEX, "c-tie", [0, 0, 0, 0, 50], vec![]),
  ];
  let context = context_of(&features, rows);
  let payload = payload_rows(&context);
  let models: Vec<&str> = payload.iter().map(|row| row["model"].as_str().unwrap()).collect();
  assert_eq!(models, ["c-tie", "o-output", "claude-big-total"]);
}

#[test]
fn an_account_no_source_attributes_fails_the_context() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let rows = vec![row(CB, "glm-z", [0, 0, 0, 0, 40], vec![account("mystery-7", 1, 40, 40)])];
  let error = token_usage_context(&read_of(&features, rows, &[], None), &features.registry)
    .expect_err("no source attributes the account");
  assert!(error.to_string().contains("mystery-7"), "{error}");
}

#[test]
fn the_window_spans_the_first_and_last_stamps() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  let mut early = row(CC, "claude-sonnet-4", [0, 0, 0, 0, 4], vec![]);
  early.first = "2026-01-05".to_string();
  early.last = "2026-01-20".to_string();
  let mut late = row(OC, "o3", [0, 0, 0, 0, 9], vec![]);
  late.first = "2026-02-01".to_string();
  late.last = "2026-02-09".to_string();
  let context = context_of(&features, vec![early, late]);
  assert_eq!(context["ctx"]["rows"][0]["window"], json!("2026-02-01 → 2026-02-09"));
  assert_eq!(context["window"], json!(["2026-01-05", "2026-02-09"]));
}

#[tokio::test]
async fn the_route_answers_get_and_refuses_other_methods() {
  let home = tempfile::tempdir().expect("a temporary home");
  let features = page_features(home.path(), registry());
  seed_ledger(home.path(), true);
  for (method, status) in [
    (Method::GET, StatusCode::OK),
    (Method::HEAD, StatusCode::METHOD_NOT_ALLOWED),
    (Method::POST, StatusCode::METHOD_NOT_ALLOWED),
  ] {
    let response = routes(&features)
      .oneshot(Request::builder().method(method.clone()).uri("/token-usage").body(Body::empty()).unwrap())
      .await
      .unwrap();
    assert_eq!(response.status(), status, "{method}");
  }
}

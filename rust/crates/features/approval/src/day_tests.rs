//! The day view's tests. Two sessions are seeded with items of each tier and rule, and digest entries come
//! through the core store. A `policy_table_changed` record is the one the table edit appends.

use std::fs;

use axum::http::Method;
use charliebot_core::pacific::{day_of, zone};
use charliebot_store::metadata::UtcDateTime;
use jiff::civil::Date;
use jiff::{Span, Timestamp};
use serde_json::{Value, json};

use charliebot_registry::registry::OpenWaitingRow;

use super::{age_text, row_json};
use crate::test_support::Home;

/// The PT day the seeds fall in: the stamps below name UTC instants whose Pacific day is 2026-10-30 (PDT, UTC-7).
const DAY: &str = "2026-10-30";
/// The previous Pacific day, whose user-act item stays off the day's sheet.
const PREVIOUS_DAY: &str = "2026-10-29";

/// Noon in Pacific time on `date`, as a stored stamp reads.
fn pt_noon(date: Date) -> String {
  date.at(12, 0, 0, 0).to_zoned(zone().expect("the zone loads")).expect("the noon resolves").timestamp().to_string()
}

fn item(text: &str, tier: Value, rule: Value, since: &str) -> Value {
  json!({"text": text, "context": "Why the item waits.", "link": null, "since": since, "since_exact": true, "tier": tier, "rule": rule})
}

fn open_waiting_row(tier: Option<&str>, is_derived: bool, since: &str) -> OpenWaitingRow {
  OpenWaitingRow {
    session_id: "session".to_string(),
    session_name: "Session".to_string(),
    text: "Review the step".to_string(),
    context: None,
    link: None,
    tier: tier.map(ToString::to_string),
    rule: None,
    since: UtcDateTime::parse(since).expect("the fixture time parses"),
    is_derived,
  }
}

#[test]
fn needs_recheck_only_marks_untiered_or_over_fourteen_day_stored_rows() {
  let now: Timestamp = "2026-11-12T00:00:00Z".parse().expect("the fixed time parses");
  let untiered = row_json(&open_waiting_row(None, false, "2026-11-11T23:00:00Z"), now);
  let exact_fourteen_days = row_json(&open_waiting_row(Some("t2"), false, "2026-10-29T00:00:00Z"), now);
  let over_fourteen_days = row_json(&open_waiting_row(Some("t2"), false, "2026-10-28T23:59:59Z"), now);
  let old_derived = row_json(&open_waiting_row(Some("user-act"), true, "2026-10-01T00:00:00Z"), now);

  assert_eq!(untiered["needs_recheck"], json!(true));
  assert_eq!(exact_fourteen_days["needs_recheck"], json!(false));
  assert_eq!(over_fourteen_days["needs_recheck"], json!(true));
  assert_eq!(old_derived["needs_recheck"], json!(false));
}

#[test]
fn stored_t0_t1_and_t2_rows_share_the_decision_display_kind() {
  let now: Timestamp = "2026-11-12T00:00:00Z".parse().expect("the fixed time parses");
  for tier in ["t0", "t1", "t2"] {
    let row = row_json(&open_waiting_row(Some(tier), false, "2026-11-11T23:00:00Z"), now);
    assert_eq!(row["tier_label"], json!("decision"), "tier {tier}");
  }
  assert_eq!(
    row_json(&open_waiting_row(Some("user-act"), false, "2026-11-11T23:00:00Z"), now)["tier_label"],
    json!("user-act")
  );
}

/// Seeds the two sessions and the digest. Session names order the rows' session fields, not the queue.
async fn seed(home: &Home) -> (String, String) {
  let first = home.manager("Alpha").await;
  let second = home.manager("Beta").await;
  home
    .set_web_status(
      &first,
      json!({
        "now": "Running",
        "waiting": [
          item("Delete the backup volume", json!("t0"), json!("irreversible"), "2026-10-30T16:00:00Z"),
          item("Land on the shared branch", json!("t0"), json!("out-of-scope:shared-repo-landing"), "2026-10-30T15:00:00Z"),
          item("Log into the console", json!("user-act"), json!(null), "2026-10-30T14:00:00Z"),
          item("Reply in the own thread", json!("t1"), json!("release-restart"), "2026-10-30T13:00:00Z"),
          item("Pick the layout", json!("t2"), json!(null), "2026-10-30T12:00:00Z"),
          item("Edit the approval rule text", json!("t0"), json!("rule-named"), "2026-10-30T10:00:00Z"),
        ],
        "updated_at": "2026-10-30T16:30:00Z",
      }),
    )
    .await;
  home
    .set_web_status(
      &second,
      json!({
        "now": "Running",
        "waiting": [
          item("Spend over the budget", json!("t0"), json!("money"), "2026-10-30T15:30:00Z"),
          item("Review the delivery", json!("review"), json!(null), "2026-10-30T15:20:00Z"),
          item("Grant the API key", json!("t0"), json!("authority"), "2026-10-30T15:10:00Z"),
          item("Draft the standing approval", json!("t0"), json!("rule-named"), "2026-10-30T09:00:00Z"),
          item("Log into the other console", json!("user-act"), json!(null), "2026-10-29T20:00:00Z"),
          item("Legacy untiered item", json!(null), json!(null), "2026-10-30T08:00:00Z"),
        ],
        "updated_at": "2026-10-30T16:30:00Z",
      }),
    )
    .await;
  (first, second)
}

/// One T1 use and one T2 decision on `DAY`, one T1 use on the previous day.
fn seed_digest(home: &Home) {
  let root = home.root();
  for (policy, scope_check, now) in [
    ("shared-repo-landing", Some("Only Chao uses this repo."), "2026-10-30T18:30:00+00:00"),
    ("t2", None, "2026-10-30T19:30:00+00:00"),
    ("shared-repo-landing", Some("Only Chao uses this repo."), "2026-10-29T20:30:00+00:00"),
  ] {
    charliebot_core::approval_digest::add(
      &root,
      charliebot_core::approval_digest::DigestCall {
        session: "seed-session",
        policy,
        step: "The recorded step.",
        undo: "Revert the step.",
        scope_check,
        link: Some("https://example.com/step"),
        now,
      },
    )
    .expect("the digest entry records");
  }
}

/// One `policy_table_changed` record, appended by a table edit read back through the server-side loader.
fn seed_table_change(home: &Home) -> String {
  let root = home.root();
  let path = root.join(charliebot_core::approval_policy::TABLE_RELATIVE_PATH);
  let original = fs::read_to_string(&path).expect("the table reads");
  charliebot_core::approval_policy::load_and_record(&root).expect("the baseline records");
  fs::write(&path, format!("{original}# a comment changes the content\n")).expect("the edit writes");
  charliebot_core::approval_policy::load_and_record(&root).expect("the change records");
  let records = charliebot_core::approval_policy::change_records(&root).expect("the records read");
  assert_eq!(records.len(), 1);
  records[0]["time"].as_str().expect("the record carries its time").to_string()
}

#[tokio::test]
async fn the_queue_orders_t0_by_rule_rank_then_review_then_the_rest() {
  let home = Home::new();
  seed(&home).await;
  seed_digest(&home);

  let (status, body) = home.send(Method::GET, &format!("/api/approval/day?date={DAY}")).await;

  assert_eq!(status, 200, "{body}");
  assert_eq!(body["date"], json!(DAY));
  let texts = body["queue"].as_array().unwrap().iter().map(|row| row["text"].as_str().unwrap()).collect::<Vec<_>>();
  assert_eq!(
    texts,
    [
      // rank 1, irreversible.
      "Delete the backup volume",
      // rank 2: an out-of-scope rule ranks with the chao-name test.
      "Land on the shared branch",
      // rank 3, money.
      "Spend over the budget",
      // rank 4 items in arrival order, then the same rank's authority use.
      "Draft the standing approval",
      "Edit the approval rule text",
      "Grant the API key",
      // review items follow the T0 items.
      "Review the delivery",
      // t1, t2, untiered and user-act items follow, each marked with its tier, in arrival order.
      "Log into the other console",
      "Legacy untiered item",
      "Pick the layout",
      "Reply in the own thread",
      "Log into the console",
    ]
  );
  let tiers = body["queue"].as_array().unwrap().iter().map(|row| row["tier"].clone()).collect::<Vec<_>>();
  assert_eq!(
    tiers,
    [
      json!("t0"),
      json!("t0"),
      json!("t0"),
      json!("t0"),
      json!("t0"),
      json!("t0"),
      json!("review"),
      json!("user-act"),
      json!(null),
      json!("t2"),
      json!("t1"),
      json!("user-act"),
    ]
  );
  // Every open item appears once: the queue holds all twelve seeded items.
  assert_eq!(body["queue"].as_array().unwrap().len(), 12);
  // The T0 rows carry their rule's rank, and each row names its session.
  let first = &body["queue"][0];
  assert_eq!(first["rank"], json!(1));
  assert_eq!(first["session_name"], json!("Alpha"));
  assert!(first["session_id"].as_str().is_some_and(|id| !id.is_empty()));
}

#[tokio::test]
async fn today_s_actions_only_you_can_do_holds_every_open_user_act_item() {
  let home = Home::new();
  let session = home.manager("Alpha").await;
  let today = day_of(Timestamp::now()).expect("today reads");
  let earlier = today.checked_sub(Span::new().days(2)).expect("the earlier day resolves");
  home
    .set_web_status(
      &session,
      json!({
        "now": "Running",
        "waiting": [
          // Judged two PT days ago and still open: today's sheet carries it.
          item("Log into the console", json!("user-act"), json!(null), &pt_noon(earlier)),
          item("Log into the admin console", json!("user-act"), json!(null), &pt_noon(today)),
          item("Approve the deploy", json!("t0"), json!("money"), &pt_noon(today)),
        ],
        "updated_at": pt_noon(today),
      }),
    )
    .await;

  let (status, body) = home.send(Method::GET, &format!("/api/approval/day?date={today}")).await;

  assert_eq!(status, 200, "{body}");
  let texts = body["actions_only_you_can_do"]
    .as_array()
    .unwrap()
    .iter()
    .map(|row| row["text"].as_str().unwrap())
    .collect::<Vec<_>>();
  assert_eq!(texts, ["Log into the console", "Log into the admin console"]);
}

#[tokio::test]
async fn a_past_day_s_actions_only_you_can_do_holds_that_day_s_user_act_items() {
  let home = Home::new();
  let session = home.manager("Alpha").await;
  let today = day_of(Timestamp::now()).expect("today reads");
  let earlier = today.checked_sub(Span::new().days(2)).expect("the earlier day resolves");
  home
    .set_web_status(
      &session,
      json!({
        "now": "Running",
        "waiting": [
          item("Log into the console", json!("user-act"), json!(null), &pt_noon(earlier)),
          item("Log into the admin console", json!("user-act"), json!(null), &pt_noon(today)),
        ],
        "updated_at": pt_noon(today),
      }),
    )
    .await;

  let (status, body) = home.send(Method::GET, &format!("/api/approval/day?date={earlier}")).await;

  assert_eq!(status, 200, "{body}");
  let texts = body["actions_only_you_can_do"]
    .as_array()
    .unwrap()
    .iter()
    .map(|row| row["text"].as_str().unwrap())
    .collect::<Vec<_>>();
  assert_eq!(texts, ["Log into the console"]);
}

/// The texts of a day body's actions only you can do, sorted, since the reader orders sessions by listing.
fn action_texts(body: &Value) -> Vec<&str> {
  let mut texts: Vec<&str> =
    body["actions_only_you_can_do"].as_array().unwrap().iter().map(|row| row["text"].as_str().unwrap()).collect();
  texts.sort();
  texts
}

#[tokio::test]
async fn a_session_scope_lists_its_tree_s_user_act_items_and_the_host_lists_all() {
  let home = Home::new();
  let root = home.manager("Alpha").await;
  let child = home.child(&root, "Beta").await;
  let grandchild = home.child(&child, "Gamma").await;
  let unrelated = home.manager("Delta").await;
  let today = day_of(Timestamp::now()).expect("today reads");
  let stamp = pt_noon(today);
  home
    .set_web_status(
      &root,
      json!({
        "now": "Running",
        "waiting": [
          item("Log into the console", json!("user-act"), json!(null), &stamp),
          item("Approve the deploy", json!("t0"), json!("money"), &stamp),
        ],
        "updated_at": stamp,
      }),
    )
    .await;
  home
    .set_web_status(
      &grandchild,
      json!({
        "now": "Running",
        "waiting": [item("Log into the admin console", json!("user-act"), json!(null), &stamp)],
        "updated_at": stamp,
      }),
    )
    .await;
  home
    .set_web_status(
      &unrelated,
      json!({
        "now": "Running",
        "waiting": [item("Log into the other console", json!("user-act"), json!(null), &stamp)],
        "updated_at": stamp,
      }),
    )
    .await;

  let (status, body) = home.send(Method::GET, &format!("/api/approval/day?date={today}&session={root}")).await;
  assert_eq!(status, 200, "{body}");
  assert_eq!(action_texts(&body), ["Log into the admin console", "Log into the console"]);

  let (status, body) = home.send(Method::GET, &format!("/api/approval/day?date={today}")).await;
  assert_eq!(status, 200, "{body}");
  assert_eq!(action_texts(&body), ["Log into the admin console", "Log into the console", "Log into the other console"]);

  let (status, body) = home.send(Method::GET, &format!("/api/approval/day?date={today}&session=no-such-session")).await;
  assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn the_digest_holds_the_day_s_entries_and_the_day_s_table_change() {
  let home = Home::new();
  seed(&home).await;
  let change_time = seed_table_change(&home);
  let change_day = charliebot_core::pacific::day_of_stamp(&change_time).expect("the record's stamp reads").to_string();
  // A use recorded at the change's own stamp, so the change's day holds both kinds of record whatever the date.
  charliebot_core::approval_digest::add(
    &home.root(),
    charliebot_core::approval_digest::DigestCall {
      session: "seed-session",
      policy: "t2",
      step: "The recorded step.",
      undo: "Revert the step.",
      scope_check: None,
      link: Some("https://example.com/step"),
      now: &change_time,
    },
  )
  .expect("the digest entry records");

  let (status, body) = home.send(Method::GET, &format!("/api/approval/day?date={change_day}")).await;

  assert_eq!(status, 200, "{body}");
  let rows = body["digest"].as_array().unwrap();
  let kinds = rows.iter().map(|row| row["type"].as_str().unwrap()).collect::<Vec<_>>();
  assert!(kinds.contains(&"digest_entry"), "{rows:?}");
  assert!(kinds.contains(&"policy_table_changed"), "{rows:?}");
  // The digest rows read in stamp order, so the two kinds interleave by time.
  let times = rows.iter().map(|row| row["time"].as_str().unwrap()).collect::<Vec<_>>();
  let mut sorted = times.clone();
  sorted.sort();
  assert_eq!(times, sorted);
  // Each digest entry carries its undo, and the change record carries the diff that reverts it.
  for row in rows.iter().filter(|row| row["type"] == "digest_entry") {
    assert_eq!(row["undo"], json!("Revert the step."), "{row}");
    assert_eq!(row["link"], json!("https://example.com/step"), "{row}");
  }
  let change = rows.iter().find(|row| row["type"] == "policy_table_changed").unwrap();
  assert_eq!(change["time"], json!(change_time));
  assert!(change["diff"].as_str().unwrap().contains("+ # a comment changes the content"), "{change}");
  // The day the change did not fall in lists no digest rows from it.
  let other_day = if change_day == DAY { PREVIOUS_DAY } else { DAY };
  let (status, other) = home.send(Method::GET, &format!("/api/approval/day?date={other_day}")).await;
  assert_eq!(status, 200, "{other}");
  assert!(!other["digest"].as_array().unwrap().iter().any(|row| row["type"] == "policy_table_changed"), "{other}");
}

#[tokio::test]
async fn the_day_s_digest_counts_each_use_of_one_policy() {
  let home = Home::new();
  seed(&home).await;
  seed_digest(&home);
  // Fill the capped policy's day with 15 more uses (16 land on DAY together with the seeded one).
  let path = charliebot_core::approval_digest::digest_path(&home.root());
  let entries = fs::read_to_string(&path).unwrap();
  let first_use = entries.lines().next().unwrap();
  let repeated = format!("{first_use}\n").repeat(15);
  fs::write(&path, format!("{entries}{repeated}")).unwrap();

  let (_, body) = home.send(Method::GET, &format!("/api/approval/day?date={DAY}")).await;

  let uses = body["digest"]
    .as_array()
    .unwrap()
    .iter()
    .filter(|row| row["type"] == "digest_entry" && row["policy"] == "shared-repo-landing")
    .count();
  assert_eq!(uses, 16, "{body}");
}

#[tokio::test]
async fn a_day_with_no_records_shows_empty_lists() {
  let home = Home::new();

  let (status, body) = home.send(Method::GET, "/api/approval/day?date=2026-01-01").await;

  assert_eq!(status, 200, "{body}");
  assert_eq!(body["queue"], json!([]));
  assert_eq!(body["actions_only_you_can_do"], json!([]));
  assert_eq!(body["digest"], json!([]));
}

#[tokio::test]
async fn a_mistyped_date_refuses_with_the_expected_form() {
  let home = Home::new();

  for query in ["?date=soon", "?date=2026-13-01"] {
    let (status, body) = home.send(Method::GET, &format!("/api/approval/day{query}")).await;
    assert_eq!(status, 422, "{query}: {body}");
    assert!(body["detail"].as_str().unwrap().contains("YYYY-MM-DD"), "{query}: {body}");
  }
}

#[test]
fn the_age_reads_the_span_before_the_clock() {
  let now: Timestamp = "2026-11-02T17:00:00Z".parse().expect("the fixed clock parses");
  let at = |stamp: &str| age_text(UtcDateTime::parse(stamp).expect("the stamp parses"), now);

  assert_eq!(at("2026-11-02T17:00:00Z"), "just now");
  assert_eq!(at("2026-11-02T16:59:00Z"), "1 min ago");
  assert_eq!(at("2026-11-02T16:00:00Z"), "1 h ago");
  assert_eq!(at("2026-11-02T12:00:00Z"), "5 h ago");
  assert_eq!(at("2026-10-30T17:00:00Z"), "3 d ago");
  assert_eq!(at("2026-09-03T17:00:00Z"), "2 mo ago");
  assert_eq!(at("2025-11-02T17:00:00Z"), "1 y ago");
  // A stamp after the clock reads as just now rather than a negative span.
  assert_eq!(at("2026-11-02T17:05:00Z"), "just now");
}

//! The approval digest (`$CHARLIEBOT_HOME/state/digest_entries.jsonl`, plan 4 v2 task W3): one append-only record per
//! T1 use of a standing policy and per T2 decision the agent tree makes, next to the other host-level state records.
//!
//! `charliebot digest add` writes through [`add`], which reads the approval policy table through the W2 loader and
//! refuses an unknown policy, an expired policy, and a use over the policy's `cap_per_pt_day` in the current Pacific
//! day. The approval day view reads a day's entries with [`read_entries`].

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approval_policy;
use crate::pacific;
use crate::paths;
use crate::text_repr::{JsonStyle, json_dumps};

/// The digest's record store, next to the other host-level state records
/// (`approval_digest.DIGEST_RELATIVE_PATH`).
pub const DIGEST_RELATIVE_PATH: &str = "state/digest_entries.jsonl";

/// The pseudo-policy a T2 decision records under (`approval_digest.T2_POLICY`).
pub const T2_POLICY: &str = "t2";

/// `json.dumps(..., ensure_ascii=False)` with the default separators: one NDJSON record line.
const RECORD_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// One digest record: a T1 policy use or a T2 decision, what was done, and how to undo it
/// (`approval_digest.DigestEntry`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DigestEntry {
  /// The UTC instant the entry was written, as `datetime.now(UTC).isoformat()` renders it.
  pub time: String,
  /// The session that made the step.
  pub session: String,
  /// The standing policy id, or `t2` for a decision the agent tree made.
  pub policy: String,
  /// What was done.
  pub step: String,
  /// How to undo it.
  pub undo: String,
  /// Why a T1 use stays inside the policy's scope. A T2 decision carries none.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub scope_check: Option<String>,
  /// A link to the step's evidence, when the caller gave one.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub link: Option<String>,
}

/// A digest read or write that refused (`approval_digest.DigestError`).
#[derive(Debug)]
pub struct DigestError(String);

impl std::fmt::Display for DigestError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl std::error::Error for DigestError {}

/// The digest store's path under `home` (`approval_digest.digest_path`).
pub fn digest_path(home: &Path) -> PathBuf {
  home.join(DIGEST_RELATIVE_PATH)
}

/// The digest entries in the store, in append order (`approval_digest.read_entries`). A blank line is skipped; any
/// other line that does not hold a digest record refuses the read with the store's path and the line number.
pub fn read_entries(home: &Path) -> Result<Vec<DigestEntry>, DigestError> {
  let path = digest_path(home);
  let raw = match fs::read_to_string(&path) {
    Ok(raw) => raw,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(DigestError(format!("{}: {error}", paths::display(&path)))),
  };
  let mut entries = Vec::new();
  for (index, line) in raw.lines().enumerate() {
    if line.trim().is_empty() {
      continue;
    }
    let entry: DigestEntry = serde_json::from_str(line)
      .map_err(|error| DigestError(format!("{} line {}: {}", paths::display(&path), index + 1, error)))?;
    entries.push(entry);
  }
  Ok(entries)
}

/// One digest-add call's inputs (`approval_digest.DigestCall`). `scope_check` carries a T1 use's scope reason; `now`
/// is the instant the entry records, as `datetime.now(UTC).isoformat()` text.
#[derive(Clone, Copy, Debug)]
pub struct DigestCall<'a> {
  pub session: &'a str,
  pub policy: &'a str,
  pub step: &'a str,
  pub undo: &'a str,
  pub scope_check: Option<&'a str>,
  pub link: Option<&'a str>,
  pub now: &'a str,
}

/// Record one digest entry (`approval_digest.add`). The step names its policy: a standing policy id checks the table's
/// scope gates — the policy must exist, must not have expired, and must have cap room in the Pacific day `now` falls
/// in — and requires the caller's `scope_check`; `t2` records a decision without either. Every refusal names the
/// reason and tells the caller to ask for the step as a T0 Waiting on you item instead.
pub fn add(home: &Path, call: DigestCall<'_>) -> Result<DigestEntry, DigestError> {
  let loaded = approval_policy::load(home).map_err(|error| DigestError(error.to_string()))?;
  let DigestCall { session, policy, step, undo, scope_check, link, now } = call;
  if policy != T2_POLICY {
    let standing = loaded
      .table
      .standing_policies
      .iter()
      .find(|standing| standing.id == policy)
      .ok_or_else(|| unknown_policy(policy, &loaded.table))?;
    let today = pacific::day_of_stamp(now).map_err(|error| DigestError(error.to_string()))?;
    let today_text = pacific::date_text(today);
    // ISO dates compare as text the way they compare as dates, so a use on a day after `expires` is a use of an
    // expired policy; the expires day itself still admits uses.
    if today_text.as_str() > standing.expires.as_str() {
      return Err(DigestError(format!(
        "standing policy {policy:?} expired on {}; ask for the step as a T0 Waiting on you item",
        standing.expires
      )));
    }
    if scope_check.is_none() {
      return Err(DigestError(format!(
        "--scope-check is required to record a use of standing policy {policy:?}; say why the step stays inside the \
         policy's scope"
      )));
    }
    if let Some(cap) = standing.cap_per_pt_day {
      let used = read_entries(home)?
        .iter()
        .filter(|entry| entry.policy == policy && day_text_of(&entry.time).as_deref() == Some(today_text.as_str()))
        .count();
      if used >= usize::try_from(cap).map_err(|error| DigestError(format!("cap_per_pt_day {cap}: {error}")))? {
        return Err(DigestError(format!(
          "standing policy {policy:?} reached its cap of {cap} uses in PT day {today_text}; ask for the step as a T0 \
           Waiting on you item"
        )));
      }
    }
  }
  let entry = DigestEntry {
    time: now.to_string(),
    session: session.to_string(),
    policy: policy.to_string(),
    step: step.to_string(),
    undo: undo.to_string(),
    scope_check: scope_check.map(str::to_string),
    link: link.map(str::to_string),
  };
  append(home, &entry)?;
  Ok(entry)
}

/// The refusal for a policy id the table does not name (`approval_digest._unknown_policy`).
fn unknown_policy(policy: &str, table: &approval_policy::ApprovalPolicyTable) -> DigestError {
  let known = table.standing_policies.iter().map(|standing| standing.id.as_str()).collect::<Vec<_>>();
  DigestError(format!(
    "unknown standing policy {policy:?}; the table names {}{}",
    if known.is_empty() { "no standing policies".to_string() } else { known.join(", ") },
    "; ask for the step as a T0 Waiting on you item"
  ))
}

/// The Pacific day a stored stamp falls in, when the stamp is one (`approval_digest._day_text_of`).
fn day_text_of(stamp: &str) -> Option<String> {
  pacific::day_of_stamp(stamp).ok().map(pacific::date_text)
}

/// One stored record of `entry`, `type` first (`approval_digest._record_of`).
pub fn record_of(entry: &DigestEntry) -> Value {
  let mut record = json!({"type": "digest_entry"});
  let fields = serde_json::to_value(entry).expect("a digest entry serializes to JSON");
  let Value::Object(fields) = fields else { unreachable!("a digest entry serializes to an object") };
  for (name, value) in fields {
    record[name.as_str()] = value;
  }
  record
}

/// Append one record line (`approval_digest._append`), the NDJSON-APPEND discipline.
fn append(home: &Path, entry: &DigestEntry) -> Result<(), DigestError> {
  let store = digest_path(home);
  if let Some(parent) = store.parent() {
    fs::create_dir_all(parent).map_err(|error| DigestError(format!("{}: {error}", paths::display(parent))))?;
  }
  let mut line = json_dumps(&record_of(entry), &RECORD_JSON);
  line.push('\n');
  {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(&store)
      .map_err(|error| DigestError(format!("{}: {error}", paths::display(&store))))?;
    file.write_all(line.as_bytes()).map_err(|error| DigestError(format!("{}: {error}", paths::display(&store))))?;
    file.sync_data().map_err(|error| DigestError(format!("{}: {error}", paths::display(&store))))?;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use tempfile::TempDir;

  use super::*;
  use crate::approval_policy::TABLE_RELATIVE_PATH;

  /// The table the digest tests read: one uncapped policy, one capped at 16, both expiring 2099-01-01.
  pub const TABLE: &str = "\
consequence_tests:
  - {id: irreversible, rank: 1, text: \"Deletes data with no backup, or sends what cannot be recalled.\"}
  - {id: chao-name, rank: 2, text: \"A message in Chao's name reaches other people.\"}
  - {id: money, rank: 3, text: \"Spends money beyond a budget that Chao set.\"}
  - {id: authority, rank: 4, text: \"Widens what agents may do.\"}
  - {id: rule-named, rank: 4, text: \"A rule in force names the user's explicit approval for this step.\"}
  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}
standing_policies:
  - {id: release-restart, scope: \"Release and restart.\", cap_per_pt_day: null, expires: 2099-01-01}
  - {id: shared-repo-landing, scope: \"Land on a shared non-main branch.\", cap_per_pt_day: 16, expires: 2099-01-01}
  - {id: own-thread-reply, scope: \"Reply in the session's own thread.\", cap_per_pt_day: null, expires: 2026-10-30}
  - {id: past-policy, scope: \"Expired before the test's day.\", cap_per_pt_day: null, expires: 2026-10-15}
enable_ask_tier_refusal: false
";

  /// The stamp `add` records: PT day 2026-10-30 (PDT, UTC-7).
  const NOW: &str = "2026-10-30T18:30:00+00:00";

  fn home_with_table() -> TempDir {
    let home = tempfile::tempdir().expect("temp home");
    let path = home.path().join(TABLE_RELATIVE_PATH);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, TABLE).unwrap();
    home
  }

  fn call<'a>(policy: &'a str, scope_check: Option<&'a str>) -> DigestCall<'a> {
    DigestCall {
      session: "session-a",
      policy,
      step: "Merged the branch.",
      undo: "Revert the merge commit.",
      scope_check,
      link: None,
      now: NOW,
    }
  }

  fn added(home: &Path, policy: &str, scope_check: Option<&str>) -> Result<DigestEntry, DigestError> {
    add(home, call(policy, scope_check))
  }

  #[test]
  fn a_t1_use_records_its_scope_check_and_a_t2_decision_records_without_one() {
    let home = home_with_table();

    let entry = added(home.path(), "shared-repo-landing", Some("Only Chao uses this repo.")).expect("the use records");

    assert_eq!(entry.time, NOW);
    assert_eq!(entry.session, "session-a");
    assert_eq!(entry.policy, "shared-repo-landing");
    assert_eq!(entry.scope_check.as_deref(), Some("Only Chao uses this repo."));
    let stored = read_entries(home.path()).expect("the store reads");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0], entry);

    let decision = added(home.path(), "t2", None).expect("the decision records without a scope check");
    assert_eq!(decision.policy, "t2");
    assert_eq!(decision.scope_check, None);
  }

  #[test]
  fn an_unknown_policy_is_refused_with_the_reason_and_the_t0_advice() {
    let home = home_with_table();

    let error = added(home.path(), "chaos-policy", Some("Inside nothing.")).expect_err("the use is refused");

    assert!(error.to_string().contains("unknown standing policy \"chaos-policy\""), "{error}");
    assert!(error.to_string().contains("shared-repo-landing"), "{error}");
    assert!(error.to_string().contains("ask for the step as a T0 Waiting on you item"), "{error}");
    assert!(read_entries(home.path()).expect("the store reads").is_empty());
  }

  #[test]
  fn an_expired_policy_is_refused_and_the_expires_day_still_admits_uses() {
    let home = home_with_table();

    let error = added(home.path(), "past-policy", Some("Inside nothing any more."))
      .expect_err("a use after the expiry is refused");
    assert!(error.to_string().contains("expired on 2026-10-15"), "{error}");
    assert!(error.to_string().contains("ask for the step as a T0 Waiting on you item"), "{error}");

    let on_expiry_day = add(
      home.path(),
      DigestCall {
        session: "session-a",
        policy: "own-thread-reply",
        step: "Replied.",
        undo: "Delete the reply.",
        scope_check: Some("The session's own thread."),
        link: None,
        now: NOW,
      },
    )
    .expect("a use on the expires day records");
    assert_eq!(on_expiry_day.policy, "own-thread-reply");
  }

  #[test]
  fn a_t1_use_without_a_scope_check_is_refused() {
    let home = home_with_table();

    let error = added(home.path(), "release-restart", None).expect_err("the use is refused");

    assert!(error.to_string().contains("--scope-check is required"), "{error}");
    assert!(error.to_string().contains("release-restart"), "{error}");
    assert!(read_entries(home.path()).expect("the store reads").is_empty());
  }

  #[test]
  fn the_use_over_the_cap_is_refused_and_the_day_boundary_resets_the_cap() {
    let home = home_with_table();
    for index in 0..16 {
      add(
        home.path(),
        DigestCall {
          session: "session-a",
          policy: "shared-repo-landing",
          step: &format!("Landing {index}."),
          undo: "Revert it.",
          scope_check: Some("Only Chao uses this repo."),
          link: None,
          now: NOW,
        },
      )
      .expect("the use records");
    }

    let error = added(home.path(), "shared-repo-landing", Some("Only Chao uses this repo."))
      .expect_err("the 17th use of the day is refused");
    assert!(error.to_string().contains("cap of 16 uses in PT day 2026-10-30"), "{error}");
    assert!(error.to_string().contains("ask for the step as a T0 Waiting on you item"), "{error}");

    // The next Pacific day starts a new count.
    add(
      home.path(),
      DigestCall {
        session: "session-a",
        policy: "shared-repo-landing",
        step: "Landing 17.",
        undo: "Revert it.",
        scope_check: Some("Only Chao uses this repo."),
        link: None,
        now: "2026-10-31T18:30:00+00:00",
      },
    )
    .expect("the next day's first use records");
    let stored = read_entries(home.path()).expect("the store reads");
    assert_eq!(stored.len(), 17);
    assert_eq!(stored[16].time, "2026-10-31T18:30:00+00:00");
  }

  #[test]
  fn the_cap_counts_only_the_same_policy_uses_of_one_day() {
    let home = home_with_table();
    for index in 0..16 {
      add(
        home.path(),
        DigestCall {
          session: "session-a",
          policy: "shared-repo-landing",
          step: &format!("Landing {index}."),
          undo: "Revert it.",
          scope_check: Some("Only Chao uses this repo."),
          link: None,
          now: NOW,
        },
      )
      .expect("the use records");
    }

    add(
      home.path(),
      DigestCall {
        session: "session-a",
        policy: "release-restart",
        step: "Released.",
        undo: "Switch back.",
        scope_check: Some("CharlieBot's own run."),
        link: None,
        now: NOW,
      },
    )
    .expect("another policy's use records");

    let stored = read_entries(home.path()).expect("the store reads");
    assert_eq!(stored.len(), 17);
    assert_eq!(stored[16].policy, "release-restart");
  }

  #[test]
  fn a_store_read_of_an_absent_file_answers_no_entries_and_a_broken_line_names_the_store() {
    let home = tempfile::tempdir().unwrap();
    assert!(read_entries(home.path()).expect("an absent store reads").is_empty());

    let home = home_with_table();
    added(home.path(), "shared-repo-landing", Some("Only Chao uses this repo.")).unwrap();
    let path = digest_path(home.path());
    let raw = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{raw}{{broken\n")).unwrap();

    let error = read_entries(home.path()).expect_err("the broken line refuses the read");
    assert!(error.to_string().contains("state/digest_entries.jsonl line 2"), "{error}");
  }

  #[test]
  fn blank_lines_are_skipped_and_a_non_digest_record_line_names_the_store_and_the_line() {
    let home = home_with_table();
    let path = digest_path(home.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "{\"type\": \"policy_table_changed\", \"time\": \"x\"}\n\n").unwrap();

    let error = read_entries(home.path()).expect_err("a non-digest record refuses the read");
    assert!(error.to_string().contains("state/digest_entries.jsonl line 1"), "{error}");

    fs::write(&path, "\n").unwrap();
    assert!(read_entries(home.path()).expect("a blank store reads").is_empty());
  }
}

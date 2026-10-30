//! The approval policy table (`$CHARLIEBOT_HOME/config.d/approval_policy.yaml`, plan 4 v2 task W2): the consequence
//! tests that make a step T0, the standing policies that keep a step at T1, and the ask-tier refusal switch.
//!
//! The table is required host configuration: an absent or invalid table is an error that names the path and the
//! field, and nothing falls back to a default table. Every read opens the file fresh, so an edit takes effect on the
//! next read with no server restart. A server-side read whose table content differs from the last recorded content
//! appends one `policy_table_changed` record to `$CHARLIEBOT_HOME/state/policy_table_changes.jsonl`, next to the
//! other host-level state records; the baseline content the first read records lives in
//! `$CHARLIEBOT_HOME/state/policy_table_recorded.yaml`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::Date;
use time::Month;

use crate::paths;
use crate::text_repr::{JsonStyle, json_dumps};
use crate::timefmt;

/// The table's file name under `config.d/`.
pub const TABLE_RELATIVE_FILENAME: &str = "approval_policy.yaml";

/// The table's path under the CharlieBot home (`approval_policy.TABLE_PATH`).
pub const TABLE_RELATIVE_PATH: &str = "config.d/approval_policy.yaml";

/// The title the manager prompts and the ask probe carry the table under (`approval_policy.SECTION_TITLE`).
pub const SECTION_TITLE: &str = "Approval policy table";

/// The ids a consequence test row may carry, each exactly once (`approval_policy.CONSEQUENCE_TEST_IDS`).
pub const CONSEQUENCE_TEST_IDS: [&str; 6] =
  ["irreversible", "chao-name", "money", "authority", "reading", "rule-named"];

/// The tier values a cold-read answer's machine-readable line may give (`approval_policy.ASK_TIERS`).
pub const ASK_TIERS: [&str; 5] = ["t0", "t1", "t2", "user-act", "review"];

/// The exact form of the machine-readable line the ask cold read ends with (`approval_policy.TIER_LINE_FORM`).
pub const TIER_LINE_FORM: &str = "tier: <tier> rule: <rule>";

/// The host-level record store of table changes, next to the other host-level state records.
pub const CHANGES_RELATIVE_PATH: &str = "state/policy_table_changes.jsonl";

/// The file that holds the last recorded table content, the baseline the change records diff against.
pub const RECORDED_RELATIVE_PATH: &str = "state/policy_table_recorded.yaml";

/// `json.dumps(..., ensure_ascii=False)` with the default separators: one NDJSON record line.
const RECORD_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// One consequence test: a T0 criterion with its sort rank and its text (`approval_policy.ConsequenceTest`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConsequenceTest {
  pub id: String,
  pub rank: i64,
  pub text: String,
}

/// One standing policy: what it covers, its per-PT-day cap (`None` when uncapped), and its expiry date
/// (`approval_policy.StandingPolicy`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StandingPolicy {
  pub id: String,
  pub scope: String,
  pub cap_per_pt_day: Option<i64>,
  pub expires: String,
}

/// The parsed approval policy table as the server reads it (`approval_policy.ApprovalPolicyTable`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalPolicyTable {
  pub consequence_tests: Vec<ConsequenceTest>,
  pub standing_policies: Vec<StandingPolicy>,
  pub enable_ask_tier_refusal: bool,
}

/// The table as read: the parsed table, the raw file text, and the SHA-256 of that text
/// (`approval_policy.LoadedTable`).
#[derive(Debug)]
pub struct LoadedTable {
  pub table: ApprovalPolicyTable,
  pub raw: String,
  pub sha256: String,
}

/// A table read or a tier-line parse that refused, with its message (`approval_policy.ApprovalPolicyError`).
#[derive(Debug)]
pub struct PolicyError(String);

impl std::fmt::Display for PolicyError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl std::error::Error for PolicyError {}

/// The table's path under `home` (`approval_policy.table_path`).
pub fn table_path(home: &Path) -> PathBuf {
  home.join(TABLE_RELATIVE_PATH)
}

/// The changes record store's path under `home`.
fn changes_path(home: &Path) -> PathBuf {
  home.join(CHANGES_RELATIVE_PATH)
}

/// The last recorded content's path under `home`.
fn recorded_path(home: &Path) -> PathBuf {
  home.join(RECORDED_RELATIVE_PATH)
}

/// The lowercase SHA-256 of `bytes`.
fn sha256_bytes(bytes: &[u8]) -> String {
  format!("{:x}", Sha256::digest(bytes))
}

/// The lowercase SHA-256 of `text`.
fn sha256_text(text: &str) -> String {
  sha256_bytes(text.as_bytes())
}

/// Read and validate the table fresh from `home` (`approval_policy.load`). The error names the path and, for an
/// invalid field, the field.
pub fn load(home: &Path) -> Result<LoadedTable, PolicyError> {
  let path = table_path(home);
  let path_text = paths::display(&path);
  let raw = match fs::read_to_string(&path) {
    Ok(raw) => raw,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      return Err(PolicyError(format!(
        "{path_text}: missing; the approval policy table is required host configuration and no default table exists"
      )));
    }
    Err(error) => return Err(PolicyError(format!("{path_text}: {error}"))),
  };
  let table: ApprovalPolicyTable = match serde_path_to_error::deserialize(serde_yaml::Deserializer::from_str(&raw)) {
    Ok(table) => table,
    Err(error) => {
      let field = error.path().to_string();
      return Err(PolicyError(if field.is_empty() {
        format!("{path_text}: {}", error.inner())
      } else {
        format!("{path_text}: {field}: {}", error.inner())
      }));
    }
  };
  validate(&path_text, &table)?;
  Ok(LoadedTable { table, sha256: sha256_text(&raw), raw })
}

/// The checks serde cannot express: known ids, each exactly once (`approval_policy._validate`).
fn validate(path_text: &str, table: &ApprovalPolicyTable) -> Result<(), PolicyError> {
  let mut seen = Vec::new();
  for (index, test) in table.consequence_tests.iter().enumerate() {
    if !CONSEQUENCE_TEST_IDS.contains(&test.id.as_str()) {
      return Err(PolicyError(format!(
        "{path_text}: consequence_tests[{index}].id: unknown consequence test id {:?} (known: {})",
        test.id,
        CONSEQUENCE_TEST_IDS.join(", ")
      )));
    }
    if seen.contains(&test.id) {
      return Err(PolicyError(format!(
        "{path_text}: consequence_tests[{index}].id: consequence test id {:?} appears twice",
        test.id
      )));
    }
    seen.push(test.id.clone());
  }
  for id in CONSEQUENCE_TEST_IDS {
    if !seen.contains(&id.to_string()) {
      return Err(PolicyError(format!(
        "{path_text}: consequence_tests: consequence test id {id:?} is missing (the table carries all of: {})",
        CONSEQUENCE_TEST_IDS.join(", ")
      )));
    }
  }
  let mut policy_ids = Vec::new();
  for (index, policy) in table.standing_policies.iter().enumerate() {
    if parse_date(&policy.expires).is_none() {
      return Err(PolicyError(format!(
        "{path_text}: standing_policies[{index}].expires: must be a YYYY-MM-DD date, got {:?}",
        policy.expires
      )));
    }
    if policy_ids.contains(&policy.id) {
      return Err(PolicyError(format!(
        "{path_text}: standing_policies[{index}].id: standing policy id {:?} appears twice",
        policy.id
      )));
    }
    policy_ids.push(policy.id.clone());
  }
  Ok(())
}

/// The canonical `YYYY-MM-DD` date `text` holds, or `None` (`approval_policy._parse_date`).
pub fn parse_date(text: &str) -> Option<Date> {
  let (year, rest) = text.split_once('-')?;
  let (month, day) = rest.split_once('-')?;
  let date = Date::from_calendar_date(
    year.parse().ok()?,
    Month::try_from(u8::try_from(month.parse::<u32>().ok()?).ok()?).ok()?,
    day.parse().ok()?,
  )
  .ok()?;
  (text == format!("{date}")).then_some(date)
}

/// The titled section the manager prompts and the ask probe carry: the table rendered back to YAML
/// (`approval_policy.prompt_section`).
pub fn prompt_section(table: &ApprovalPolicyTable) -> String {
  let yaml = serde_yaml::to_string(table).expect("the parsed approval policy table serializes");
  format!("## {SECTION_TITLE}\n\n{yaml}")
}

/// Read the table and append one `policy_table_changed` record when its content differs from the last recorded
/// content (`approval_policy.load_and_record`). Server-side reads use this; the CLI's reads use [`load`], which
/// records nothing.
pub fn load_and_record(home: &Path) -> Result<LoadedTable, PolicyError> {
  let loaded = load(home)?;
  record_if_changed(home, &loaded)?;
  Ok(loaded)
}

/// The check-and-append one read performs, serialized per process (`approval_policy._record_if_changed`). The first
/// read on a home records the baseline content and no record; a later read whose content hash differs appends one
/// record and becomes the new baseline.
pub fn record_if_changed(home: &Path, loaded: &LoadedTable) -> Result<bool, PolicyError> {
  let _guard = RECORD_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let recorded = recorded_path(home);
  let previous = match fs::read_to_string(&recorded) {
    Ok(previous) => previous,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      write_recorded(&recorded, &loaded.raw)?;
      return Ok(false);
    }
    Err(error) => {
      return Err(PolicyError(format!("{}: {error}", paths::display(&recorded))));
    }
  };
  if sha256_text(&previous) == loaded.sha256 {
    return Ok(false);
  }
  let record = json!({
    "type": "policy_table_changed",
    "time": timefmt::utc_now_iso(),
    "old_sha256": sha256_text(&previous),
    "new_sha256": loaded.sha256,
    "diff": unified_diff(&previous, &loaded.raw),
  });
  let store = changes_path(home);
  if let Some(parent) = store.parent() {
    fs::create_dir_all(parent).map_err(|error| PolicyError(format!("{}: {error}", paths::display(parent))))?;
  }
  let mut line = json_dumps(&record, &RECORD_JSON);
  line.push('\n');
  {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(&store)
      .map_err(|error| PolicyError(format!("{}: {error}", paths::display(&store))))?;
    file.write_all(line.as_bytes()).map_err(|error| PolicyError(format!("{}: {error}", paths::display(&store))))?;
    file.sync_data().map_err(|error| PolicyError(format!("{}: {error}", paths::display(&store))))?;
  }
  write_recorded(&recorded, &loaded.raw)?;
  Ok(true)
}

/// The last recorded content's file, written after each read that sees content (`approval_policy._write_recorded`).
fn write_recorded(path: &Path, raw: &str) -> Result<(), PolicyError> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).map_err(|error| PolicyError(format!("{}: {error}", paths::display(parent))))?;
  }
  fs::write(path, raw).map_err(|error| PolicyError(format!("{}: {error}", paths::display(path))))
}

/// The append-only check-and-append serialization. Only the server process records, and its reads run on several
/// tokio workers, so one mutex makes a changed read append exactly one record.
static RECORD_LOCK: Mutex<()> = Mutex::new(());

/// One tiered ask cold read: the tier and the rule that names it, `None` for no rule
/// (`approval_policy.AskTier`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskTier {
  pub tier: String,
  pub rule: Option<String>,
}

/// Parse the machine-readable tier line the ask cold read ends with (`approval_policy.parse_ask_tier`). The line is
/// the answer's last non-blank line in [`TIER_LINE_FORM`]; the rule must be a consequence test id, a standing policy
/// id, `out-of-scope:<policy id>` for a known policy id, or `none`.
pub fn parse_ask_tier(answer: &str, table: &ApprovalPolicyTable) -> Result<AskTier, PolicyError> {
  let missing = || {
    PolicyError(format!(
      "the cold-read answer carries no parsable tier line; it must end with one line of the form {TIER_LINE_FORM}, \
     where <tier> is t0, t1, t2, user-act or review"
    ))
  };
  let line = answer.lines().rev().find(|line| !line.trim().is_empty()).ok_or_else(missing)?;
  let rest = line.trim().strip_prefix("tier: ").ok_or_else(missing)?;
  let (tier, rule) = rest.split_once(" rule: ").ok_or_else(missing)?;
  if !ASK_TIERS.contains(&tier) {
    return Err(PolicyError(format!(
      "the cold-read answer's tier line names the unknown tier {tier:?}; the tier is one of {}",
      ASK_TIERS.join(", ")
    )));
  }
  let rule = match rule {
    "none" => None,
    rule if rule.starts_with("out-of-scope:") => {
      let policy_id = rule.trim_start_matches("out-of-scope:");
      require_policy(table, policy_id)?;
      Some(rule.to_string())
    }
    rule if CONSEQUENCE_TEST_IDS.contains(&rule) => Some(rule.to_string()),
    rule => {
      require_policy(table, rule)?;
      Some(rule.to_string())
    }
  };
  Ok(AskTier { tier: tier.to_string(), rule })
}

/// The standing policy `id`, or an error naming it and the known ids.
fn require_policy(table: &ApprovalPolicyTable, id: &str) -> Result<(), PolicyError> {
  let known = table.standing_policies.iter().map(|policy| policy.id.as_str()).collect::<Vec<_>>();
  if known.contains(&id) {
    return Ok(());
  }
  Err(PolicyError(format!(
    "the cold-read answer's tier line names the unknown rule {id:?}; a standing policy rule is one of {}",
    if known.is_empty() { "none (the table names no standing policies)".to_string() } else { known.join(", ") }
  )))
}

/// The line-level difference of `new` from `old`: removed lines prefixed `-`, added lines prefixed `+`, shared
/// lines prefixed two spaces (`approval_policy._unified_diff`).
fn unified_diff(old: &str, new: &str) -> String {
  let old_lines: Vec<&str> = old.lines().collect();
  let new_lines: Vec<&str> = new.lines().collect();
  let rows = old_lines.len();
  let columns = new_lines.len();
  // LCS lengths: table[i][j] is the common suffix length of old_lines[i..] and new_lines[j..].
  let mut common = vec![vec![0usize; columns + 1]; rows + 1];
  for i in (0..rows).rev() {
    for j in (0..columns).rev() {
      common[i][j] =
        if old_lines[i] == new_lines[j] { common[i + 1][j + 1] + 1 } else { common[i + 1][j].max(common[i][j + 1]) };
    }
  }
  let mut diff = String::new();
  let mut i = 0;
  let mut j = 0;
  while i < rows && j < columns {
    if old_lines[i] == new_lines[j] {
      diff.push_str(&format!("  {}\n", old_lines[i]));
      i += 1;
      j += 1;
    } else if common[i + 1][j] >= common[i][j + 1] {
      diff.push_str(&format!("- {}\n", old_lines[i]));
      i += 1;
    } else {
      diff.push_str(&format!("+ {}\n", new_lines[j]));
      j += 1;
    }
  }
  for line in &old_lines[i..] {
    diff.push_str(&format!("- {line}\n"));
  }
  for line in &new_lines[j..] {
    diff.push_str(&format!("+ {line}\n"));
  }
  diff
}

#[cfg(test)]
mod tests {
  use tempfile::TempDir;

  use super::*;

  /// A valid table with one uncapped policy and one capped policy.
  pub const VALID_TABLE: &str = "\
consequence_tests:
  - {id: irreversible, rank: 1, text: \"Deletes data with no backup, or sends what cannot be recalled.\"}
  - {id: chao-name, rank: 2, text: \"A message or publication in Chao's name reaches other people.\"}
  - {id: money, rank: 3, text: \"Spends money beyond a budget that Chao set.\"}
  - {id: authority, rank: 4, text: \"Widens what agents may do.\"}
  - {id: rule-named, rank: 4, text: \"A rule in force names the user's explicit approval for this step.\"}
  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}
standing_policies:
  - {id: release-restart, scope: \"Release and restart the CharlieBot checkout.\", cap_per_pt_day: null, expires: 2099-01-01}
  - {id: shared-repo-landing, scope: \"Land on a shared repository's non-main branch.\", cap_per_pt_day: 16, expires: 2099-01-01}
enable_ask_tier_refusal: false
";

  fn home_with_table(text: &str) -> (TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("temp home");
    let path = table_path(home.path());
    fs::create_dir_all(path.parent().unwrap()).expect("config.d");
    fs::write(&path, text).expect("the table writes");
    let path = path.to_path_buf();
    (home, path)
  }

  fn loaded_valid(home: &Path) -> LoadedTable {
    load(home).expect("the valid table loads")
  }

  #[test]
  fn a_valid_table_loads_with_its_fields_and_hash() {
    let (home, _path) = home_with_table(VALID_TABLE);

    let loaded = loaded_valid(home.path());

    assert_eq!(loaded.table.consequence_tests.len(), 6);
    assert_eq!(loaded.table.consequence_tests[0].id, "irreversible");
    assert_eq!(loaded.table.standing_policies[1].cap_per_pt_day, Some(16));
    assert_eq!(parse_date(&loaded.table.standing_policies[0].expires).map(|date| date.year()), Some(2099));
    assert!(!loaded.table.enable_ask_tier_refusal);
    assert_eq!(loaded.sha256, sha256_text(&loaded.raw));
    // The same bytes load to the same hash; a different byte reads as a different hash.
    assert_ne!(loaded.sha256, sha256_text(&format!("{VALID_TABLE}\n")));
  }

  #[test]
  fn an_absent_table_is_an_error_that_names_the_path() {
    let home = tempfile::tempdir().expect("temp home");

    let error = load(home.path()).expect_err("the absent table is an error");

    assert!(
      error.to_string().starts_with(&format!("{}: missing", paths::display(&table_path(home.path())))),
      "{error}"
    );
  }

  #[test]
  fn a_malformed_table_is_an_error_that_names_the_path() {
    let (home, _path) = home_with_table("consequence_tests: [1, 2\n");

    let error = load(home.path()).expect_err("the malformed table is an error");

    assert!(error.to_string().contains(&paths::display(&table_path(home.path()))), "{error}");
  }

  #[test]
  fn an_unknown_field_is_an_error_that_names_the_path_and_the_field() {
    let (home, _path) = home_with_table(&format!("{VALID_TABLE}extra_key: true\n"));

    let error = load(home.path()).expect_err("the unknown field is an error");

    assert!(error.to_string().contains("extra_key"), "{error}");
    assert!(error.to_string().contains(&paths::display(&table_path(home.path()))), "{error}");
  }

  #[test]
  fn an_unknown_inner_field_is_an_error_that_names_the_field() {
    let (home, _path) = home_with_table(
      "consequence_tests:\n  - {id: irreversible, rank: 1, text: t, why: no}\n\
       standing_policies: []\nenable_ask_tier_refusal: false\n",
    );

    let error = load(home.path()).expect_err("the unknown inner field is an error");

    assert!(error.to_string().contains("consequence_tests[0].why"), "{error}");
  }

  #[test]
  fn a_missing_field_is_an_error_that_names_the_field() {
    let (home, _path) = home_with_table("consequence_tests: []\nenable_ask_tier_refusal: false\n");

    let error = load(home.path()).expect_err("the missing field is an error");

    assert!(error.to_string().contains("standing_policies"), "{error}");
  }

  #[test]
  fn a_wrong_typed_field_is_an_error_that_names_the_field() {
    let (home, _path) =
      home_with_table(&VALID_TABLE.replace("enable_ask_tier_refusal: false", "enable_ask_tier_refusal: \"no\""));

    let error = load(home.path()).expect_err("the wrong type is an error");

    assert!(error.to_string().contains("enable_ask_tier_refusal"), "{error}");
  }

  #[test]
  fn an_unknown_and_a_missing_and_a_repeated_consequence_test_id_are_errors() {
    for (case, replacement) in [
      ("unknown", "  - {id: chaos, rank: 9, text: \"x\"}\n"),
      ("repeated", "  - {id: irreversible, rank: 9, text: \"x\"}\n"),
    ] {
      let text = VALID_TABLE.replace(
        "  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}\n",
        replacement,
      );
      let (home, _path) = home_with_table(&text);

      let error = load(home.path()).expect_err("the id is refused");

      assert!(error.to_string().contains("consequence_tests[5].id"), "{case}: {error}");
      if case == "unknown" {
        assert!(error.to_string().contains("unknown consequence test id"), "{error}");
      } else {
        assert!(error.to_string().contains("appears twice"), "{error}");
      }
    }
    // A missing id: drop the reading row without a replacement.
    let text = VALID_TABLE
      .replace("  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}\n", "");
    let (home, _path) = home_with_table(&text);

    let error = load(home.path()).expect_err("the missing id is refused");

    assert!(error.to_string().contains("consequence test id \"reading\" is missing"), "{error}");
  }

  #[test]
  fn a_non_date_expiry_is_an_error_that_names_the_field() {
    let text = VALID_TABLE.replace("expires: 2099-01-01", "expires: soon");
    let (home, _path) = home_with_table(&text);

    let error = load(home.path()).expect_err("the expiry is refused");

    assert!(error.to_string().contains("standing_policies[0].expires"), "{error}");
  }

  #[test]
  fn a_repeated_standing_policy_id_is_an_error() {
    let text = VALID_TABLE.replace(
      "  - {id: shared-repo-landing, scope: \"Land on a shared repository's non-main branch.\", cap_per_pt_day: 16, expires: 2099-01-01}\n",
      "  - {id: release-restart, scope: \"Land on a shared repository's non-main branch.\", cap_per_pt_day: 16, expires: 2099-01-01}\n  - {id: release-restart, scope: \"Again.\", cap_per_pt_day: null, expires: 2099-01-01}\n",
    );
    let (home, _path) = home_with_table(&text);

    let error = load(home.path()).expect_err("the repeated policy id is refused");

    assert!(error.to_string().contains("standing policy id \"release-restart\" appears twice"), "{error}");
  }

  #[test]
  fn the_prompt_section_carries_the_title_and_the_rendered_table() {
    let (home, _path) = home_with_table(VALID_TABLE);
    let loaded = loaded_valid(home.path());

    let section = prompt_section(&loaded.table);

    assert!(section.starts_with("## Approval policy table\n\n"), "{section}");
    assert!(section.contains("enable_ask_tier_refusal: false"), "{section}");
    assert!(section.contains("- id: release-restart"), "{section}");
    assert!(section.contains("cap_per_pt_day: null"), "{section}");
  }

  #[test]
  fn the_first_read_records_no_change_and_a_changed_read_appends_exactly_one_record() {
    let (home, path) = home_with_table(VALID_TABLE);
    let changes = changes_path(home.path());

    let first = record_if_changed(home.path(), &loaded_valid(home.path())).expect("the baseline records");
    let second = record_if_changed(home.path(), &loaded_valid(home.path())).expect("the unchanged read records");

    assert!(!first);
    assert!(!second);
    assert!(!changes.exists(), "an unchanged read appends nothing");

    let edited = format!("{VALID_TABLE}# a comment changes the content\n");
    fs::write(&path, edited).expect("the table edit writes");

    let changed = record_if_changed(home.path(), &loaded_valid(home.path())).expect("the changed read records");
    let again = record_if_changed(home.path(), &loaded_valid(home.path())).expect("the re-read records");

    assert!(changed);
    assert!(!again);
    let store = fs::read_to_string(&changes).expect("the record store reads");
    assert_eq!(store.lines().count(), 1, "{store}");
    let record: serde_json::Value = serde_json::from_str(store.trim()).expect("the record parses");
    assert_eq!(record["type"], "policy_table_changed");
    assert_ne!(record["old_sha256"], record["new_sha256"]);
    let diff = record["diff"].as_str().expect("the record carries the diff");
    assert!(diff.contains("+ # a comment changes the content"), "{diff}");
  }

  #[test]
  fn an_edited_table_read_back_carries_the_new_text_in_its_prompt_section() {
    let (home, path) = home_with_table(VALID_TABLE);
    let before = prompt_section(&loaded_valid(home.path()).table);
    fs::write(&path, VALID_TABLE.replace("cap_per_pt_day: 16", "cap_per_pt_day: 8")).expect("the edit writes");

    let after = prompt_section(&loaded_valid(home.path()).table);

    assert!(!before.contains("cap_per_pt_day: 8"), "{before}");
    assert!(after.contains("cap_per_pt_day: 8"), "{after}");
  }

  fn table_with_refusal(enabled: bool) -> ApprovalPolicyTable {
    let text = VALID_TABLE
      .replace("enable_ask_tier_refusal: false", &format!("enable_ask_tier_refusal: {enabled}"))
      .replace("cap_per_pt_day: 16", "cap_per_pt_day: null");
    let (home, _path) = home_with_table(&text);
    loaded_valid(home.path()).table
  }

  #[test]
  fn each_tier_value_parses_with_its_rule() {
    let table = table_with_refusal(false);
    for (tier, rule, expected) in [
      ("t0", "irreversible", Some("irreversible".to_string())),
      ("t0", "out-of-scope:shared-repo-landing", Some("out-of-scope:shared-repo-landing".to_string())),
      ("t1", "release-restart", Some("release-restart".to_string())),
      ("t2", "none", None),
      ("user-act", "none", None),
      ("review", "none", None),
    ] {
      let answer = format!("(1) Decide.\n(5) tiered.\ntier: {tier} rule: {rule}");

      let parsed = parse_ask_tier(&answer, &table).expect("the tier line parses");

      assert_eq!(parsed, AskTier { tier: tier.to_string(), rule: expected }, "{tier} {rule}");
    }
  }

  #[test]
  fn an_answer_without_a_parsable_tier_line_is_refused() {
    let table = table_with_refusal(false);
    for answer in ["", "(1) Decide.", "tier: t0 rule: irreversible\nextra trailing note"] {
      let error = parse_ask_tier(answer, &table).expect_err("the answer is refused");
      assert!(error.to_string().contains("no parsable tier line"), "{answer:?}: {error}");
    }
  }

  #[test]
  fn an_unknown_tier_or_rule_is_refused() {
    let table = table_with_refusal(false);
    for answer in ["tier: t3 rule: none", "tier: t0 rule: chaos", "tier: t1 rule: out-of-scope:chaos"] {
      let error = parse_ask_tier(answer, &table).expect_err("the line is refused");
      assert!(error.to_string().contains("unknown"), "{answer:?}: {error}");
    }
  }

  #[test]
  fn the_diff_names_the_removed_and_the_added_line() {
    let diff = unified_diff("a: 1\nb: 2\nc: 3\n", "a: 1\nb: 9\nc: 3\n");
    assert_eq!(diff, "  a: 1\n- b: 2\n+ b: 9\n  c: 3\n");
  }

  #[test]
  fn load_and_record_records_the_same_change_the_explicit_recording_does() {
    let (home, path) = home_with_table(VALID_TABLE);
    load_and_record(home.path()).expect("the first load records the baseline");
    fs::write(&path, format!("{VALID_TABLE}# edited\n")).expect("the edit writes");

    load_and_record(home.path()).expect("the second load records the change");

    let store = fs::read_to_string(changes_path(home.path())).expect("the record store reads");
    assert_eq!(store.lines().count(), 1, "{store}");
  }
}

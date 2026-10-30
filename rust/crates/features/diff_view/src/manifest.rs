//! The per-file manifest of a diff range (`src/features/diff_view/api.py` `diff_files`): the name-status and numstat
//! reads of `git diff -z`, and the rows they build.

use std::collections::HashMap;
use std::path::Path;

use axum::response::Response;
use charliebot_compat::responses;
use charliebot_core::text_repr;
use serde_json::{Value, json};

use crate::git::{self, DIFF_TIMEOUT};

/// `_range_spec`: `base...head` for three-dot, `base..head` for two-dot.
pub(crate) fn range_spec(base: &str, head: &str, mode: &str) -> String {
  if mode == "three-dot" { format!("{base}...{head}") } else { format!("{base}..{head}") }
}

/// The rows and totals of one range, shared by every response that serves it.
pub(crate) struct Manifest {
  /// Each row is `{"path", "status", "additions", "deletions"}` plus `old_path` for a rename or copy.
  pub(crate) rows: Vec<Value>,
  pub(crate) total_additions: i64,
  pub(crate) total_deletions: i64,
}

/// One numstat record: a file's counts, its path, and the pre-rename path of a rename or copy.
struct NumstatEntry {
  additions: i64,
  deletions: i64,
  path: String,
  old_path: Option<String>,
}

/// Runs the two manifest reads over the same range at once. The name-status failure surfaces before the numstat one,
/// the order the sequential form raised them in.
pub(crate) async fn build(repo: &Path, range: &str) -> Result<Manifest, Response> {
  let name_status_args = ["diff", "--name-status", "-z", range];
  let numstat_args = ["diff", "--numstat", "-z", range];
  let (name_status, numstat) = tokio::join!(
    git::run(repo, &name_status_args, DIFF_TIMEOUT, "git diff failed"),
    git::run(repo, &numstat_args, DIFF_TIMEOUT, "git diff failed"),
  );
  let name_status = name_status?;
  let numstat = numstat?;
  let statuses = parse_name_status(&name_status)?;
  let mut manifest = Manifest { rows: Vec::new(), total_additions: 0, total_deletions: 0 };
  for entry in parse_numstat(&numstat)? {
    let Some(status) = statuses.get(&entry.path) else {
      return Err(responses::internal_error("KeyError", text_repr::str_repr(&entry.path)));
    };
    let mut row = json!({
      "path": entry.path,
      "status": status.to_string(),
      "additions": entry.additions,
      "deletions": entry.deletions,
    });
    if let Some(old_path) = entry.old_path {
      row["old_path"] = json!(old_path);
    }
    manifest.rows.push(row);
    manifest.total_additions += entry.additions;
    manifest.total_deletions += entry.deletions;
  }
  Ok(manifest)
}

/// `_parse_name_status_z`: each new path's status letter. A rename or copy carries two path tokens after its status,
/// and the new path is keyed under `R` or `C`.
fn parse_name_status(output: &str) -> Result<HashMap<String, char>, Response> {
  let tokens: Vec<&str> = output.split('\0').collect();
  let mut statuses = HashMap::new();
  let mut i = 0;
  while i < tokens.len() {
    let status = tokens[i];
    if status.is_empty() {
      i += 1;
      continue;
    }
    let letter = status.chars().next().expect("a non-empty status has a first character");
    let (path_index, step) = if letter == 'R' || letter == 'C' { (i + 2, 3) } else { (i + 1, 2) };
    let Some(path) = tokens.get(path_index) else {
      return Err(responses::internal_error("IndexError", "list index out of range".to_string()));
    };
    statuses.insert(path.to_string(), letter);
    i += step;
  }
  Ok(statuses)
}

/// `_parse_numstat_z`: one record per file. A rename or copy is `added\tdeleted\t` with an empty path, followed by the
/// old and the new path. A binary file's `-` counts read as 0.
fn parse_numstat(output: &str) -> Result<Vec<NumstatEntry>, Response> {
  let tokens: Vec<&str> = output.split('\0').collect();
  let mut entries = Vec::new();
  let mut i = 0;
  while i < tokens.len() {
    let token = tokens[i];
    if token.is_empty() {
      i += 1;
      continue;
    }
    let fields: Vec<&str> = token.splitn(3, '\t').collect();
    let [added, deleted, path] = fields[..] else {
      return Err(responses::internal_error("ValueError", "numstat record has no path field".to_string()));
    };
    let (path, old_path, step) = if path.is_empty() {
      let (Some(old), Some(new)) = (tokens.get(i + 1), tokens.get(i + 2)) else {
        return Err(responses::internal_error("IndexError", "list index out of range".to_string()));
      };
      (new.to_string(), Some(old.to_string()), 3)
    } else {
      (path.to_string(), None, 1)
    };
    entries.push(NumstatEntry { additions: count(added)?, deletions: count(deleted)?, path, old_path });
    i += step;
  }
  Ok(entries)
}

/// One numstat count: `-` for a binary file reads as 0.
fn count(text: &str) -> Result<i64, Response> {
  if text == "-" {
    return Ok(0);
  }
  text.parse::<i64>().map_err(|_| {
    responses::internal_error("ValueError", format!("invalid literal for int(): {}", text_repr::str_repr(text)))
  })
}

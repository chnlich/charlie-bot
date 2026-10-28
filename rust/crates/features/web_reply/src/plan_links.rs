//! Plan-registry lookup for a Waiting on you item's file links (`src/features/web_reply/plan_links.py`).
//!
//! A link to a registered plan version names its plan lineage and version. The check refuses a link to a version
//! below the lineage's newest version or to a closed lineage. The since pairing groups items of one lineage together,
//! so a rewording or a newer version keeps the item's start time.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::anyhow;
use charliebot_compat::file_urls::{self, ScanContext};
use charliebot_core::paths;
use charliebot_registry::registry::PlanRegistryFn;
use regex::Regex;
use serde_json::Value;

use crate::metadata::WaitingItem;
use crate::reply::WebReplyError;

/// A bare file-server path inside prose: the context and text fields carry these without a Markdown target
/// (`plan_links._MOUNT_PATH`).
static MOUNT_PATH: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(&format!(r#"{}[^\s<>"'`)]*"#, regex::escape(file_urls::FILE_SERVER_MOUNT))).expect("valid pattern")
});

/// Trailing punctuation that a link in prose does not keep (`LINK_TRAILING_PUNCTUATION_RE`).
static TRAILING_PUNCTUATION: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"[.,;:!?*。！？；，、：．]+$").expect("valid pattern"));

/// One item link bound to a registered plan version (`plan_links.Binding`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
  pub session_id: String,
  pub plan_id: i64,
  pub version: i64,
  pub newest_version: i64,
  pub newest_file: PathBuf,
  pub closed: Option<String>,
}

impl Binding {
  /// Whether the link names an older version than the newest or a version of a closed lineage (`Binding.stale`).
  pub fn stale(&self) -> bool {
    self.closed.is_some() || self.version < self.newest_version
  }
}

/// One item link the registry refuses with the newest file the user should read instead (`plan_links.StaleLink`).
#[derive(Clone, Debug)]
pub struct StaleLink {
  pub link: String,
  pub binding: Binding,
}

/// Read the plan registries that item links name, once per session for one status command (`plan_links.PlanLinks`).
pub struct PlanLinks {
  sessions_dir: PathBuf,
  root: PathBuf,
  registries: HashMap<String, Option<Vec<Value>>>,
  read_plans: PlanRegistryFn,
}

impl PlanLinks {
  pub fn new(sessions_dir: PathBuf, read_plans: PlanRegistryFn) -> Self {
    let root = paths::realpath(&sessions_dir);
    Self { sessions_dir, root, registries: HashMap::new(), read_plans }
  }

  /// The session id and session-relative posix path that `path` names, or None outside the sessions directory
  /// (`PlanLinks._relative`).
  fn relative(&self, path: &Path) -> Option<(String, String)> {
    let resolved = paths::realpath(path);
    let relative = resolved.strip_prefix(&self.root).ok()?;
    let mut parts = relative.components();
    let session_id = parts.next()?.as_os_str().to_string_lossy().into_owned();
    let rest: Vec<String> = parts.map(|part| part.as_os_str().to_string_lossy().into_owned()).collect();
    if rest.is_empty() {
      return None;
    }
    Some((session_id, posix_normpath(&rest.join("/"))))
  }

  /// The plans of `session_id`, or None when its registry is missing (`PlanLinks._registry`). The registered reader
  /// decides what a registry that does not read means.
  fn registry(&mut self, session_id: &str) -> Result<Option<Vec<Value>>, WebReplyError> {
    if !self.registries.contains_key(session_id) {
      let plans = (self.read_plans)(&self.sessions_dir.join(session_id)).map_err(WebReplyError::Failed)?;
      self.registries.insert(session_id.to_string(), plans);
    }
    Ok(self.registries.get(session_id).expect("the registry was just read").clone())
  }

  /// The registered plan version that `path` names, or None for any other file (`PlanLinks.binding`).
  pub fn binding(&mut self, path: &Path) -> Result<Option<Binding>, WebReplyError> {
    let Some((session_id, file_relative)) = self.relative(path) else {
      return Ok(None);
    };
    let Some(plans) = self.registry(&session_id)? else {
      return Ok(None);
    };
    for plan in plans {
      let Value::Object(plan) = plan else {
        return Err(WebReplyError::Failed(anyhow!("a plan of session {session_id} is not an object")));
      };
      let versions = plan.get("versions").ok_or_else(|| {
        WebReplyError::Failed(anyhow!(
          "plan {} has no versions",
          plan.get("id").map_or_else(|| "None".to_string(), |id| id.to_string())
        ))
      })?;
      let Value::Array(versions) = versions else {
        return Err(WebReplyError::Failed(anyhow!(
          "the versions of plan {} are not a list",
          plan.get("id").map_or_else(|| "None".to_string(), |id| id.to_string())
        )));
      };
      let matched = versions
        .iter()
        .find(|version| version_file(version).is_some_and(|file| posix_normpath(&file) == file_relative));
      let Some(matched) = matched else { continue };
      let newest = versions
        .iter()
        .map(|version| version_number(version).map(|v| (v, version)))
        .collect::<Result<Vec<_>, WebReplyError>>()?
        .into_iter()
        .max_by_key(|(v, _)| *v);
      let Some((newest_version, newest)) = newest else { continue };
      let closed = match plan.get("closed") {
        None | Some(Value::Null) => None,
        Some(closed) => Some(
          closed
            .get("as")
            .and_then(Value::as_str)
            .ok_or_else(|| {
              WebReplyError::Failed(anyhow!(
                "plan {} closed state has no as string",
                plan.get("id").map_or_else(|| "None".to_string(), |id| id.to_string())
              ))
            })?
            .to_string(),
        ),
      };
      let plan_id = plan
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| WebReplyError::Failed(anyhow!("plan of session {session_id} has no integer id")))?;
      let newest_file = newest
        .get("file")
        .and_then(Value::as_str)
        .ok_or_else(|| WebReplyError::Failed(anyhow!("plan {plan_id} newest version has no file string")))?;
      return Ok(Some(Binding {
        newest_file: self.sessions_dir.join(&session_id).join(newest_file),
        session_id,
        plan_id,
        version: version_number(matched)?,
        newest_version,
        closed,
      }));
    }
    Ok(None)
  }

  /// The pairing target of `item`: its plan lineage, its other link, or the one group for items without a link
  /// (`PlanLinks.target`).
  pub fn target(&mut self, item: &WaitingItem) -> Result<String, WebReplyError> {
    let Some(link) = item.link.clone() else {
      return Ok("no-link".to_string());
    };
    let path = host_path(Some(&link))?;
    let binding = match path.as_deref() {
      Some(path) => self.binding(path)?,
      None => None,
    };
    Ok(match binding {
      Some(binding) => format!("plan:{}:{}", binding.session_id, binding.plan_id),
      None => format!("link:{link}"),
    })
  }

  /// The link text and local file path of each file link `value` names (`PlanLinks._links`).
  fn links(&self, value: Option<&str>, scan_context: &ScanContext) -> Result<Vec<(String, PathBuf)>, WebReplyError> {
    let Some(value) = value else {
      return Ok(Vec::new());
    };
    let mut links = Vec::new();
    for finding in
      file_urls::scan_file_links(value, Some(scan_context)).map_err(|error| WebReplyError::Failed(error.into()))?
    {
      if finding.live != Some(true) {
        continue;
      }
      let Some(path) = finding.path else { continue };
      links.push((finding.link, path));
    }
    for matched in MOUNT_PATH.find_iter(value) {
      let link = TRAILING_PUNCTUATION.replace_all(matched.as_str(), "").into_owned();
      if let Some(path) = host_path(Some(&link))? {
        links.push((link, path));
      }
    }
    Ok(links)
  }

  /// Every stale registered-plan link in the text, context and link of `items` (`PlanLinks.stale_links`).
  pub fn stale_links(
    &mut self,
    items: &[WaitingItem],
    scan_context: &ScanContext,
  ) -> Result<Vec<StaleLink>, WebReplyError> {
    let mut stale = Vec::new();
    let mut seen: HashSet<(String, String, i64)> = HashSet::new();
    for item in items {
      for value in [Some(item.text.as_str()), item.context.as_deref(), item.link.as_deref()] {
        let mut links = self.links(value, scan_context)?;
        if value == item.link.as_deref()
          && let Some(path) = host_path(item.link.as_deref())?
        {
          let link = item.link.clone().expect("the value equals the link");
          links.push((link, path));
        }
        for (link, path) in links {
          let Some(binding) = self.binding(&path)? else { continue };
          if !binding.stale() {
            continue;
          }
          if !seen.insert((link.clone(), binding.session_id.clone(), binding.plan_id)) {
            continue;
          }
          stale.push(StaleLink { link, binding });
        }
      }
    }
    Ok(stale)
  }
}

/// The link's host file, as `file_urls.host_file` returns it; a malformed link fails as Python's urlsplit does.
fn host_path(link: Option<&str>) -> Result<Option<PathBuf>, WebReplyError> {
  file_urls::host_file(link).map_err(|message| WebReplyError::Failed(anyhow!("{message}")))
}

/// The `file` string of one plan-registry version row; a version without one fails as Python's indexing does.
fn version_file(version: &Value) -> Option<String> {
  version.get("file").and_then(Value::as_str).map(str::to_string)
}

/// The `v` integer of one plan-registry version row; a version without one fails as Python's indexing does.
fn version_number(version: &Value) -> Result<i64, WebReplyError> {
  version
    .get("v")
    .and_then(Value::as_i64)
    .ok_or_else(|| WebReplyError::Failed(anyhow!("a plan version has no integer v: {version}")))
}

/// `posixpath.normpath` for the session-relative and registry-file paths the lookup compares.
fn posix_normpath(text: &str) -> String {
  let absolute = text.starts_with('/');
  let mut parts: Vec<&str> = Vec::new();
  for part in text.split('/') {
    match part {
      "" | "." => {}
      ".." if absolute => {
        parts.pop();
      }
      ".." if parts.last().is_some_and(|last| *last != "..") => {
        parts.pop();
      }
      ".." => parts.push(".."),
      part => parts.push(part),
    }
  }
  let joined = parts.join("/");
  if absolute {
    format!("/{joined}")
  } else if joined.is_empty() {
    ".".to_string()
  } else {
    joined
  }
}

/// The 409 detail that names each stale link and its lineage's newest file (`plan_links.stale_detail`).
pub fn stale_detail(stale: &[StaleLink]) -> String {
  let mut lines = vec!["Stale plan links; nothing was stored.".to_string()];
  for entry in stale {
    let binding = &entry.binding;
    let reason = match &binding.closed {
      None => {
        format!("plan {} version {} is older than version {}", binding.plan_id, binding.version, binding.newest_version)
      }
      Some(closed) => format!(
        "plan {} is closed ({closed}); its newest version is version {}",
        binding.plan_id, binding.newest_version
      ),
    };
    lines.push(format!("- {}: {} at {}", entry.link, reason, binding.newest_file.display()));
  }
  lines.join("\n")
}

#[cfg(test)]
mod tests {
  use std::fs;

  use serde_json::json;

  use super::*;
  use crate::test_support::read_plans_fixture;

  fn item(text: &str, link: Option<&str>) -> WaitingItem {
    let raw = json!({"text": text, "context": "The line is active now.", "link": link});
    crate::status::parse_item(&raw).expect("the item validates")
  }

  fn registry(dir: &Path, session_id: &str, plans: Value) -> PathBuf {
    let path = dir.join(session_id).join("plans.json");
    fs::create_dir_all(path.parent().expect("the session directory")).expect("the session directory writes");
    fs::write(&path, json!({"plans": plans}).to_string()).expect("the plan registry writes");
    path
  }

  fn version(v: i64, file: &str) -> Value {
    json!({"v": v, "file": file})
  }

  #[test]
  fn a_link_to_the_newest_version_of_an_open_lineage_is_not_stale() {
    let dir = tempfile::tempdir().expect("the registry directory exists");
    let page = dir.path().join("s1").join("artifacts").join("plan_01_v2.html");
    fs::create_dir_all(page.parent().unwrap()).expect("the artifact directory writes");
    fs::write(&page, "page").expect("the page writes");
    registry(
      dir.path(),
      "s1",
      json!([{"id": 1, "versions": [version(1, "artifacts/plan_01_v1.html"), version(2, "artifacts/plan_01_v2.html")], "closed": null}]),
    );
    let mut links = PlanLinks::new(dir.path().to_path_buf(), read_plans_fixture);

    let binding = links.binding(&page).expect("the registry reads").expect("the version binds");

    assert_eq!(binding.session_id, "s1");
    assert_eq!(binding.plan_id, 1);
    assert_eq!(binding.version, 2);
    assert_eq!(binding.newest_version, 2);
    assert_eq!(binding.closed, None);
    assert!(!binding.stale());
  }

  #[test]
  fn a_link_below_the_newest_version_and_one_to_a_closed_lineage_are_stale() {
    let dir = tempfile::tempdir().expect("the registry directory exists");
    let old = dir.path().join("s1").join("artifacts").join("plan_01_v1.html");
    let closed_page = dir.path().join("s1").join("artifacts").join("plan_02_v1.html");
    for page in [&old, &closed_page] {
      fs::create_dir_all(page.parent().unwrap()).expect("the artifact directory writes");
      fs::write(page, "page").expect("the page writes");
    }
    registry(
      dir.path(),
      "s1",
      json!([
        {"id": 1, "versions": [version(1, "artifacts/plan_01_v1.html"), version(2, "artifacts/plan_01_v2.html")], "closed": null},
        {"id": 2, "versions": [version(1, "artifacts/plan_02_v1.html")], "closed": {"as": "completed"}},
      ]),
    );
    let mut links = PlanLinks::new(dir.path().to_path_buf(), read_plans_fixture);

    let old_binding = links.binding(&old).expect("the registry reads").expect("the old version binds");
    let closed_binding = links.binding(&closed_page).expect("the registry reads").expect("the closed version binds");

    assert!(old_binding.stale());
    assert_eq!(old_binding.newest_file, dir.path().join("s1").join("artifacts").join("plan_01_v2.html"));
    assert!(closed_binding.stale());
    assert_eq!(closed_binding.closed.as_deref(), Some("completed"));
  }

  #[test]
  fn a_missing_registry_an_outside_path_and_an_unregistered_file_bind_nothing() {
    let dir = tempfile::tempdir().expect("the registry directory exists");
    let page = dir.path().join("s1").join("artifacts").join("plan_01_v1.html");
    fs::create_dir_all(page.parent().unwrap()).expect("the artifact directory writes");
    fs::write(&page, "page").expect("the page writes");
    let outside = tempfile::tempdir().expect("the outside directory exists");
    let outside_page = outside.path().join("page.html");
    fs::write(&outside_page, "page").expect("the page writes");
    let mut links = PlanLinks::new(dir.path().to_path_buf(), read_plans_fixture);

    let no_registry = links.binding(&page).expect("the registry reads");
    let outside_binding = links.binding(&outside_page).expect("the outside path resolves");
    let short = links.binding(&dir.path().join("s1")).expect("the session directory resolves");

    assert_eq!(no_registry, None);
    assert_eq!(outside_binding, None);
    assert_eq!(short, None);
  }

  #[test]
  fn a_broken_registry_fails_and_a_version_without_a_number_fails() {
    let dir = tempfile::tempdir().expect("the registry directory exists");
    let page = dir.path().join("s1").join("artifacts").join("plan_01_v1.html");
    fs::create_dir_all(page.parent().unwrap()).expect("the artifact directory writes");
    fs::write(&page, "page").expect("the page writes");
    fs::write(dir.path().join("s1").join("plans.json"), "invalid JSON").expect("the broken registry writes");
    let mut links = PlanLinks::new(dir.path().to_path_buf(), read_plans_fixture);
    assert!(links.binding(&page).is_err(), "invalid JSON fails");

    let dir = tempfile::tempdir().expect("the registry directory exists");
    let page = dir.path().join("s1").join("artifacts").join("plan_01_v1.html");
    fs::create_dir_all(page.parent().unwrap()).expect("the artifact directory writes");
    fs::write(&page, "page").expect("the page writes");
    registry(
      dir.path(),
      "s1",
      json!([{"id": 1, "versions": [version(1, "artifacts/plan_01_v1.html"), {"v": "2", "file": "artifacts/plan_01_v2.html"}], "closed": null}]),
    );
    let mut links = PlanLinks::new(dir.path().to_path_buf(), read_plans_fixture);
    assert!(links.binding(&page).is_err(), "a version without an integer v fails");
  }

  #[test]
  fn posix_normpath_matches_posixpath_for_the_paths_the_lookup_compares() {
    assert_eq!(posix_normpath("artifacts/plan_01_v2.html"), "artifacts/plan_01_v2.html");
    assert_eq!(posix_normpath("artifacts/./plan_01_v2.html"), "artifacts/plan_01_v2.html");
    assert_eq!(posix_normpath("artifacts/../artifacts/plan_01_v2.html"), "artifacts/plan_01_v2.html");
    assert_eq!(posix_normpath("artifacts//plan_01_v2.html"), "artifacts/plan_01_v2.html");
    assert_eq!(posix_normpath("/a/b/../c"), "/a/c");
    assert_eq!(posix_normpath("/.."), "/");
    assert_eq!(posix_normpath("a/../../b"), "../b");
    assert_eq!(posix_normpath(""), ".");
  }

  #[test]
  fn the_pairing_target_groups_a_lineage_a_link_and_the_linkless_items() {
    let dir = tempfile::tempdir().expect("the registry directory exists");
    let page = dir.path().join("s1").join("artifacts").join("plan_01_v2.html");
    fs::create_dir_all(page.parent().unwrap()).expect("the artifact directory writes");
    fs::write(&page, "page").expect("the page writes");
    registry(
      dir.path(),
      "s1",
      json!([{"id": 7, "versions": [version(2, "artifacts/plan_01_v2.html")], "closed": null}]),
    );
    let mut links = PlanLinks::new(dir.path().to_path_buf(), read_plans_fixture);
    let plan_link = format!("{}{}", file_urls::FILE_SERVER_MOUNT, page.display());

    let plan_target = links.target(&item("Decide", Some(&plan_link))).expect("the lineage target resolves");
    let link_target =
      links.target(&item("Decide", Some("https://synthetic.invalid/answer"))).expect("the link target resolves");
    let no_link_target = links.target(&item("Decide", None)).expect("the linkless target resolves");

    assert_eq!(plan_target, "plan:s1:7");
    assert_eq!(link_target, "link:https://synthetic.invalid/answer");
    assert_eq!(no_link_target, "no-link");
  }
}

//! The artifacts package's sidebar contribution (`src/features/artifacts/sidebar.py`): the pending-approval flag of a row,
//! and the plan registry a fork carries over.

use std::collections::HashSet;
use std::fs::{self, FileTimes};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::bail;
use charliebot_core::fsutil;
use charliebot_core::paths;
use charliebot_registry::sidebar::SidebarContribution;
use charliebot_store::sidebar_state;
use serde_json::{Map, Value};

use crate::plan_paths;
use crate::plans::{self, AWAITING_APPROVAL_STATE, LoadFailure, PLANS_FILE};

/// The artifacts package's sidebar contribution: the pending-approval flag, and the registry a fork copies.
pub(crate) struct ArtifactsSidebar;

impl SidebarContribution for ArtifactsSidebar {
  fn watched_files(&self) -> &'static [&'static str] {
    &[PLANS_FILE]
  }

  fn row_flags(&self, session_dir: &Path, session_id: &str) -> Vec<(&'static str, bool)> {
    let pending = pending_plan_approval(&session_dir.join(PLANS_FILE), session_id);
    vec![(sidebar_state::HAS_PENDING_PLAN_APPROVAL, pending)]
  }

  fn copy_on_fork(&self, parent_dir: &Path, child_dir: &Path) -> anyhow::Result<()> {
    copy_plans_to_child(parent_dir, child_dir)
  }
}

/// Whether a plan in the registry at `plans_path` awaits approval. Each error entry is logged and contributes nothing.
fn pending_plan_approval(plans_path: &Path, session_id: &str) -> bool {
  let read = match plans::read_plans_tolerant(plans_path, session_id) {
    Ok(read) => read,
    // Python's probe raises here. A row contribution cannot fail, so the escape is logged and the row gets no flag.
    Err(error) => {
      tracing::error!(session_id, error = %error, "plan_registry_read_failed");
      return false;
    }
  };
  for entry in &read.errors {
    tracing::warn!(session_id, error = %entry.error, "plan_registry_read_failed");
  }
  read.plans.iter().any(|plan| plan.get("state").and_then(Value::as_str) == Some(AWAITING_APPROVAL_STATE))
}

/// `_copy_plans_to_child`: copies the parent's `plans.json` and every artifact it references into the child session.
///
/// The child registry names each artifact by its path relative to the child session. A missing or outside-parent
/// artifact logs a warning and does not abort the copy. A parent with no `plans.json` has nothing to copy.
pub(crate) fn copy_plans_to_child(parent_dir: &Path, child_dir: &Path) -> anyhow::Result<()> {
  let parent_dir = paths::realpath(parent_dir);
  let parent_plans_path = parent_dir.join(PLANS_FILE);
  if !plan_paths::exists(&parent_plans_path)? {
    return Ok(());
  }
  copy_plans_sync(&parent_plans_path, &parent_dir, &paths::realpath(child_dir))
}

fn copy_plans_sync(parent_plans_path: &Path, parent_dir: &Path, child_dir: &Path) -> anyhow::Result<()> {
  let mut data =
    plans::load_registry(parent_plans_path).map_err(|failure: LoadFailure| anyhow::anyhow!("{failure}"))?;
  let references = plan_references(&data, parent_dir)?;
  // Every in-parent path is reserved before any outside-parent fallback is chosen, so a fallback can never alias an
  // artifact that a later version copies.
  let mut reserved: HashSet<String> = HashSet::from([PLANS_FILE.to_string()]);
  reserved.extend(references.iter().filter_map(|reference| reference.relative.as_deref()).map(posix));

  for reference in &references {
    let (file, inside_parent) = match &reference.relative {
      Some(relative) => (relative.clone(), true),
      None => {
        let fallback = plan_paths::fallback_relative_path(parent_dir, &reference.candidate);
        let fallback_name = fallback.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let mut file = fallback.clone();
        let mut suffix_number = 1;
        while reserved.contains(&posix(&file)) || plan_paths::exists(&child_dir.join(&file))? {
          file = fallback.with_file_name(format!("{fallback_name}.outside-{suffix_number}"));
          suffix_number += 1;
        }
        reserved.insert(posix(&file));
        tracing::warn!(
          file = %reference.candidate.display(),
          relative_file = %posix(&file),
          plan = %reference.plan_id,
          v = %reference.v,
          "plan_artifact_outside_parent_on_fork"
        );
        (file, false)
      }
    };
    set_version_file(&mut data, reference, &posix(&file));
    if !inside_parent {
      continue;
    }
    let source = parent_dir.join(&file);
    if !plan_paths::exists(&source)? {
      tracing::warn!(file = %source.display(), plan = %reference.plan_id, v = %reference.v, "plan_artifact_missing_on_fork");
      continue;
    }
    let destination = child_dir.join(&file);
    fs::create_dir_all(destination.parent().expect("a copy destination has a parent"))?;
    copy2(&source, &destination)?;
  }

  fs::create_dir_all(child_dir)?;
  let text = plans::registry_text(&data);
  fsutil::atomic_write_text(&child_dir.join(PLANS_FILE), &text)?;
  Ok(())
}

/// A version of the parent registry that names a file.
struct Reference {
  plan_index: usize,
  version_index: usize,
  plan_id: Value,
  v: Value,
  /// The artifact's resolved path.
  candidate: PathBuf,
  /// The artifact's path relative to the parent session, or `None` when it lies outside the session.
  relative: Option<PathBuf>,
}

/// The versions of the parent registry that name a file, in registry order.
fn plan_references(data: &Value, parent_dir: &Path) -> anyhow::Result<Vec<Reference>> {
  let top = attribute_dict(data)?;
  let mut references = Vec::new();
  for (plan_index, plan) in registry_list(top.get("plans"))?.iter().enumerate() {
    let plan = attribute_dict(plan)?;
    for (version_index, version) in registry_list(plan.get("versions"))?.iter().enumerate() {
      let version = attribute_dict(version)?;
      let Some(file) = version.get("file").filter(|file| py_truthy(file)) else {
        continue;
      };
      let Value::String(file) = file else {
        bail!("unsupported operand type(s) for /: 'PosixPath' and '{}'", plans::py_type_name(file));
      };
      let (candidate, relative) = plan_paths::resolve_plan_file(parent_dir, file);
      references.push(Reference {
        plan_index,
        version_index,
        plan_id: plan.get("id").cloned().unwrap_or(Value::Null),
        v: version.get("v").cloned().unwrap_or(Value::Null),
        candidate,
        relative,
      });
    }
  }
  Ok(references)
}

/// The entries of a registry list as `for item in value` yields them. An absent list is empty. A dict or a string
/// yields keys or characters, which have no `get` for the reads that follow, so only an empty one is an empty list.
fn registry_list(value: Option<&Value>) -> anyhow::Result<&[Value]> {
  match value {
    None => Ok(&[]),
    Some(Value::Array(items)) => Ok(items),
    Some(Value::Object(members)) if members.is_empty() => Ok(&[]),
    Some(Value::String(text)) if text.is_empty() => Ok(&[]),
    Some(Value::Object(_) | Value::String(_)) => bail!("'str' object has no attribute 'get'"),
    Some(other) => bail!("'{}' object is not iterable", plans::py_type_name(other)),
  }
}

/// The members of a registry entry that Python reads with `.get`. Any other type has no `get`.
fn attribute_dict(value: &Value) -> anyhow::Result<&Map<String, Value>> {
  match value {
    Value::Object(members) => Ok(members),
    other => bail!("'{}' object has no attribute 'get'", plans::py_type_name(other)),
  }
}

/// `ver["file"] = file` for the version a reference names.
fn set_version_file(data: &mut Value, reference: &Reference, file: &str) {
  data["plans"][reference.plan_index]["versions"][reference.version_index]["file"] = Value::String(file.to_string());
}

/// `shutil.copy2(source, destination)`: the content, the permission bits, and the access and modification times.
fn copy2(source: &Path, destination: &Path) -> io::Result<()> {
  fs::copy(source, destination)?;
  let metadata = fs::metadata(source)?;
  let times = FileTimes::new().set_accessed(metadata.accessed()?).set_modified(metadata.modified()?);
  fs::OpenOptions::new().write(true).open(destination)?.set_times(times)
}

/// Python's truth value of a JSON value that `json.loads` produced.
fn py_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(flag) => *flag,
    Value::Number(number) => number.as_f64() != Some(0.0),
    Value::String(text) => !text.is_empty(),
    Value::Array(items) => !items.is_empty(),
    Value::Object(members) => !members.is_empty(),
  }
}

/// `Path.as_posix()`.
fn posix(path: &Path) -> String {
  path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  /// The parent and child session directories under one sessions directory.
  struct Sessions {
    _dir: tempfile::TempDir,
    parent: PathBuf,
    child: PathBuf,
  }

  impl Sessions {
    fn new() -> Self {
      let dir = tempfile::tempdir().expect("a temporary sessions directory is available");
      let parent = dir.path().join("parent");
      let child = dir.path().join("child");
      fs::create_dir_all(&child).expect("the child session directory is created");
      fs::create_dir_all(&parent).expect("the parent session directory is created");
      Self { _dir: dir, parent, child }
    }

    fn write_parent(&self, relative: &str, text: &str) {
      let path = self.parent.join(relative);
      fs::create_dir_all(path.parent().expect("a parent file has a directory")).expect("the directory is created");
      fs::write(path, text).expect("the parent file is written");
    }

    fn child_registry(&self) -> Value {
      let text = fs::read_to_string(self.child.join(PLANS_FILE)).expect("the child registry is written");
      serde_json::from_str(&text).expect("the child registry parses")
    }
  }

  fn version(v: u32, file: &str) -> Value {
    json!({"v": v, "file": file, "created_at": "2026-07-20T00:00:00+00:00", "trigger": "initial", "base": null, "note": null})
  }

  #[test]
  fn the_flag_is_set_only_while_a_plan_awaits_approval() {
    let dir = tempfile::tempdir().expect("a temporary session directory is available");
    let awaiting = dir.path().join("awaiting");
    fs::create_dir_all(&awaiting).expect("the session directory is created");
    fs::write(awaiting.join(PLANS_FILE), json!({"plans": [{"id": 1, "versions": [version(1, "a.html")]}]}).to_string())
      .expect("the registry is written");
    let approved = dir.path().join("approved");
    fs::create_dir_all(&approved).expect("the session directory is created");
    fs::write(
      approved.join(PLANS_FILE),
      json!({"plans": [{"id": 1, "versions": [version(1, "a.html")], "takeoff": {"v": 1, "at": "t"}}]}).to_string(),
    )
    .expect("the registry is written");
    let corrupt = dir.path().join("corrupt");
    fs::create_dir_all(&corrupt).expect("the session directory is created");
    fs::write(corrupt.join(PLANS_FILE), "{not valid json").expect("the registry is written");

    let flag = |session_dir: &Path| ArtifactsSidebar.row_flags(session_dir, "sess");
    assert_eq!(flag(&awaiting), vec![(sidebar_state::HAS_PENDING_PLAN_APPROVAL, true)]);
    assert_eq!(flag(&approved), vec![(sidebar_state::HAS_PENDING_PLAN_APPROVAL, false)]);
    assert_eq!(flag(&corrupt), vec![(sidebar_state::HAS_PENDING_PLAN_APPROVAL, false)]);
    assert_eq!(flag(&dir.path().join("missing")), vec![(sidebar_state::HAS_PENDING_PLAN_APPROVAL, false)]);
  }

  #[test]
  fn a_fork_copies_the_registry_and_each_artifact_in_the_parent_session() {
    let sessions = Sessions::new();
    sessions.write_parent("artifacts/plan_01.html", "<html>v1</html>");
    sessions.write_parent("artifacts/plan_02.html", "<html>v2</html>");
    let registry = json!({"plans": [{
      "id": 1,
      "title": "My Plan",
      "versions": [version(1, "artifacts/plan_01.html"), version(2, "artifacts/plan_02.html")],
      "takeoff": null,
      "closed": null,
    }]});
    fs::write(sessions.parent.join(PLANS_FILE), registry.to_string()).expect("the registry is written");

    copy_plans_to_child(&sessions.parent, &sessions.child).expect("the fork copies the registry");

    assert_eq!(sessions.child_registry(), registry);
    assert_eq!(
      fs::read_to_string(sessions.child.join("artifacts/plan_01.html")).expect("the artifact is copied"),
      "<html>v1</html>"
    );
    assert!(sessions.child.join("artifacts/plan_02.html").exists());
  }

  #[test]
  fn a_fork_skips_a_missing_artifact_and_keeps_its_version() {
    let sessions = Sessions::new();
    sessions.write_parent("artifacts/plan_01.html", "<html>present</html>");
    let registry = json!({"plans": [{
      "id": 1,
      "versions": [version(1, "artifacts/plan_01.html"), version(2, "artifacts/plan_02.html")],
    }]});
    fs::write(sessions.parent.join(PLANS_FILE), registry.to_string()).expect("the registry is written");

    copy_plans_to_child(&sessions.parent, &sessions.child).expect("the fork copies the registry");

    assert!(sessions.child.join("artifacts/plan_01.html").exists());
    assert!(!sessions.child.join("artifacts/plan_02.html").exists());
    let files: Vec<Value> = sessions.child_registry()["plans"][0]["versions"]
      .as_array()
      .expect("the versions are a list")
      .iter()
      .map(|version| version["file"].clone())
      .collect();
    assert_eq!(files, vec![json!("artifacts/plan_01.html"), json!("artifacts/plan_02.html")]);
  }

  #[test]
  fn a_fork_with_an_outside_parent_artifact_writes_the_registry_bytes_python_writes() {
    let provenance: Value = serde_json::from_str(include_str!("../tests/data/fork_fixture_source.json"))
      .expect("fixture provenance is valid");
    assert_eq!(provenance["source"], "rust/crates/features/artifacts/tests/plans_fork_fixture.py");
    assert_eq!(provenance["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
    assert_eq!(provenance["fixtures"], json!(["fork_parent_plans.json", "fork_child_plans.json"]));
    let sessions = Sessions::new();
    let parent_registry: &[u8] = include_bytes!("../tests/data/fork_parent_plans.json");
    fs::write(sessions.parent.join(PLANS_FILE), parent_registry).expect("the registry is written");
    sessions.write_parent("artifacts/collision.html", "<html>parent</html>");

    copy_plans_to_child(&sessions.parent, &sessions.child).expect("the fork copies the registry");

    let child_registry: &[u8] = include_bytes!("../tests/data/fork_child_plans.json");
    let written = fs::read(sessions.child.join(PLANS_FILE)).expect("the child registry is written");
    assert_eq!(written.as_slice(), child_registry);
    assert!(!sessions.child.join("artifacts/collision.html.outside-1").exists());
  }
}

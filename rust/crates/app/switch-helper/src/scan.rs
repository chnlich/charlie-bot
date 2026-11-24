//! `home-helper scan`: the live runs and improve loops of a home, and each live run's re-attachment declaration in
//! the running and target builds. A run re-attaches after a restart only when its backend type's registration sets
//! `restart_reattach` in both builds.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::LazyLock;

use anyhow::{Context as _, Result};
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config;
use charliebot_core::proc_identity::{PID_START_FIELD, is_same_process};
use charliebot_core::text_repr;
use charliebot_improve::command::{active_lock_path_in, loop_dir_in, read_active_lock_loop_id, read_loop_controller};
use charliebot_store::layout;
use charliebot_store::run_records::{ENDED_AT_FIELD, RUN_METADATA_NAME, STARTED_AT_FIELD, read_run_metadata};
use regex::Regex;
use serde_json::Value;

use crate::common::{exact, failed, metadata_read_failure, read_config, value_repr};
use crate::process;

/// The three registration layouts a build may have, each with the directory that holds its backends: the agents
/// crate's backends, the runtime/agents crate's backends, and each backend's own crate. A build's tree holds exactly
/// one of them.
const BACKEND_LAYOUTS: [(&str, Layout); 3] = [
  ("rust/crates/charliebot-agents/src/backends", Layout::ModRs),
  ("rust/crates/runtime/agents/src/backends", Layout::ModRs),
  ("rust/crates/backends", Layout::CrateSrc),
];

/// How a layout's backends sit under its directory.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
  /// A backend is one `mod.rs`. Its registration is that file's text, and the other files under its directory
  /// declare the const names it uses.
  ModRs,
  /// A backend is one crate. A registration may sit in any file under the crate's `src/`, and each crate holds one.
  CrateSrc,
}

/// The foundation's shared backend type names. A registration may name one by path, such as `backend_types::CODEX`,
/// and the scan reads that constant from this file in the same build. Builds older than the file lack it.
const SHARED_NAMES_FILE: &str = "rust/crates/infra/core/src/backend_types.rs";

/// The constant hops a registration's type name may take before the scan stops following them.
const MAX_CONST_HOPS: usize = 4;

/// One registration in a backend module: its type name (a string or a const) and its trait block.
static REGISTRATION: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r#"(?s)type_name:\s*(?P<name>"[^"]+"|[A-Za-z0-9_:]+)\s*,\s*traits:\s*BackendTraits\s*\{(?P<body>.*?)\}"#)
    .expect("the registration pattern")
});

/// The `restart_reattach` value at the start of a registration's own line.
static REATTACH: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"(?m)^\s*restart_reattach:\s*(true|false)\s*,?\s*$").expect("the re-attach pattern"));

/// `scan HOME REPO RUNNING_IMPL TARGET_SHA`: one `live` line per live run, one `block` line per refusal.
pub fn scan(args: &[OsString]) -> Result<(), CliExit> {
  let [home, repo, running_impl, target_sha] = exact::<4>(args)?;
  let home = PathBuf::from(home);
  let repo = PathBuf::from(repo);
  let types = backend_types(&home)?;
  let boot = process::boot_time().map_err(failed)?;
  let source_sha = running_impl.strip_prefix("rust ").unwrap_or(&running_impl);
  let sessions = config::sessions_dir_in(&home);
  let mut registrations = Registrations::default();
  for session in sorted_directories(&sessions).map_err(failed)? {
    let runs = layout::runs_dir(&session);
    for run_dir in sorted_directories(&runs).map_err(failed)? {
      let metadata = run_dir.join(RUN_METADATA_NAME);
      if !metadata.is_file() {
        continue;
      }
      let run = read_run_metadata(&metadata).map_err(|error| failed(metadata_read_failure(&metadata, error)))?;
      if matches!(run.get(ENDED_AT_FIELD), Some(value) if !value.is_null()) {
        continue;
      }
      let Some((pid, pid_start, backend)) = live_run(&run, boot).map_err(failed)? else {
        continue;
      };
      let id = text_field(&run, "id")?;
      let session_id = text_field(&run, "session_id")?;
      let display = text_repr::value_str(&backend);
      println!("live\t{session_id}\t{id}\t{display}\t{pid}\t{pid_start}");
      let Some(backend_type) = backend.as_str().and_then(|backend| types.get(backend)) else {
        println!(
          "block\tbackend {} of run {id} (pid {pid}) is not in backends.options; restart re-attachment is unknown",
          value_repr(&backend)
        );
        continue;
      };
      for (label, sha) in [("running build", source_sha), ("target build", target_sha.as_str())] {
        let (state, detail) = registrations.lookup(&repo, sha, backend_type).map_err(failed)?;
        if state != ReattachState::Enabled {
          println!(
            "block\t{display} run {id} (type {backend_type}, pid {pid}) cannot be re-attached after a restart: {label} \
             {sha} marks restart_reattach = {} ({detail})",
            state.text()
          );
        }
      }
    }
  }
  // A loop's controller runs inside the server process. A restart never resumes it
  // (rust/crates/runtime/tasks/src/recovery.rs).
  // The controller's end path saves the terminal status and then removes the session's active lock
  // (rust/crates/features/improve/src/sequence.rs). `charliebot improve stop` saves `stopped` while the current
  // iteration still runs, so the saved status is no liveness signal. The active lock is one: the controller holds it
  // from the loop's reservation until the end path removes it. It names the loop it belongs to. The named loop's
  // state carries the controller's server pid. A pid that no longer runs proves the lock is a leak from an earlier
  // server lifetime — that leak never blocks. A leak in the same server lifetime still blocks: a loop's end path
  // failed after the terminal status save and before the lock removal. The scan cannot tell it from a live
  // controller, and the operator clears it by hand.
  for session in sorted_directories(&sessions).map_err(failed)? {
    let Some(session_id) = session.file_name().and_then(|name| name.to_str()) else {
      continue;
    };
    let lock = active_lock_path_in(&home, session_id);
    if !lock.is_file() {
      continue;
    }
    let loop_id = read_active_lock_loop_id(&lock).map_err(failed)?;
    let Some(controller) = read_loop_controller(&loop_dir_in(&home, session_id, loop_id)).map_err(failed)? else {
      return Err(failed(format!(
        "the active lock of session {session_id} names loop {loop_id}, whose state file is missing"
      )));
    };
    let Ok(pid) = u32::try_from(controller.server_pid) else {
      return Err(failed(format!(
        "improve loop {loop_id} of session {session_id} has the server pid {}",
        controller.server_pid
      )));
    };
    if process::alive(pid) {
      println!(
        "block\timprove loop {loop_id} of session {session_id} (status {}) is alive: its active lock is present and \
         server pid {pid} runs",
        controller.status
      );
    }
  }
  Ok(())
}

/// The backend options' id-to-type map, which the scan uses to find a run's registered type.
fn backend_types(home: &Path) -> Result<HashMap<String, String>, CliExit> {
  let document = read_config(home).map_err(failed)?;
  let options = document
    .get(config::BACKENDS_KEY)
    .and_then(|backends| backends.get(config::BACKENDS_OPTIONS_KEY))
    .and_then(Value::as_array)
    .ok_or_else(|| CliExit::error("config.yaml has no backends.options"))?;
  let mut types = HashMap::new();
  for option in options {
    let id = option.get("id").and_then(Value::as_str).ok_or_else(|| CliExit::error("a backend option has no id"))?;
    let backend_type =
      option.get("type").and_then(Value::as_str).ok_or_else(|| CliExit::error("a backend option has no type"))?;
    types.insert(id.to_string(), backend_type.to_string());
  }
  Ok(types)
}

/// A live run's recorded identity: `(pid, pid_start, backend)`. A run whose process has ended or started before this
/// boot is not live.
fn live_run(run: &Value, boot: i64) -> Result<Option<(u32, String, Value)>> {
  let (Some(pid), Some(pid_start), Some(started_at)) = (
    run.get("pid").and_then(Value::as_u64),
    run.get(PID_START_FIELD).and_then(Value::as_str),
    run.get(STARTED_AT_FIELD).and_then(Value::as_str),
  ) else {
    return Ok(None);
  };
  let Ok(pid) = u32::try_from(pid) else {
    return Ok(None);
  };
  if !is_same_process(pid, pid_start) || !process::started_after_boot(started_at, boot)? {
    return Ok(None);
  }
  Ok(Some((pid, pid_start.to_string(), run.get("backend").cloned().unwrap_or(Value::Null))))
}

/// One run record's required text field.
fn text_field<'a>(run: &'a Value, name: &str) -> Result<&'a str, CliExit> {
  run.get(name).and_then(Value::as_str).ok_or_else(|| CliExit::error(format!("a run record has no {name}")))
}

/// The directories under `path` in file-name order. A missing directory holds none.
fn sorted_directories(path: &Path) -> std::io::Result<Vec<PathBuf>> {
  let mut directories = Vec::new();
  let entries = match fs::read_dir(path) {
    Ok(entries) => entries,
    Err(error) if error.kind() == ErrorKind::NotFound => return Ok(directories),
    Err(error) => return Err(error),
  };
  for entry in entries {
    let entry = entry?;
    if entry.file_type()?.is_dir() {
      directories.push(entry.path());
    }
  }
  directories.sort();
  Ok(directories)
}

/// One git command in the repository. A spawn failure is an error. A command's own failure is the caller's.
fn git(repo: &Path, args: &[&str]) -> Result<Output> {
  charliebot_core::git::output(repo, args).with_context(|| format!("run git {args:?}"))
}

/// One backend in a build: the files that may hold its registration, and the other files that may declare its const
/// names.
struct BackendFiles {
  /// The backend's directory in messages: its crate, or the directory of its `mod.rs`.
  name: String,
  layout: Layout,
  registrations: Vec<String>,
  siblings: Vec<String>,
}

/// A build's `restart_reattach` declaration for one backend type: the value its trait block sets, or that the
/// declaration could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReattachState {
  Enabled,
  Disabled,
  Unknown,
}

impl ReattachState {
  /// The word the scan prints for this declaration: the trait block's value, or `unknown` when it could not be read.
  pub fn text(self) -> &'static str {
    match self {
      Self::Enabled => "true",
      Self::Disabled => "false",
      Self::Unknown => "unknown",
    }
  }
}

/// The declared backend types in one build, keyed by type name.
type BuildRegistrations = HashMap<String, (ReattachState, String)>;

/// The backends of the first layout whose directory holds one in build `sha`, or the read error.
fn registration_files(repo: &Path, sha: &str) -> Result<(Vec<BackendFiles>, Option<String>)> {
  for (prefix, layout) in BACKEND_LAYOUTS {
    let listed = git(repo, &["ls-tree", "-r", "--name-only", sha, prefix])?;
    if !listed.status.success() {
      let stderr = String::from_utf8_lossy(&listed.stderr);
      return Ok((Vec::new(), Some(format!("cannot inspect build {sha}: {}", stderr.trim()))));
    }
    let all_files: Vec<String> = String::from_utf8_lossy(&listed.stdout)
      .lines()
      .filter(|line| line.ends_with(".rs"))
      .map(str::to_string)
      .collect();
    let backends = match layout {
      Layout::ModRs => mod_rs_backends(&all_files),
      Layout::CrateSrc => crate_backends(prefix, &all_files),
    };
    if !backends.is_empty() {
      return Ok((backends, None));
    }
  }
  Ok((Vec::new(), None))
}

/// The backends of a `mod.rs` layout: each `mod.rs` is one backend, and the files under its directory are its siblings.
fn mod_rs_backends(all_files: &[String]) -> Vec<BackendFiles> {
  all_files
    .iter()
    .filter(|path| path.ends_with("/mod.rs"))
    .map(|registration| {
      let directory = registration.strip_suffix("/mod.rs").expect("ends with /mod.rs");
      let siblings = all_files
        .iter()
        .filter(|path| path.starts_with(&format!("{directory}/")) && *path != registration)
        .cloned()
        .collect();
      BackendFiles {
        name: directory.to_string(),
        layout: Layout::ModRs,
        registrations: vec![registration.clone()],
        siblings,
      }
    })
    .collect()
}

/// The backends of the per-crate layout: each crate under `prefix` that has a source file under its `src/`. Every
/// source file of a crate may hold its registration. Files outside a crate's `src/`, such as its tests, are not read.
fn crate_backends(prefix: &str, all_files: &[String]) -> Vec<BackendFiles> {
  let mut crates: BTreeMap<String, Vec<String>> = BTreeMap::new();
  for path in all_files {
    let rest =
      path.strip_prefix(prefix).and_then(|rest| rest.strip_prefix('/')).expect("ls-tree lists paths under the prefix");
    let Some((krate, inner)) = rest.split_once('/') else {
      continue;
    };
    if inner.starts_with("src/") {
      crates.entry(format!("{prefix}/{krate}")).or_default().push(path.clone());
    }
  }
  crates
    .into_iter()
    .map(|(name, registrations)| BackendFiles { name, layout: Layout::CrateSrc, registrations, siblings: Vec::new() })
    .collect()
}

/// The text of each other source of a backend in build `sha`, then of the shared names file. A file the build lacks is
/// skipped: an older build has no shared names file, and its registrations spell their names directly.
fn read_sources(repo: &Path, sha: &str, paths: &[String]) -> Result<Vec<String>> {
  let mut texts = Vec::new();
  for path in paths.iter().map(String::as_str).chain(std::iter::once(SHARED_NAMES_FILE)) {
    let shown = git(repo, &["show", &format!("{sha}:{path}")])?;
    if shown.status.success() {
      texts.push(String::from_utf8_lossy(&shown.stdout).into_owned());
    }
  }
  Ok(texts)
}

/// Every backend type registration in build `sha`, read without checking out or changing its source tree. A type
/// registered twice, or a crate with no readable registration, refuses the whole build: the scan cannot tell which
/// declaration holds.
fn read_build_registrations(repo: &Path, sha: &str) -> Result<Result<BuildRegistrations, String>> {
  let (backends, error) = registration_files(repo, sha)?;
  if let Some(error) = error {
    return Ok(Err(error));
  }
  let mut registrations: BuildRegistrations = HashMap::new();
  for backend in backends {
    let mut resolved = 0;
    for file in &backend.registrations {
      let shown = git(repo, &["show", &format!("{sha}:{file}")])?;
      if !shown.status.success() {
        let stderr = String::from_utf8_lossy(&shown.stderr);
        return Ok(Err(format!("cannot read {file} from build {sha}: {}", stderr.trim())));
      }
      let source = String::from_utf8_lossy(&shown.stdout);
      // The other sources are read on the first registration whose name is a const, not a quoted literal.
      let mut others: Option<Vec<String>> = None;
      for capture in REGISTRATION.captures_iter(&source) {
        let name = &capture["name"];
        let declared = if name.starts_with('"') {
          declared_name(name, &[])
        } else {
          if others.is_none() {
            let paths: Vec<String> =
              backend.registrations.iter().filter(|path| *path != file).chain(&backend.siblings).cloned().collect();
            others = Some(read_sources(repo, sha, &paths)?);
          }
          let mut sources: Vec<&str> = vec![&*source];
          sources.extend(others.iter().flatten().map(String::as_str));
          declared_name(name, &sources)
        };
        let Some(declared) = declared else {
          continue;
        };
        resolved += 1;
        let reattach = match REATTACH.captures(&capture["body"]) {
          Some(value) => {
            let state = if &value[1] == "true" { ReattachState::Enabled } else { ReattachState::Disabled };
            (state, file.clone())
          }
          None => (ReattachState::Unknown, format!("{file} in build {sha} has no restart_reattach value")),
        };
        if let Some((_, first)) = registrations.get(&declared) {
          return Ok(Err(format!("backend type {declared} is registered twice in build {sha}: {first} and {file}")));
        }
        registrations.insert(declared, reattach);
      }
    }
    if backend.layout == Layout::CrateSrc && resolved == 0 {
      return Ok(Err(format!(
        "crate {} in build {sha} has no backend registration that the scan reads; it read {}",
        backend.name,
        backend.registrations.join(", ")
      )));
    }
  }
  Ok(Ok(registrations))
}

/// The backend type declarations of each build, each read from its tree on the first lookup in that build.
#[derive(Default)]
pub struct Registrations {
  cache: HashMap<String, Result<BuildRegistrations, String>>,
}

impl Registrations {
  /// The declaration of `backend_type` in build `sha`, read from that build's tree once per build.
  pub fn lookup(&mut self, repo: &Path, sha: &str, backend_type: &str) -> Result<(ReattachState, String)> {
    if !self.cache.contains_key(sha) {
      let build = read_build_registrations(repo, sha)?;
      self.cache.insert(sha.to_string(), build);
    }
    match self.cache.get(sha).expect("entry inserted") {
      Ok(registrations) => match registrations.get(backend_type) {
        Some(found) => Ok(found.clone()),
        None => {
          Ok((ReattachState::Unknown, format!("build {sha} has no registration for backend type {backend_type}")))
        }
      },
      Err(error) => Ok((ReattachState::Unknown, error.clone())),
    }
  }
}

/// One build's registration, read without checking out or changing its source tree.
pub fn read_registration(repo: &Path, sha: &str, backend_type: &str) -> Result<(ReattachState, String)> {
  Registrations::default().lookup(repo, sha, backend_type)
}

/// The type name a registration spells: a quoted string as written, or the literal that a const declares in one of
/// `sources`, the registration's own file first. A const whose value is another const's path, such as
/// `backend_types::CODEX`, is followed through `sources` too. `None` when no source declares the name.
fn declared_name(name: &str, sources: &[&str]) -> Option<String> {
  declared_name_within(name, sources, MAX_CONST_HOPS)
}

fn declared_name_within(name: &str, sources: &[&str], hops: usize) -> Option<String> {
  if let Some(quoted) = name.strip_prefix('"') {
    return Some(quoted.strip_suffix('"')?.to_string());
  }
  if hops == 0 {
    return None;
  }
  let ident = name.rsplit("::").next().unwrap_or(name);
  let constant = Regex::new(&format!(
    r#"const\s+{}\s*:\s*&(?:'static\s+)?str\s*=\s*("[^"]+"|[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*)\s*;"#,
    regex::escape(ident)
  ))
  .ok()?;
  sources.iter().find_map(|source| {
    let value = constant.captures(source)?.get(1)?.as_str();
    declared_name_within(value, sources, hops - 1)
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .find(|dir| dir.join(".git").exists() || dir.join("AGENTS.md").is_file())
      .expect("the repository root")
      .to_path_buf()
  }

  #[test]
  fn a_quoted_and_a_const_type_name_are_read() {
    let source = "pub const TYPE_NAME: &str = \"codex\";\nregister(TypeRegistration { type_name: TYPE_NAME, \
                  traits: BackendTraits { restart_reattach: true, }, });";
    assert_eq!(declared_name("TYPE_NAME", &[source]).as_deref(), Some("codex"));
    assert_eq!(declared_name("\"opencode\"", &[]).as_deref(), Some("opencode"));
    assert_eq!(declared_name("options::TYPE_NAME", &[source]).as_deref(), Some("codex"));
    // A const that names another const's path is followed into the shared names file, and a name that no
    // source declares stays unresolved, so the scan reports that build's type as unknown.
    let alias = "pub const TYPE_NAME: &str = backend_types::CODEX;";
    let shared = "pub const CODEX: &str = \"codex\";";
    assert_eq!(declared_name("TYPE_NAME", &[alias, shared]).as_deref(), Some("codex"));
    assert_eq!(declared_name("backend_types::CODEX", &[shared]).as_deref(), Some("codex"));
    assert_eq!(declared_name("backend_types::CODEX", &[alias]), None);
  }

  #[test]
  fn a_registration_carries_its_type_and_re_attach_value() {
    let source = "type_name: \"antigravity\",\n  traits: BackendTraits {\n    restart_reattach: false,\n  },";
    let registration = REGISTRATION.captures(source).expect("the registration");
    assert_eq!(&registration["name"], "\"antigravity\"");
    assert_eq!(&REATTACH.captures(&registration["body"]).expect("the value")[1], "false");
  }

  const CRATE_LIB: &str = "rust/crates/backends/fake/src/lib.rs";
  const CRATE_API: &str = "rust/crates/backends/fake/src/api.rs";
  const DECLARATION: &str = "pub const TYPE_NAME: &str = \"fake-type\";\n";

  /// A registration laid out as the backend crates write it: the trait block's `restart_reattach` sits on its own line.
  fn registration_source(restart_reattach: bool) -> String {
    format!(
      "pub fn register(registry: &mut Registry) {{\n  registry.add(TypeRegistration {{\n    type_name: TYPE_NAME,\n    \
       traits: BackendTraits {{\n      restart_reattach: {restart_reattach},\n    }},\n  }});\n}}\n"
    )
  }

  /// A git repository whose one commit holds `files`, each a path and its text. The directory lives as long as the
  /// returned handle.
  fn repo_with_files(files: &[(&str, String)]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("a temporary repository");
    let root = dir.path();
    for (path, text) in files {
      let file = root.join(path);
      fs::create_dir_all(file.parent().expect("a file has a directory")).expect("the file's directory");
      fs::write(&file, text).expect("the source file");
    }
    let run = |args: &[&str]| {
      let output = git(root, args).expect("git runs");
      assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    };
    run(&["init", "-q"]);
    run(&["add", "-A"]);
    run(&[
      "-c",
      "user.name=scan-test",
      "-c",
      "user.email=scan-test@example.invalid",
      "-c",
      "commit.gpgsign=false",
      "commit",
      "-q",
      "-m",
      "fixture",
    ]);
    let sha = git(root, &["rev-parse", "HEAD"]).expect("git runs");
    (dir, String::from_utf8_lossy(&sha.stdout).trim().to_string())
  }

  /// A registration in a crate's `api.rs`, with `lib.rs` only declaring the module, reads the same declaration as one
  /// in `lib.rs`. A file under the crate's `tests/` that repeats the type name is not read.
  ///
  /// Each fixture repository and each lookup spawns git, which puts the test over the unit-test time limit on CI
  /// runners, so the Integration job runs it.
  #[test]
  #[ignore = "integration"]
  fn a_registration_in_a_crate_source_file_other_than_lib_rs_reads_as_in_lib_rs() {
    for restart_reattach in [true, false] {
      let (lib_dir, lib_sha) =
        repo_with_files(&[(CRATE_LIB, format!("{DECLARATION}{}", registration_source(restart_reattach)))]);
      let (api_dir, api_sha) = repo_with_files(&[
        (CRATE_LIB, format!("{DECLARATION}mod api;\npub use api::*;\n")),
        (CRATE_API, registration_source(restart_reattach)),
        ("rust/crates/backends/fake/tests/fake.rs", registration_source(!restart_reattach)),
      ]);
      let in_lib = read_registration(lib_dir.path(), &lib_sha, "fake-type").expect("lookup").0;
      let in_api = read_registration(api_dir.path(), &api_sha, "fake-type").expect("lookup").0;
      let expected = if restart_reattach { ReattachState::Enabled } else { ReattachState::Disabled };
      assert_eq!(in_lib, expected);
      assert_eq!(in_api, in_lib);
    }
  }

  /// One type registered in two files of one crate refuses the build, and the refusal names both files.
  #[test]
  fn a_type_registered_twice_in_one_crate_refuses_naming_both_files() {
    let (dir, sha) = repo_with_files(&[
      (CRATE_LIB, format!("{DECLARATION}{}", registration_source(true))),
      (CRATE_API, registration_source(false)),
    ]);
    let (state, detail) = read_registration(dir.path(), &sha, "fake-type").expect("lookup");
    assert_eq!(state, ReattachState::Unknown);
    assert!(detail.contains(CRATE_LIB) && detail.contains(CRATE_API), "{detail}");
  }

  /// A crate whose source files hold no registration refuses the build, and the refusal names the crate and the
  /// files it read.
  #[test]
  fn a_crate_without_a_registration_refuses_naming_the_crate_and_its_files() {
    let (dir, sha) = repo_with_files(&[
      (CRATE_LIB, "mod api;\npub use api::*;\n".to_string()),
      (CRATE_API, "pub fn helper() {}\n".to_string()),
    ]);
    let (state, detail) = read_registration(dir.path(), &sha, "fake-type").expect("lookup");
    assert_eq!(state, ReattachState::Unknown);
    for path in ["rust/crates/backends/fake", CRATE_LIB, CRATE_API] {
      assert!(detail.contains(path), "{detail}");
    }
  }

  #[test]
  #[ignore = "integration"]
  fn every_registered_backend_type_resolves_in_current_tree() {
    let repo = repo_root();
    let registry = charliebot_packages::registry().expect("the registry loads");
    let types = registry.config_schema().backend_types;
    assert!(!types.is_empty(), "expected registered backend types");
    let mut registrations = Registrations::default();
    for backend_type in types {
      let (state, detail) = registrations.lookup(&repo, "HEAD", backend_type).expect("lookup succeeds");
      assert!(
        state != ReattachState::Unknown,
        "backend type {backend_type} declaration could not be resolved from tree HEAD: {detail}"
      );
    }
  }

  #[test]
  #[ignore = "integration"]
  fn genuinely_missing_backend_type_fails_closed() {
    let repo = repo_root();
    let mut registrations = Registrations::default();
    let (state, detail) = registrations.lookup(&repo, "HEAD", "nonexistent-backend-type").expect("lookup succeeds");
    assert_eq!(state, ReattachState::Unknown);
    assert!(
      detail.contains("has no registration for backend type nonexistent-backend-type"),
      "unexpected detail: {detail}"
    );
  }
}

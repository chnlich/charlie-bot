//! The cases of `tests/test_git_diff_api.py` against real git repositories, and the route answers that the memo, the
//! repository list and the 400, 422, 405 and 500 paths give.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::{Registry, RegistryError};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::api::{self, BranchKey, GitState, ManifestKey};
use crate::manifest::Manifest;
use crate::refs;

/// `_build_repo`: a repository under `workspace` with a `main` and a `feature` branch covering add, modify, delete and
/// rename.
fn build_repo(workspace: &Path) -> PathBuf {
  let repo = workspace.join("repo");
  fs::create_dir_all(&repo).expect("the repo directory exists");
  git(&repo, &["init", "-q", "-b", "main"]);
  git(&repo, &["config", "user.email", "t@t.t"]);
  git(&repo, &["config", "user.name", "t"]);
  fs::write(repo.join("keep.txt"), "line1\nline2\nline3\n").expect("the file writes");
  fs::write(repo.join("torename.txt"), "old content\nsecond\n").expect("the file writes");
  fs::write(repo.join("todelete.txt"), "to be deleted\n").expect("the file writes");
  git(&repo, &["add", "-A"]);
  git(&repo, &["commit", "-qm", "base"]);

  git(&repo, &["checkout", "-q", "-b", "feature"]);
  fs::write(repo.join("keep.txt"), "line1\nline2 changed\nline3\nline4\n").expect("the file writes");
  git(&repo, &["mv", "torename.txt", "renamed.txt"]);
  fs::write(repo.join("renamed.txt"), "old content\nsecond\nthird\n").expect("the file writes");
  git(&repo, &["rm", "-q", "todelete.txt"]);
  fs::write(repo.join("added.txt"), "brand new\nfile\n").expect("the file writes");
  git(&repo, &["add", "-A"]);
  git(&repo, &["commit", "-qm", "feature"]);
  repo
}

/// `git <args>` in `repo`, which must succeed; returns its stdout.
fn git(repo: &Path, args: &[&str]) -> String {
  let output = Command::new("git").args(args).current_dir(repo).output().expect("git runs");
  assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
  String::from_utf8(output.stdout).expect("git output is UTF-8")
}

fn path_text(path: &Path) -> &str {
  path.to_str().expect("the temp path is UTF-8")
}

fn register_memory_root(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.diff_root("memory", memory_root)
}

/// `memory.store_root.memory_dir`: the memory store under the home.
fn memory_root(cfg: &CharlieBotConfig) -> PathBuf {
  cfg.charliebot_home.join("memory")
}

/// The state of the `/api/git` routes over a home whose config names `workspace` as its one workspace directory. With
/// `with_memory_root`, the registry holds the memory package's diff root.
fn state_for(home: &Path, workspace: &Path, with_memory_root: bool) -> Arc<GitState> {
  let mut registry = Registry::new();
  if with_memory_root {
    registry.register_package("src.features.memory", register_memory_root).expect("the memory root registers");
  }
  let registry = Arc::new(registry);
  fs::create_dir_all(home).expect("the home exists");
  let config_file = home.join("config.yaml");
  fs::write(&config_file, json!({"paths": {"workspace_dirs": [path_text(workspace)]}}).to_string())
    .expect("the config writes");
  let loader_registry = Arc::clone(&registry);
  let loader_home = home.to_path_buf();
  let config =
    HotReload::new(config_file, "config_reload_failed", Box::new(move || loader_registry.load_config(&loader_home)));
  Arc::new(GitState::new(Arc::new(config), registry))
}

fn routes_for(home: &Path, workspace: &Path, with_memory_root: bool) -> Router {
  api::router(state_for(home, workspace, with_memory_root))
}

/// One response: its status, headers and body.
struct Reply {
  status: StatusCode,
  headers: HeaderMap,
  body: Vec<u8>,
}

impl Reply {
  fn json(&self) -> Value {
    serde_json::from_slice(&self.body).expect("the body is JSON")
  }
}

async fn send(router: &Router, method: Method, uri: &str) -> Reply {
  let request = Request::builder().method(method).uri(uri).body(Body::empty()).expect("the request builds");
  let response = router.clone().oneshot(request).await.expect("the router answers");
  let (parts, body) = response.into_parts();
  let body = to_bytes(body, usize::MAX).await.expect("the body reads").to_vec();
  Reply { status: parts.status, headers: parts.headers, body }
}

async fn get(router: &Router, uri: &str) -> Reply {
  send(router, Method::GET, uri).await
}

/// A request target with `params` percent-encoded into its query string.
fn target(path: &str, params: &[(&str, &str)]) -> String {
  let query: Vec<String> = params.iter().map(|(name, value)| format!("{}={}", encode(name), encode(value))).collect();
  format!("{path}?{}", query.join("&"))
}

fn encode(text: &str) -> String {
  text
    .bytes()
    .map(|byte| {
      if byte.is_ascii_alphanumeric() || b"-_.~/".contains(&byte) {
        char::from(byte).to_string()
      } else {
        format!("%{byte:02X}")
      }
    })
    .collect()
}

/// The diff-files manifest of `main...feature` in `repo`, by path.
fn manifest_rows(data: &Value) -> HashMap<&str, &Value> {
  data["files"]
    .as_array()
    .expect("files is a list")
    .iter()
    .map(|row| (row["path"].as_str().expect("a row has a path"), row))
    .collect()
}

fn files_uri(repo: &Path, base: &str, head: &str, mode: &str) -> String {
  target("/api/git/diff/files", &[("repo", path_text(repo)), ("base", base), ("head", head), ("mode", mode)])
}

fn file_uri(repo: &Path, path: &str, extra: &[(&str, &str)]) -> String {
  let mut params = vec![("repo", path_text(repo)), ("base", "main"), ("head", "feature"), ("path", path)];
  params.extend_from_slice(extra);
  target("/api/git/diff/file", &params)
}

#[tokio::test]
async fn the_manifest_lists_each_changed_file_with_its_status_and_counts() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let router = routes_for(&dir.path().join("home"), dir.path(), false);

  let reply = get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await;
  assert_eq!(reply.status, StatusCode::OK);
  let data = reply.json();
  let rows = manifest_rows(&data);

  assert_eq!(rows["added.txt"]["status"], "A");
  assert_eq!((&rows["added.txt"]["additions"], &rows["added.txt"]["deletions"]), (&json!(2), &json!(0)));
  assert_eq!(rows["keep.txt"]["status"], "M");
  assert_eq!((&rows["keep.txt"]["additions"], &rows["keep.txt"]["deletions"]), (&json!(2), &json!(1)));
  assert_eq!(rows["todelete.txt"]["status"], "D");
  // A rename is keyed by its new path with an R status and carries the pre-rename path.
  assert_eq!(rows["renamed.txt"]["status"], "R");
  assert_eq!(rows["renamed.txt"]["old_path"], "torename.txt");
  assert!(!rows.contains_key("torename.txt"));
  // A non-rename carries no old_path key.
  assert!(rows["keep.txt"].get("old_path").is_none());

  assert_eq!(data["total_files"], json!(4));
  assert_eq!(data["files"].as_array().expect("files is a list").len(), 4);
  let additions: i64 = data["files"].as_array().unwrap().iter().map(|row| row["additions"].as_i64().unwrap()).sum();
  let deletions: i64 = data["files"].as_array().unwrap().iter().map(|row| row["deletions"].as_i64().unwrap()).sum();
  assert_eq!(data["total_additions"], json!(additions));
  assert_eq!(data["total_deletions"], json!(deletions));
  assert_eq!(data["head_sha"], git(&repo, &["rev-parse", "feature"]).trim());
  assert_eq!(data["base"], "main");
  assert_eq!(data["head"], "feature");
  assert_eq!(data["mode"], "three-dot");
}

#[tokio::test]
async fn the_two_dot_mode_compares_the_tips_and_the_three_dot_mode_compares_from_the_merge_base() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  // main moves past the fork point, so the two modes diverge on main's own file.
  git(&repo, &["checkout", "-q", "main"]);
  fs::write(repo.join("main-only.txt"), "moved on\n").expect("the file writes");
  git(&repo, &["add", "-A"]);
  git(&repo, &["commit", "-qm", "main moves"]);
  let router = routes_for(&dir.path().join("home"), dir.path(), false);

  let three_dot = get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await.json();
  assert!(!manifest_rows(&three_dot).contains_key("main-only.txt"));
  let two_dot = get(&router, &files_uri(&repo, "main", "feature", "two-dot")).await.json();
  assert_eq!(manifest_rows(&two_dot)["main-only.txt"]["status"], "D");
  assert_eq!(two_dot["mode"], "two-dot");
}

#[tokio::test]
async fn a_file_diff_returns_the_unified_diff_and_its_byte_count() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let router = routes_for(&dir.path().join("home"), dir.path(), false);

  let reply = get(&router, &file_uri(&repo, "keep.txt", &[])).await;
  assert_eq!(reply.status, StatusCode::OK);
  let data = reply.json();
  assert_eq!(data["path"], "keep.txt");
  assert!(data["diff"].as_str().unwrap().contains("line2 changed"));
  assert_eq!(data["size_bytes"], json!(data["diff"].as_str().unwrap().len()));
}

#[tokio::test]
async fn a_rename_with_its_old_path_renders_as_a_rename_and_without_it_as_a_new_file() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let router = routes_for(&dir.path().join("home"), dir.path(), false);

  let paired = get(&router, &file_uri(&repo, "renamed.txt", &[("old_path", "torename.txt")])).await.json();
  let diff = paired["diff"].as_str().unwrap();
  assert!(diff.contains("rename from torename.txt"));
  assert!(diff.contains("rename to renamed.txt"));
  assert!(!diff.contains("new file"));

  // Without old_path git drops the pairing and the same file looks like a wholesale add.
  let unpaired = get(&router, &file_uri(&repo, "renamed.txt", &[])).await.json();
  assert!(unpaired["diff"].as_str().unwrap().contains("new file"));
}

#[tokio::test]
async fn a_diff_over_the_cap_answers_a_stub_and_force_loads_it() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let mut state = state_for(&dir.path().join("home"), dir.path(), false);
  // The cap is a field of the state, so a small one trips keep.txt's small diff.
  Arc::get_mut(&mut state).expect("the state is unshared").diff_cap = 10;
  let router = api::router(state);

  let stub = get(&router, &file_uri(&repo, "keep.txt", &[])).await.json();
  assert_eq!(stub["too_large"], true);
  assert_eq!(stub["path"], "keep.txt");
  assert!(stub["size_bytes"].as_u64().unwrap() > 10);
  assert!(stub.get("diff").is_none());

  let forced = get(&router, &file_uri(&repo, "keep.txt", &[("force", "true")])).await.json();
  assert!(forced.get("too_large").is_none());
  assert!(forced["diff"].as_str().unwrap().contains("line2 changed"));
}

#[tokio::test]
async fn a_moved_feature_ref_busts_the_manifest_memo_and_the_file_diff() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let state = state_for(&dir.path().join("home"), dir.path(), false);
  let router = api::router(Arc::clone(&state));
  let first = get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await.json();
  let first_file = get(&router, &file_uri(&repo, "added.txt", &[])).await.json();

  fs::write(repo.join("added.txt"), "brand new\nfile\nthird\n").expect("the file writes");
  git(&repo, &["add", "-A"]);
  git(&repo, &["commit", "-qm", "advance feature"]);

  let second = get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await.json();
  assert_ne!(second["head_sha"], first["head_sha"]);
  assert_eq!(manifest_rows(&second)["added.txt"]["additions"], json!(3));
  assert_eq!(state.manifests.len(), 2);

  let second_file = get(&router, &file_uri(&repo, "added.txt", &[])).await.json();
  assert_ne!(second_file["diff"], first_file["diff"]);
  assert!(second_file["diff"].as_str().unwrap().contains("+third"));
}

#[tokio::test]
async fn a_repeat_view_of_an_unchanged_range_reuses_its_manifest_and_its_branch_listing() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let state = state_for(&dir.path().join("home"), dir.path(), false);
  let router = api::router(Arc::clone(&state));

  let first = get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await.json();
  let again = get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await.json();
  assert_eq!(first, again);
  assert_eq!(state.manifests.len(), 1);

  let branches = get(&router, &target("/api/git/branches", &[("repo", path_text(&repo))])).await;
  let branches_again = get(&router, &target("/api/git/branches", &[("repo", path_text(&repo))])).await;
  assert_eq!(branches.body, branches_again.body);
  assert_eq!(state.branches.len(), 1);
}

#[tokio::test]
async fn a_gitattributes_file_moves_the_manifest_key() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let state = state_for(&dir.path().join("home"), dir.path(), false);
  let router = api::router(Arc::clone(&state));
  get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await;

  fs::write(repo.join(".gitattributes"), "*.txt diff=text\n").expect("the file writes");
  let reply = get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await;
  assert_eq!(reply.status, StatusCode::OK);
  assert_eq!(state.manifests.len(), 2);
}

#[test]
fn the_memos_keep_their_bounds_and_evict_the_least_recent_entry() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let state = state_for(&dir.path().join("home"), dir.path(), false);
  assert_eq!(api::MANIFEST_MEMO_LIMIT, 64);
  assert_eq!(api::BRANCH_MEMO_LIMIT, 8);

  let manifest_key = |index: usize| ManifestKey {
    repo: format!("repo-{index}"),
    base_sha: "a".to_string(),
    head_sha: "b".to_string(),
    mode: "three-dot",
    attributes: None,
  };
  for index in 0..=api::MANIFEST_MEMO_LIMIT {
    let manifest = Manifest { rows: Vec::new(), total_additions: 0, total_deletions: 0 };
    state.manifests.store(manifest_key(index), Arc::new(manifest));
  }
  assert_eq!(state.manifests.len(), api::MANIFEST_MEMO_LIMIT);
  assert!(state.manifests.get(&manifest_key(0)).is_none());
  assert!(state.manifests.get(&manifest_key(api::MANIFEST_MEMO_LIMIT)).is_some());

  let branch_key = |index: usize| BranchKey { repo: format!("repo-{index}"), signature: Vec::new() };
  for index in 0..=api::BRANCH_MEMO_LIMIT {
    state.branches.store(branch_key(index), Arc::new(Vec::new()));
  }
  assert_eq!(state.branches.len(), api::BRANCH_MEMO_LIMIT);
  assert!(state.branches.get(&branch_key(0)).is_none());
}

#[tokio::test]
async fn the_memory_store_repo_is_accepted_outside_the_workspace_dirs() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let workspace = dir.path().join("workspace");
  fs::create_dir(&workspace).expect("the workspace exists");
  let home = dir.path().join("charliebot-home");
  let repo = build_repo(&home.join("memory"));
  let router = routes_for(&home, &workspace, true);

  let reply = get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await;
  assert_eq!(reply.status, StatusCode::OK);
  let data = reply.json();
  let rows = manifest_rows(&data);
  assert!(rows.contains_key("added.txt") && rows.contains_key("renamed.txt"));
}

#[tokio::test]
async fn a_repo_outside_the_workspace_dirs_and_the_memory_store_is_refused() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(&dir.path().join("stray-repo"));
  let router = routes_for(&dir.path().join("home"), &dir.path().join("elsewhere"), false);

  let reply = get(&router, &files_uri(&repo, "main", "feature", "three-dot")).await;
  assert_eq!(reply.status, StatusCode::BAD_REQUEST);
  assert_eq!(
    reply.json(),
    json!({"detail": "repo must be under configured paths.workspace_dirs or a registered package repository"})
  );
}

#[tokio::test]
async fn the_memory_store_repo_is_refused_when_the_memory_package_is_not_registered() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let home = dir.path().join("charliebot-home");
  let workspace = dir.path().join("workspace");
  let memory_repo = build_repo(&home.join("memory"));
  let workspace_repo = build_repo(&workspace);
  let router = routes_for(&home, &workspace, false);

  let stray = get(&router, &files_uri(&memory_repo, "main", "feature", "three-dot")).await;
  assert_eq!(stray.status, StatusCode::BAD_REQUEST);
  let inside = get(&router, &files_uri(&workspace_repo, "main", "feature", "three-dot")).await;
  assert_eq!(inside.status, StatusCode::OK);
}

#[tokio::test]
async fn the_branch_list_starts_with_head_and_a_dot_git_path_lists_its_repo() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let router = routes_for(&dir.path().join("home"), dir.path(), false);

  let plain = get(&router, &target("/api/git/branches", &[("repo", path_text(&repo))])).await;
  assert_eq!(plain.status, StatusCode::OK);
  assert_eq!(plain.json(), json!(["HEAD", "feature", "main"]));
  let dot_git = get(&router, &target("/api/git/branches", &[("repo", path_text(&repo.join(".git")))])).await;
  assert_eq!(dot_git.json(), json!(["HEAD", "feature", "main"]));
}

#[tokio::test]
async fn a_branch_request_for_a_plain_directory_is_400() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let plain = dir.path().join("plain");
  fs::create_dir(&plain).expect("the directory exists");
  let router = routes_for(&dir.path().join("home"), dir.path(), false);

  let reply = get(&router, &target("/api/git/branches", &[("repo", path_text(&plain))])).await;
  assert_eq!(reply.status, StatusCode::BAD_REQUEST);
  assert_eq!(reply.json(), json!({"detail": format!("Not a git repo: {}", path_text(&plain))}));
}

#[tokio::test]
async fn the_repository_list_is_sorted_by_name_and_skips_directories_without_git() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let workspace = dir.path().join("workspace");
  let beta = workspace.join("beta");
  let alpha = workspace.join("alpha");
  for repo in [&beta, &alpha] {
    fs::create_dir_all(repo).expect("the repo directory exists");
    git(repo, &["init", "-q"]);
  }
  fs::create_dir(workspace.join("plain")).expect("the plain directory exists");
  let router = routes_for(&dir.path().join("home"), &workspace, false);

  let reply = get(&router, "/api/git/repos").await;
  assert_eq!(reply.status, StatusCode::OK);
  assert_eq!(
    reply.json(),
    json!([
      {"label": "alpha", "path": path_text(&fs::canonicalize(&alpha).unwrap())},
      {"label": "beta", "path": path_text(&fs::canonicalize(&beta).unwrap())},
    ])
  );
}

#[tokio::test]
async fn an_unknown_ref_answers_500_with_the_git_error() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let router = routes_for(&dir.path().join("home"), dir.path(), false);

  let reply = get(&router, &files_uri(&repo, "nope", "feature", "three-dot")).await;
  assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
  let detail = reply.json()["detail"].as_str().expect("the detail is text").to_string();
  assert!(detail.starts_with("fatal: ambiguous argument 'nope^{commit}'"), "detail: {detail}");
}

#[tokio::test]
async fn a_missing_required_parameter_or_a_bad_mode_is_422_with_its_location() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let router = routes_for(&dir.path().join("home"), dir.path(), false);

  let missing = get(&router, "/api/git/diff/files?base=main&head=feature").await;
  assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
  let entry = &missing.json()["detail"][0];
  assert_eq!((&entry["type"], &entry["loc"]), (&json!("missing"), &json!(["query", "repo"])));

  let bad_mode = get(&router, &files_uri(&repo, "main", "feature", "sideways")).await;
  assert_eq!(bad_mode.status, StatusCode::UNPROCESSABLE_ENTITY);
  let entry = &bad_mode.json()["detail"][0];
  assert_eq!((&entry["type"], &entry["loc"]), (&json!("literal_error"), &json!(["query", "mode"])));

  let bad_force = get(&router, &file_uri(&repo, "keep.txt", &[("force", "maybe")])).await;
  assert_eq!(bad_force.status, StatusCode::UNPROCESSABLE_ENTITY);
  let entry = &bad_force.json()["detail"][0];
  assert_eq!((&entry["type"], &entry["loc"]), (&json!("bool_parsing"), &json!(["query", "force"])));
}

#[tokio::test]
async fn head_and_other_methods_are_405_with_the_get_allow_header() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let router = routes_for(&dir.path().join("home"), dir.path(), false);
  let uri = files_uri(&repo, "main", "feature", "three-dot");

  for method in [Method::HEAD, Method::PUT] {
    let reply = send(&router, method, &uri).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(reply.headers[header::ALLOW], "GET");
  }
}

#[test]
fn the_ref_signature_moves_with_each_ref_change_and_holds_still_otherwise() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let repo = build_repo(dir.path());
  let before = refs::signature(&repo).expect("the signature reads");

  // An idle repo keeps its signature.
  assert_eq!(refs::signature(&repo).expect("the signature reads"), before);

  // A commit rewrites the branch ref.
  git(&repo, &["commit", "-q", "--allow-empty", "-m", "advance"]);
  let after_commit = refs::signature(&repo).expect("the signature reads");
  assert_ne!(after_commit, before);

  // A checkout rewrites HEAD.
  git(&repo, &["checkout", "-q", "main"]);
  assert_ne!(refs::signature(&repo).expect("the signature reads"), after_commit);

  // Packing rewrites packed-refs and removes the loose ref files, then holds still.
  let before_pack = refs::signature(&repo).expect("the signature reads");
  git(&repo, &["pack-refs", "--all"]);
  let after_pack = refs::signature(&repo).expect("the signature reads");
  assert_ne!(after_pack, before_pack);
  assert_eq!(refs::signature(&repo).expect("the signature reads"), after_pack);
}

//! The `/api/git` routes: the branch list, the repository list, and the manifest, the diff and the
//! line spans of one range.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::Response;
use charliebot_compat::routing::get_only;
use charliebot_compat::{responses, validation};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::paths;
use charliebot_registry::{FeatureContext, Registry};
use http::StatusCode;
use serde_json::{Value, json};

use crate::git::{self, DIFF_TIMEOUT};
use crate::manifest::{self, Manifest};
use crate::memo::LockedMemo;
use crate::params::{self, Params, take};
use crate::refs::{self, FileStamp, Signature};
use crate::repos;

/// The `/api/git` route paths: `router` mounts each, and `matches` claims each for the server's trailing-slash
/// redirect.
const BRANCHES_PATH: &str = "/api/git/branches";
const REPOS_PATH: &str = "/api/git/repos";
const DIFF_FILES_PATH: &str = "/api/git/diff/files";
const DIFF_FILE_PATH: &str = "/api/git/diff/file";
const DIFF_LINES_PATH: &str = "/api/git/diff/lines";

/// A per-file diff above this many bytes answers a stub unless `force` is set.
pub(crate) const DIFF_MAX_BYTES: usize = 5 * 1024 * 1024;
/// The ranges a /diff page watches across its tabs.
pub(crate) const MANIFEST_MEMO_LIMIT: usize = 64;
/// One listing per repo the branch picker has served.
pub(crate) const BRANCH_MEMO_LIMIT: usize = 8;
/// The most names `branches` answers, `HEAD` included.
const BRANCH_LIMIT: usize = 50;
const MODES: &[&str] = &["three-dot", "two-dot"];
const DEFAULT_MODE: &str = "three-dot";
/// The sides of a range a `/api/git/diff/lines` read takes.
const SIDES: &[&str] = &["base", "head"];

/// The repo, both resolved SHAs, the range mode and the `.gitattributes` signature.
#[derive(Eq, Hash, PartialEq)]
pub(crate) struct ManifestKey {
  pub(crate) repo: String,
  pub(crate) base_sha: String,
  pub(crate) head_sha: String,
  pub(crate) mode: &'static str,
  pub(crate) attributes: Option<FileStamp>,
}

/// The repo and its ref-state signature.
#[derive(Eq, Hash, PartialEq)]
pub(crate) struct BranchKey {
  pub(crate) repo: String,
  pub(crate) signature: Signature,
}

/// The state of the `/api/git` routes: the config and the registry they read, the per-file cap of `diff/file`, and
/// the two memos.
pub(crate) struct GitState {
  config: Arc<HotReload<CharlieBotConfig>>,
  registry: Arc<Registry>,
  pub(crate) diff_cap: usize,
  pub(crate) manifests: LockedMemo<ManifestKey, Arc<Manifest>>,
  pub(crate) branches: LockedMemo<BranchKey, Arc<Vec<String>>>,
}

impl GitState {
  pub(crate) fn new(config: Arc<HotReload<CharlieBotConfig>>, registry: Arc<Registry>) -> Self {
    Self {
      config,
      registry,
      diff_cap: DIFF_MAX_BYTES,
      manifests: LockedMemo::new(MANIFEST_MEMO_LIMIT),
      branches: LockedMemo::new(BRANCH_MEMO_LIMIT),
    }
  }
}

/// The `repo`, `base`, `head` and `mode` every diff route takes.
struct Range {
  repo: String,
  base: String,
  head: String,
  mode: &'static str,
}

/// The `/api/git` routes, with the feature context's config and registry as their state.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  router(Arc::new(GitState::new(Arc::clone(&features.config), Arc::clone(&features.registry))))
}

pub(crate) fn router(state: Arc<GitState>) -> Router {
  Router::new()
    .route(BRANCHES_PATH, get_only(list_branches))
    .route(REPOS_PATH, get_only(list_repos))
    .route(DIFF_FILES_PATH, get_only(diff_files))
    .route(DIFF_FILE_PATH, get_only(diff_file))
    .route(DIFF_LINES_PATH, get_only(diff_lines))
    .with_state(state)
}

pub(crate) fn matches(path: &str) -> bool {
  matches!(path, BRANCHES_PATH | REPOS_PATH | DIFF_FILES_PATH | DIFF_FILE_PATH | DIFF_LINES_PATH)
}

/// `list_branches`: the repo's branch names, `HEAD` first, most recent commit first, at most 50.
async fn list_branches(State(state): State<Arc<GitState>>, Query(query): Query<Params>) -> Response {
  let repo = match params::text(&query, "repo") {
    Ok(repo) => repo,
    Err(error) => return validation::response(&[error]),
  };
  let mut repo_path = PathBuf::from(paths::expanduser(&repo));
  // This check is wider than `resolve_repo`'s: a path that IS a `.git` directory passes, since git resolves the repo
  // from the directory it runs in.
  if !repo_path.join(".git").exists() && repo_path.file_name() != Some(OsStr::new(".git")) {
    return responses::http_exception_with_detail(StatusCode::BAD_REQUEST, &format!("Not a git repo: {repo}"));
  }
  // The signature reads `<repo>/.git`, so a `.git` path normalizes to its parent: the same repo, one memo key.
  if repo_path.file_name() == Some(OsStr::new(".git")) {
    repo_path = parent_of(&repo_path);
  }
  let lines = match state.branch_lines(&repo_path).await {
    Ok(lines) => lines,
    Err(response) => return response,
  };
  let mut seen: HashSet<&str> = HashSet::from(["HEAD"]);
  let mut branches = vec!["HEAD"];
  for line in lines.iter() {
    let name = line.trim();
    if name.is_empty() || name == "origin" || name == "HEAD" {
      continue;
    }
    if seen.insert(name) {
      branches.push(name);
    }
    if branches.len() >= BRANCH_LIMIT {
      break;
    }
  }
  responses::json(StatusCode::OK, &json!(branches))
}

/// `list_repos`: the discovered repositories as `{"label", "path"}`, sorted by label.
async fn list_repos(State(state): State<Arc<GitState>>) -> Response {
  let cfg = match state.config() {
    Ok(cfg) => cfg,
    Err(response) => return response,
  };
  let found = blocking(move || repos::discover(&cfg).map_err(io_error)).await;
  match found {
    Ok(found) => {
      let rows: Vec<Value> = found.into_iter().map(|repo| json!({"label": repo.name, "path": repo.path})).collect();
      responses::json(StatusCode::OK, &json!(rows))
    }
    Err(response) => response,
  }
}

/// `diff_files`: the per-file manifest of the range, memoized per resolved range.
async fn diff_files(State(state): State<Arc<GitState>>, Query(query): Query<Params>) -> Response {
  let mut errors = Vec::new();
  match take_range(&query, &mut errors) {
    Some(range) => answer(state.manifest_answer(&range).await),
    None => validation::response(&errors),
  }
}

/// `diff_file`: the unified diff of one file between the resolved SHAs.
async fn diff_file(State(state): State<Arc<GitState>>, Query(query): Query<Params>) -> Response {
  let mut errors = Vec::new();
  let range = take_range(&query, &mut errors);
  let path = take(params::text(&query, "path"), &mut errors);
  let force = take(params::flag(&query, "force", false), &mut errors);
  let old_path = params::optional_text(&query, "old_path");
  match (range, path, force) {
    (Some(range), Some(path), Some(force)) => {
      answer(state.file_answer(&range, &path, old_path.as_deref(), force).await)
    }
    _ => validation::response(&errors),
  }
}

/// `diff_lines`: the requested line span of one file at the range's base or head revision.
async fn diff_lines(State(state): State<Arc<GitState>>, Query(query): Query<Params>) -> Response {
  let mut errors = Vec::new();
  let range = take_range(&query, &mut errors);
  let path = take(params::text(&query, "path"), &mut errors);
  let side = take(params::one_of(&query, "side", SIDES), &mut errors);
  let start = take(params::uint(&query, "start", 1), &mut errors);
  let end = take(params::uint(&query, "end", 1), &mut errors);
  match (range, path, side, start, end) {
    (Some(range), Some(path), Some(side), Some(start), Some(end)) => {
      answer(state.lines_answer(&range, &path, side, start, end).await)
    }
    _ => validation::response(&errors),
  }
}

/// Reads the four parameters every diff route shares, in declaration order.
fn take_range(query: &Params, errors: &mut Vec<validation::FieldError>) -> Option<Range> {
  let repo = take(params::text(query, "repo"), errors);
  let base = take(params::text(query, "base"), errors);
  let head = take(params::text(query, "head"), errors);
  let mode = take(params::literal(query, "mode", DEFAULT_MODE, MODES), errors);
  match (repo, base, head, mode) {
    (Some(repo), Some(base), Some(head), Some(mode)) => Some(Range { repo, base, head, mode }),
    _ => None,
  }
}

impl GitState {
  async fn manifest_answer(&self, range: &Range) -> Result<Value, Response> {
    let cfg = self.config()?;
    let repo_path = resolve_repo(&range.repo, &cfg, &self.registry)?;
    let shas = git::resolve_commits(&repo_path, &[range.base.as_str(), range.head.as_str()]).await?;
    let [base_sha, head_sha] = two_shas(shas)?;
    let key = ManifestKey {
      repo: paths::display(&repo_path),
      base_sha,
      head_sha: head_sha.clone(),
      mode: range.mode,
      attributes: refs::attributes_signature(&repo_path).map_err(io_error)?,
    };
    let manifest = match self.manifests.get(&key) {
      Some(manifest) => manifest,
      None => {
        // The range is built from the refs as given, as `diff_files` builds it.
        let spec = manifest::range_spec(&range.base, &range.head, range.mode);
        let built = Arc::new(manifest::build(&repo_path, &spec).await?);
        self.manifests.store(key, Arc::clone(&built));
        built
      }
    };
    Ok(json!({
      "files": manifest.rows,
      "base": range.base,
      "head": range.head,
      "head_sha": head_sha,
      "mode": range.mode,
      "total_files": manifest.rows.len(),
      "total_additions": manifest.total_additions,
      "total_deletions": manifest.total_deletions,
    }))
  }

  /// `diff_file`: the diff between the resolved SHAs, restricted to `path` and, for a rename, to `old_path` too. A diff
  /// above the cap answers `too_large` unless `force` is set.
  async fn file_answer(
    &self,
    range: &Range,
    path: &str,
    old_path: Option<&str>,
    force: bool,
  ) -> Result<Value, Response> {
    let cfg = self.config()?;
    let repo_path = resolve_repo(&range.repo, &cfg, &self.registry)?;
    let shas = git::resolve_commits(&repo_path, &[range.base.as_str(), range.head.as_str()]).await?;
    let [base_sha, head_sha] = two_shas(shas)?;
    // The range runs over the resolved SHAs, so a ref that moves before git starts cannot pair one resolution with
    // another. Both endpoints of a rename go to git, which keeps the pairing.
    let spec = manifest::range_spec(&base_sha, &head_sha, range.mode);
    let mut args = vec!["diff", spec.as_str(), "--"];
    if let Some(old_path) = old_path.filter(|old_path| !old_path.is_empty()) {
      args.push(old_path);
    }
    args.push(path);
    let diff = git::run(&repo_path, &args, DIFF_TIMEOUT, "git diff failed").await?;
    let size_bytes = diff.len();
    if !force && size_bytes > self.diff_cap {
      return Ok(json!({"too_large": true, "size_bytes": size_bytes, "path": path}));
    }
    // The two revisions the served diff was built between. A client passes them back to `diff_lines`, whose read
    // then answers the same file content the hunk headers name even after a ref moves.
    let head_revision = head_sha.clone();
    let base_revision =
      if range.mode == "three-dot" { git::merge_base(&repo_path, &base_sha, &head_sha).await? } else { base_sha };
    let base_path = old_path.filter(|old_path| !old_path.is_empty()).unwrap_or(path);
    let base_total = revision_line_count(&repo_path, &base_revision, base_path).await?;
    let head_total = revision_line_count(&repo_path, &head_revision, path).await?;
    Ok(json!({
      "diff": diff,
      "path": path,
      "size_bytes": size_bytes,
      "base_sha": base_revision,
      "head_sha": head_revision,
      "base_total": base_total,
      "head_total": head_total,
    }))
  }

  /// `diff_lines`: the line span `start..=end` of `path` at the range's base or head revision. An `end` past the
  /// file's last line answers the lines through the end.
  async fn lines_answer(&self, range: &Range, path: &str, side: &str, start: i64, end: i64) -> Result<Value, Response> {
    let cfg = self.config()?;
    let repo_path = resolve_repo(&range.repo, &cfg, &self.registry)?;
    let revision = resolved_revision(&repo_path, range, side).await?;
    let bytes = git::show_bytes(&repo_path, &revision, path).await?.ok_or_else(|| {
      responses::http_exception_with_detail(StatusCode::NOT_FOUND, &format!("{path} does not exist at {revision}"))
    })?;
    if bytes.contains(&0) {
      return Err(responses::http_exception_with_detail(StatusCode::BAD_REQUEST, &format!("binary file: {path}")));
    }
    let text = git::decode(&bytes)?;
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
      lines.pop();
    }
    let total = lines.len() as i64;
    let end = end.min(total);
    let rows: Vec<Value> = if start <= end {
      lines[(start - 1) as usize..end as usize]
        .iter()
        .enumerate()
        .map(|(offset, line)| json!({"line": start + offset as i64, "text": line}))
        .collect()
    } else {
      Vec::new()
    };
    Ok(json!({"path": path, "side": side, "start": start, "end": end, "total": total, "lines": rows}))
  }
}

/// The revision a side's lines come from: the head commit, or for a three-dot range's base side, the merge base
/// the range's diff diffs against.
async fn resolved_revision(repo: &Path, range: &Range, side: &str) -> Result<String, Response> {
  if side == "head" {
    let shas = git::resolve_commits(repo, &[range.head.as_str()]).await?;
    return shas.into_iter().next().ok_or_else(|| missing_commit(range.head.as_str()));
  }
  if range.mode == "three-dot" {
    return git::merge_base(repo, &range.base, &range.head).await;
  }
  let shas = git::resolve_commits(repo, &[range.base.as_str()]).await?;
  shas.into_iter().next().ok_or_else(|| missing_commit(range.base.as_str()))
}

/// The internal error for a `rev-parse` that answered fewer SHAs than it was given refs.
fn missing_commit(reference: &str) -> Response {
  responses::internal_error("ValueError", format!("git rev-parse answered no commit for {reference}"))
}

/// The line count of `path` at `revision`, 0 when the path is absent there.
async fn revision_line_count(repo: &Path, revision: &str, path: &str) -> Result<usize, Response> {
  Ok(git::show_bytes(repo, revision, path).await?.map(|bytes| git::count_lines(&bytes)).unwrap_or(0))
}

/// The repository `repo` names, resolved, when it holds `.git` and lies under a
/// workspace directory or a registered diff root.
fn resolve_repo(repo: &str, cfg: &CharlieBotConfig, registry: &Registry) -> Result<PathBuf, Response> {
  let repo_path = paths::realpath(&PathBuf::from(paths::expanduser(repo)));
  if !repo_path.join(".git").exists() {
    return Err(responses::http_exception_with_detail(StatusCode::BAD_REQUEST, &format!("Not a git repo: {repo}")));
  }
  let mut roots: Vec<PathBuf> =
    cfg.paths.workspace_dirs.iter().map(|dir| paths::realpath(&PathBuf::from(paths::expanduser(dir)))).collect();
  roots.extend(registry.diff_roots().map(|(_, root)| paths::realpath(&root(cfg))));
  if !roots.iter().any(|root| repo_path.starts_with(root)) {
    return Err(responses::http_exception_with_detail(
      StatusCode::BAD_REQUEST,
      "repo must be under configured paths.workspace_dirs or a registered package repository",
    ));
  }
  Ok(repo_path)
}

impl GitState {
  /// The repo's listing, re-run only when its ref state moved.
  async fn branch_lines(&self, repo: &Path) -> Result<Arc<Vec<String>>, Response> {
    let owned = repo.to_path_buf();
    let signature = blocking(move || refs::signature(&owned).map_err(io_error)).await?;
    let key = BranchKey { repo: paths::display(repo), signature };
    if let Some(lines) = self.branches.get(&key) {
      return Ok(lines);
    }
    let lines = Arc::new(git::branch_lines(repo).await?);
    self.branches.store(key, Arc::clone(&lines));
    Ok(lines)
  }

  fn config(&self) -> Result<Arc<CharlieBotConfig>, Response> {
    self.config.get().map_err(|error| responses::internal_error("ConfigError", error.to_string()))
  }
}

fn two_shas(shas: Vec<String>) -> Result<[String; 2], Response> {
  shas.try_into().map_err(|shas: Vec<String>| {
    responses::internal_error("ValueError", format!("expected 2 commit SHAs, got {}", shas.len()))
  })
}

/// The answer of a route whose body is one JSON value. An error answers as its own response.
fn answer(result: Result<Value, Response>) -> Response {
  match result {
    Ok(value) => responses::json(StatusCode::OK, &value),
    Err(response) => response,
  }
}

/// An unhandled `OSError`: the plain 500.
fn io_error(error: io::Error) -> Response {
  responses::internal_error("OSError", error.to_string())
}

/// Runs blocking file work off the event loop, for the listing and the scan.
async fn blocking<T, F>(work: F) -> Result<T, Response>
where
  F: FnOnce() -> Result<T, Response> + Send + 'static,
  T: Send + 'static,
{
  tokio::task::spawn_blocking(work)
    .await
    .map_err(|error| responses::internal_error("RuntimeError", error.to_string()))?
}

/// `Path.parent` of a `.git` path. The parent of a bare relative `.git` is the current directory.
fn parent_of(path: &Path) -> PathBuf {
  match path.parent() {
    Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
    _ => PathBuf::from("."),
  }
}

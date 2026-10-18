//! The request-time template engine (`src/runtime/templating.py` over Jinja2 as starlette configures it).
//!
//! Each render builds a fresh minijinja environment, so a template, a page part or a template directory
//! that changes on disk takes effect at the next render without a restart, as Jinja2's auto-reload does.
//! The environment is charliebot-compat's Jinja2 environment (`jinja::environment`), with this page's
//! globals and `page_parts` added. The search path is `web/templates`, then each page package's
//! `templates/`, in registration order.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use charliebot_compat::jinja;
use charliebot_core::{git, pyfmt};
use charliebot_registry::pages::{self, package_directory};
use minijinja::{Environment, Error, ErrorKind, Value};
use sha1::{Digest, Sha1};

/// A zero-argument template global: the name and the value the call returns.
pub type TemplateGlobal = (&'static str, Value);

/// The template engine of one repository checkout.
pub struct Templating {
  repo_root: PathBuf,
  /// The registered page packages in registration order, whose `templates/` and `static/` the page uses.
  packages: Vec<&'static str>,
  /// `web/templates`, then each page package's `templates/`.
  directories: Vec<PathBuf>,
  /// Whether the unique-name check has passed once (Python checks when it first builds the engine).
  names_checked: AtomicBool,
  /// `git_version()`, computed at the first render that asks for it.
  git_version: OnceLock<String>,
  asset_digest: Mutex<AssetDigestState>,
}

#[derive(Default)]
struct AssetDigestState {
  signature: Vec<(String, i64, u64)>,
  digests: HashMap<String, [u8; 20]>,
  digest: String,
}

impl Templating {
  /// The engine of `repo_root` for the page packages `packages`, in registration order.
  pub fn new(repo_root: &Path, packages: &[&'static str]) -> Templating {
    let mut directories = vec![repo_root.join("web").join("templates")];
    directories.extend(packages.iter().map(|package| package_directory(repo_root, package).join("templates")));
    Templating {
      repo_root: repo_root.to_path_buf(),
      packages: packages.to_vec(),
      directories,
      names_checked: AtomicBool::new(false),
      git_version: OnceLock::new(),
      asset_digest: Mutex::new(AssetDigestState::default()),
    }
  }

  /// `templating.git_version()`: `"<short sha> · <MM-DD>"` of the checkout's HEAD, or `""` when git fails.
  pub fn git_version(&self) -> &str {
    self.git_version.get_or_init(|| {
      let version = git::head_sha(&self.repo_root, git::VERSION_TIMEOUT).and_then(|sha| {
        let date =
          git::run(&self.repo_root, &["log", "-1", "--format=%cd", "--date=format:%m-%d"], git::VERSION_TIMEOUT)?;
        Some(format!("{sha} \u{b7} {date}"))
      });
      version.unwrap_or_else(|| {
        tracing::warn!("git_version_failed");
        String::new()
      })
    })
  }

  /// `templating.static_asset_version()`: the git version plus the served static trees' content digest.
  pub fn static_asset_version(&self) -> io::Result<String> {
    let git_part = self.git_version().replace(" \u{b7} ", "-").replace(' ', "-");
    Ok(format!("{git_part}-{}", self.asset_tree_digest()?))
  }

  /// `_asset_tree_digest`: sha1 over each served static file's URL path and content sha1, in path
  /// order, as 12 hex digits. A file whose `(mtime_ns, size)` has not moved keeps its memoized sha1.
  fn asset_tree_digest(&self) -> io::Result<String> {
    let static_root = self.repo_root.join("web").join("static");
    if !static_root.is_dir() {
      return Ok(String::new());
    }
    let mut pairs: Vec<(String, i64, u64)> = Vec::new();
    let mut paths: HashMap<String, PathBuf> = HashMap::new();
    walk_static_tree(&static_root, "", &mut pairs, &mut paths)?;
    for package in &self.packages {
      let directory = package_directory(&self.repo_root, package).join("static");
      if directory.is_dir() {
        let url_path = package.trim_start_matches("src.").replace('.', "/");
        walk_static_tree(&directory, &format!("{url_path}/"), &mut pairs, &mut paths)?;
      }
    }
    pairs.sort();
    let mut state = self.asset_digest.lock().expect("the asset digest lock is never poisoned");
    if state.signature == pairs {
      return Ok(state.digest.clone());
    }
    let old_stats: HashMap<&str, (i64, u64)> =
      state.signature.iter().map(|(rel, mtime, size)| (rel.as_str(), (*mtime, *size))).collect();
    let mut digests = state.digests.clone();
    for (rel, mtime, size) in &pairs {
      if digests.contains_key(rel) && old_stats.get(rel.as_str()) == Some(&(*mtime, *size)) {
        continue;
      }
      digests.insert(rel.clone(), Sha1::digest(std::fs::read(&paths[rel])?).into());
    }
    let mut combined = Sha1::new();
    for (rel, _, _) in &pairs {
      combined.update(rel.as_bytes());
      combined.update(b"\0");
      combined.update(digests[rel]);
    }
    let value: String =
      combined.finalize().iter().map(|byte| format!("{byte:02x}")).collect::<String>()[..12].to_string();
    *state = AssetDigestState { signature: pairs, digests, digest: value.clone() };
    Ok(value)
  }

  /// Render template `name` with `context` and the zero-argument `globals`.
  pub fn render(
    &self,
    name: &str,
    context: &serde_json::Value,
    globals: Vec<TemplateGlobal>,
  ) -> Result<String, RenderError> {
    if !self.names_checked.load(Ordering::Acquire) {
      assert_unique_template_names(&self.directories).map_err(RenderError::DuplicateName)?;
      self.names_checked.store(true, Ordering::Release);
    }
    let environment = self.environment(globals);
    let template = environment.get_template(name).map_err(RenderError::Template)?;
    template.render(Value::from_serialize(context)).map_err(RenderError::Template)
  }

  fn environment(&self, globals: Vec<TemplateGlobal>) -> Environment<'static> {
    let mut environment = jinja::environment(jinja::file_system_loader(self.directories.clone()));
    let repo_root = self.repo_root.clone();
    let packages = self.packages.clone();
    environment.add_function("page_parts", move |slot: String| {
      pages::page_parts(&repo_root, &packages, &slot)
        .map_err(|message| Error::new(ErrorKind::InvalidOperation, format!("ValueError: {message}")))
    });
    for (name, value) in globals {
      environment.add_function(name, move || value.clone());
    }
    environment
  }
}

/// Why a render failed.
#[derive(Debug)]
pub enum RenderError {
  /// Two template directories hold one template name (`_assert_unique_template_names`).
  DuplicateName(String),
  Template(Error),
}

impl RenderError {
  /// The Python exception class the same failure raises, for the request log.
  pub fn kind(&self) -> &'static str {
    match self {
      RenderError::DuplicateName(_) => "ValueError",
      RenderError::Template(error) if error.kind() == ErrorKind::TemplateNotFound => "TemplateNotFound",
      RenderError::Template(error) if error.kind() == ErrorKind::UndefinedError => "UndefinedError",
      RenderError::Template(_) => "TemplateError",
    }
  }
}

impl std::fmt::Display for RenderError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      RenderError::DuplicateName(message) => formatter.write_str(message),
      RenderError::Template(error) => write!(formatter, "{error:#}"),
    }
  }
}

fn walk_static_tree(
  directory: &Path,
  prefix: &str,
  pairs: &mut Vec<(String, i64, u64)>,
  paths: &mut HashMap<String, PathBuf>,
) -> io::Result<()> {
  use std::os::unix::fs::MetadataExt;
  for entry in std::fs::read_dir(directory)? {
    let entry = entry?;
    let name = entry.file_name().into_string().map_err(|name| {
      io::Error::new(io::ErrorKind::InvalidData, format!("static file name is not UTF-8: {}", name.to_string_lossy()))
    })?;
    let rel = format!("{prefix}{name}");
    // os.DirEntry.is_dir/is_file follow symlinks and answer False for a dangling one.
    let metadata = match std::fs::metadata(entry.path()) {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
      Err(error) => return Err(error),
    };
    if metadata.is_dir() {
      walk_static_tree(&entry.path(), &format!("{rel}/"), pairs, paths)?;
    } else if metadata.is_file() {
      let mtime_ns = metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec();
      pairs.push((rel.clone(), mtime_ns, metadata.size()));
      paths.insert(rel, entry.path());
    }
  }
  Ok(())
}

/// `_assert_unique_template_names`: a name in two directories would let the first hide the second.
fn assert_unique_template_names(directories: &[PathBuf]) -> Result<(), String> {
  let mut holder: HashMap<String, &Path> = HashMap::new();
  for directory in directories {
    let mut names = Vec::new();
    collect_html_names(directory, "", &mut names);
    names.sort();
    for name in names {
      if let Some(first) = holder.get(&name) {
        return Err(format!(
          "template {} is in both {} and {}",
          pyfmt::str_repr(&name),
          first.display(),
          directory.display()
        ));
      }
      holder.insert(name, directory);
    }
  }
  Ok(())
}

/// The `*.html` names under `directory` as `Path.rglob` finds them: symlinked directories are not entered.
fn collect_html_names(directory: &Path, prefix: &str, names: &mut Vec<String>) {
  let Ok(entries) = std::fs::read_dir(directory) else {
    return;
  };
  for entry in entries.flatten() {
    let name = entry.file_name().to_string_lossy().into_owned();
    let rel = format!("{prefix}{name}");
    if rel.ends_with(".html") {
      names.push(rel.clone());
    }
    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
      collect_html_names(&entry.path(), &format!("{rel}/"), names);
    }
  }
}

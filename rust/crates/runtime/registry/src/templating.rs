//! The request-time template engine (`src/runtime/templating.py` over Jinja2 as starlette configures it).
//!
//! The server builds one `Templating` at startup and inserts it into the startup state. A feature reads it with
//! `startup.require::<Templating>()` and renders a page with `render_page`, as Starlette's `TemplateResponse` does.
//!
//! Each render builds a fresh minijinja environment, so a template, a page part or a template directory
//! that changes on disk takes effect at the next render without a restart, as Jinja2's auto-reload does.
//! The environment is charliebot-compat's Jinja2 environment (`jinja::environment`), with the registry's template
//! globals and `page_parts` added. The search path is `web/templates`, then each page package's `templates/`, in
//! registration order.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use axum::http::HeaderValue;
use axum::http::header;
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_compat::{jinja, responses};
use charliebot_core::{git, pyfmt};
use ignore::WalkBuilder;
use minijinja::{Environment, Error, ErrorKind, Value};
use serde_json::json;
use sha1::{Digest, Sha1};

use crate::context::FeatureContext;
use crate::pages::{self, package_directory};

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

  /// `TemplateResponse(request, name, context)`: render `name` with `context`, the request under `request`, the
  /// template globals of `features.registry` and the page parts. A failing global fails the render.
  pub fn render_page(
    &self,
    features: &FeatureContext,
    name: &str,
    context: &serde_json::Value,
    request: &Parts,
  ) -> Result<Response, RenderError> {
    self.render_page_with_substitutions(features, name, context, request, &[])
  }

  /// Render a page, then write each `(placeholder, text)` pair's text where the page carries its placeholder.
  ///
  /// The template engine copies every value it writes, and a page can carry one value far larger than its template.
  /// Such a value renders as a short placeholder and is substituted into the finished HTML, so one request copies it
  /// once instead of once per engine stage. A placeholder that the rendered page does not carry fails the render,
  /// because serving the placeholder would break the page's script.
  pub fn render_page_with_substitutions(
    &self,
    features: &FeatureContext,
    name: &str,
    context: &serde_json::Value,
    request: &Parts,
    substitutions: &[(&str, &str)],
  ) -> Result<Response, RenderError> {
    let html = self.render_page_html(features, name, context, request, substitutions)?;
    // `no-store` sends every back navigation and tab restore to the server, so the page never runs
    // JavaScript from a cached copy that references an older static asset version.
    let mut response = responses::html(html);
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
  }

  /// Render a page and each `(placeholder, text)` substitution as the finished HTML.
  ///
  /// A caller that builds its own response reads the page this way, so the response headers have one owner.
  pub fn render_page_html(
    &self,
    features: &FeatureContext,
    name: &str,
    context: &serde_json::Value,
    request: &Parts,
    substitutions: &[(&str, &str)],
  ) -> Result<String, RenderError> {
    let globals = features
      .registry
      .template_globals()
      .map(|(global_name, global)| global(features).map(|value| (global_name, value)).map_err(RenderError::Global))
      .collect::<Result<Vec<_>, _>>()?;
    let mut context = context.clone();
    context
      .as_object_mut()
      .expect("a page context is a JSON object")
      .insert("request".to_string(), request_context(request));
    let mut html = self.render(name, &context, globals)?;
    for (placeholder, text) in substitutions {
      if !html.contains(placeholder) {
        let message = format!("{name}: the rendered page does not carry the placeholder {placeholder:?}");
        return Err(RenderError::Template(Error::new(ErrorKind::InvalidOperation, message)));
      }
      html = html.replace(placeholder, text);
    }
    Ok(html)
  }

  /// Render template `name` with `context` and the zero-argument `globals`.
  fn render(
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
  /// A template global raised; the page fails with its message.
  Global(String),
  Template(Error),
}

impl RenderError {
  /// The Python exception class the same failure raises, for the request log.
  pub fn kind(&self) -> &'static str {
    match self {
      RenderError::DuplicateName(_) | RenderError::Global(_) => "ValueError",
      RenderError::Template(error) if error.kind() == ErrorKind::TemplateNotFound => "TemplateNotFound",
      RenderError::Template(error) if error.kind() == ErrorKind::UndefinedError => "UndefinedError",
      RenderError::Template(_) => "TemplateError",
    }
  }
}

impl std::fmt::Display for RenderError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      RenderError::DuplicateName(message) | RenderError::Global(message) => formatter.write_str(message),
      RenderError::Template(error) => write!(formatter, "{error:#}"),
    }
  }
}

impl std::error::Error for RenderError {}

/// The `request` a page template sees: the method, the raw path and the raw query string, a subset of Starlette's
/// `Request`.
fn request_context(request: &Parts) -> serde_json::Value {
  json!({"method": request.method.as_str(), "path": request.uri.path(), "query": request.uri.query().unwrap_or("")})
}

/// Python's `os.DirEntry.is_dir` and `is_file` answer False for a dangling symlink, so the static walk skips it.
fn is_not_found(error: &ignore::Error) -> bool {
  error.io_error().is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Build a recursive walk that applies no hidden-file or ignore-file filters.
fn filesystem_walk(directory: &Path, follow_links: bool) -> WalkBuilder {
  let mut builder = WalkBuilder::new(directory);
  builder.min_depth(Some(1)).follow_links(follow_links).hidden(false).standard_filters(false).parents(true);
  builder
}

/// The static trees under `directory`, following symlinked directories, with each file's `(rel, mtime, size)` and
/// path. `rel` is `prefix` plus the file's path under `directory`.
fn walk_static_tree(
  directory: &Path,
  prefix: &str,
  pairs: &mut Vec<(String, i64, u64)>,
  paths: &mut HashMap<String, PathBuf>,
) -> io::Result<()> {
  use std::os::unix::fs::MetadataExt;
  for entry in filesystem_walk(directory, true).build() {
    let entry = match entry {
      Ok(entry) => entry,
      Err(error) if is_not_found(&error) => continue,
      Err(error) => return Err(io::Error::other(error)),
    };
    let relative = entry.path().strip_prefix(directory).expect("a walk yields paths under its root").to_path_buf();
    let rel = relative.to_str().ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("static file name is not UTF-8: {}", relative.to_string_lossy()),
      )
    })?;
    let rel = format!("{prefix}{rel}");
    let metadata = match entry.metadata() {
      Ok(metadata) => metadata,
      Err(error) if error.io_error().is_some_and(|error| error.kind() == io::ErrorKind::NotFound) => continue,
      Err(error) => return Err(io::Error::other(error)),
    };
    if metadata.is_file() {
      let mtime_ns = metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec();
      pairs.push((rel.clone(), mtime_ns, metadata.size()));
      paths.insert(rel, entry.path().to_path_buf());
    }
  }
  Ok(())
}

/// `_assert_unique_template_names`: a name in two directories would let the first hide the second.
fn assert_unique_template_names(directories: &[PathBuf]) -> Result<(), String> {
  let mut holder: HashMap<String, &Path> = HashMap::new();
  for directory in directories {
    let mut names = Vec::new();
    collect_html_names(directory, &mut names);
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

/// The `*.html` names under `directory` as `Path.rglob` finds them: symlinked directories are not entered, and an
/// unreadable entry is skipped as `rglob` skips it.
fn collect_html_names(directory: &Path, names: &mut Vec<String>) {
  for entry in filesystem_walk(directory, false).build() {
    let entry = match entry {
      Ok(entry) => entry,
      Err(error) => {
        tracing::debug!(error = %error, "template_name_walk_skipped");
        continue;
      }
    };
    let relative = entry.path().strip_prefix(directory).expect("a walk yields paths under its root").to_path_buf();
    let rel = relative.to_string_lossy().into_owned();
    if rel.ends_with(".html") {
      names.push(rel);
    }
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::sync::Arc;

  use axum::http::header::CONTENT_TYPE;
  use axum::http::{Request, StatusCode};

  use super::*;
  use crate::context::features_over;
  use crate::registry::{Registry, RegistryError};

  fn notes_label(_features: &FeatureContext) -> Result<Value, String> {
    Ok(Value::from("synthetic-label"))
  }

  fn register_notes(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.template_global("notes_label", notes_label)
  }

  #[tokio::test]
  async fn render_page_renders_a_package_template_with_request_and_a_global() {
    let root = tempfile::tempdir().unwrap();
    let templates = package_directory(root.path(), "src.features.notes").join("templates");
    fs::create_dir_all(&templates).unwrap();
    fs::write(
      templates.join("notes.html"),
      "{{ notes_label() }} {{ request.method }} {{ request.path }}?{{ request.query }}",
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.features.notes", register_notes).unwrap();
    let home = tempfile::tempdir().unwrap();
    let features = features_over(home.path(), Arc::new(registry));
    features.startup.insert(Templating::new(root.path(), features.registry.page_packages())).unwrap();
    let (parts, _) = Request::builder().method("GET").uri("/notes?page=2").body(()).unwrap().into_parts();

    let templating = features.startup.require::<Templating>().unwrap();
    let response = templating.render_page(&features, "notes.html", &json!({}), &parts).unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body, "synthetic-label GET /notes?page=2");
  }

  /// A substitution writes the text where the page carries the placeholder, and an absent placeholder fails the
  /// render instead of serving the page with the placeholder in place of its text.
  #[tokio::test]
  async fn render_page_substitutes_a_placeholder_and_refuses_a_page_without_it() {
    let root = tempfile::tempdir().unwrap();
    let templates = package_directory(root.path(), "src.features.notes").join("templates");
    fs::create_dir_all(&templates).unwrap();
    fs::write(templates.join("splice.html"), "const DATA = {{ payload|safe }};").unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.features.notes", register_notes).unwrap();
    let home = tempfile::tempdir().unwrap();
    let features = features_over(home.path(), Arc::new(registry));
    features.startup.insert(Templating::new(root.path(), features.registry.page_packages())).unwrap();
    let (parts, _) = Request::builder().method("GET").uri("/splice").body(()).unwrap().into_parts();
    let context = json!({"payload": "PLACEHOLDER"});
    let templating = features.startup.require::<Templating>().unwrap();

    let response = templating
      .render_page_with_substitutions(
        &features,
        "splice.html",
        &context,
        &parts,
        &[("PLACEHOLDER", "[{\"name\": \"Synthetic\"}]")],
      )
      .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body, "const DATA = [{\"name\": \"Synthetic\"}];");

    let error = templating
      .render_page_with_substitutions(&features, "splice.html", &context, &parts, &[("ABSENT", "[]")])
      .unwrap_err();
    assert_eq!(error.kind(), "TemplateError");
  }

  #[test]
  fn asset_digest_hashes_each_static_file_by_url_path_in_path_order() {
    let root = tempfile::tempdir().unwrap();
    let static_root = root.path().join("web").join("static");
    fs::create_dir_all(static_root.join("sub")).unwrap();
    fs::write(static_root.join(".gitignore"), "excluded.js\n").unwrap();
    fs::write(static_root.join(".hidden.js"), "hidden").unwrap();
    fs::write(static_root.join("a.js"), "alpha").unwrap();
    fs::write(static_root.join("excluded.js"), "excluded").unwrap();
    fs::write(static_root.join("sub").join("b.css"), "beta").unwrap();
    std::os::unix::fs::symlink(static_root.join("sub"), static_root.join("linked")).unwrap();
    std::os::unix::fs::symlink(static_root.join("missing"), static_root.join("dangling")).unwrap();
    let package_static = package_directory(root.path(), "src.features.notes").join("static");
    fs::create_dir_all(&package_static).unwrap();
    fs::write(package_static.join("x.txt"), "gamma").unwrap();
    let templating = Templating::new(root.path(), &["src.features.notes"]);
    // Each file contributes its URL path and its content's sha1, in path order; a dangling link contributes nothing
    // and a symlinked directory's files count under the link's name. Hidden and git-ignored files still count.
    let mut combined = Sha1::new();
    for (rel, content) in [
      (".gitignore", "excluded.js\n"),
      (".hidden.js", "hidden"),
      ("a.js", "alpha"),
      ("excluded.js", "excluded"),
      ("features/notes/x.txt", "gamma"),
      ("linked/b.css", "beta"),
      ("sub/b.css", "beta"),
    ] {
      combined.update(rel.as_bytes());
      combined.update(b"\0");
      combined.update(Sha1::digest(content));
    }
    let expected: String = combined.finalize()[..6].iter().map(|byte| format!("{byte:02x}")).collect();
    let version = templating.static_asset_version().unwrap();
    assert_eq!(version.rsplit_once('-').unwrap().1, expected);
  }

  #[test]
  fn html_names_match_rglob_and_do_not_enter_symlinked_directories() {
    let root = tempfile::tempdir().unwrap();
    let pages = root.path().join("templates");
    fs::create_dir_all(pages.join("nested")).unwrap();
    fs::write(pages.join(".gitignore"), "*.html\n").unwrap();
    fs::create_dir(pages.join("dir.html")).unwrap();
    fs::write(pages.join(".hidden.html"), "").unwrap();
    fs::write(pages.join("index.html"), "").unwrap();
    fs::write(pages.join("ignored.html"), "").unwrap();
    fs::write(pages.join("nested").join("deep.html"), "").unwrap();
    fs::write(pages.join("notes.txt"), "").unwrap();
    std::os::unix::fs::symlink(pages.join("nested"), pages.join("linked")).unwrap();
    let mut names = Vec::new();
    collect_html_names(&pages, &mut names);
    names.sort();
    assert_eq!(names, [".hidden.html", "dir.html", "ignored.html", "index.html", "nested/deep.html"]);
  }
}

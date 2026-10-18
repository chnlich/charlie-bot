//! The request-time template engine (`src/runtime/templating.py` over Jinja2 as starlette configures it).
//!
//! Each render builds a fresh minijinja environment, so a template, a page part or a template directory
//! that changes on disk takes effect at the next render without a restart, as Jinja2's auto-reload does.
//! The environment reproduces the Jinja2 behaviors the pages observe: autoescaping with markupsafe's
//! escape table, Python's `str()` of printed values, the `tojson` filter as Jinja2's
//! `htmlsafe_json_dumps` with `sort_keys`, `urlencode` as `quote(safe="/")`, Python's `dict.get`, and
//! one stripped trailing newline per template. The search path is `web/templates`, then each page
//! package's `templates/`, in registration order.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use charliebot_core::{pyfmt, registry};
use minijinja::value::ValueKind;
use minijinja::{AutoEscape, Environment, Error, ErrorKind, Output, State, Value};
use sha1::{Digest, Sha1};

use crate::git;

/// `page_render.PAGE_SLOTS`: the places a page leaves for package parts.
pub const PAGE_SLOTS: &[&str] = &[
  "head",
  "tab_buttons",
  "below_header",
  "tab_panels",
  "input_buttons",
  "modals",
  "scripts",
  "diff_toolbar",
  "home_notes",
];

/// A zero-argument template global: the name and the value the call returns.
pub type TemplateGlobal = (&'static str, Value);

/// The template engine of one repository checkout.
pub struct Templating {
  repo_root: PathBuf,
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
  pub fn new(repo_root: &Path) -> Templating {
    let mut directories = vec![repo_root.join("web").join("templates")];
    directories.extend(
      registry::PACKAGES.iter().map(|package| registry::package_directory(repo_root, package).join("templates")),
    );
    Templating {
      repo_root: repo_root.to_path_buf(),
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
    for package in registry::PACKAGES {
      let directory = registry::package_directory(&self.repo_root, package).join("static");
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
    let mut environment = Environment::new();
    let directories = self.directories.clone();
    environment.set_loader(move |name| load_template(&directories, name));
    environment.set_auto_escape_callback(|_| AutoEscape::Html);
    environment.set_formatter(markupsafe_formatter);
    environment.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    environment.add_filter("tojson", tojson);
    environment.add_filter("urlencode", urlencode);
    let repo_root = self.repo_root.clone();
    environment.add_function("page_parts", move |slot: String| {
      page_parts(&repo_root, &slot)
        .map_err(|message| Error::new(ErrorKind::InvalidOperation, format!("ValueError: {message}")))
    });
    for (name, value) in globals {
      environment.add_function(name, move || value.clone());
    }
    environment
  }
}

/// `templating.page_parts(slot)`: the slot's part template names that exist on disk now.
pub fn page_parts(repo_root: &Path, slot: &str) -> Result<Vec<String>, String> {
  if !PAGE_SLOTS.contains(&slot) {
    let slots = PAGE_SLOTS.iter().map(|name| pyfmt::str_repr(name)).collect::<Vec<_>>().join(", ");
    return Err(format!("page slot {} is not one of ({slots})", pyfmt::str_repr(slot)));
  }
  Ok(
    registry::PACKAGES
      .iter()
      .filter_map(|package| {
        let name =
          format!("{}/page_{slot}.html", package.rsplit('.').next().expect("rsplit yields at least one piece"));
        registry::package_directory(repo_root, package).join("templates").join(&name).is_file().then_some(name)
      })
      .collect(),
  )
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

/// Jinja2 `FileSystemLoader.get_source`: the first search directory holding the name wins; a `..`
/// segment names no template. The text is read as UTF-8 with universal newlines.
fn load_template(directories: &[PathBuf], name: &str) -> Result<Option<String>, Error> {
  let mut pieces = Vec::new();
  for piece in name.split('/') {
    if piece == ".." {
      return Ok(None);
    }
    if !piece.is_empty() && piece != "." {
      pieces.push(piece);
    }
  }
  for directory in directories {
    let path = pieces.iter().fold(directory.clone(), |path, piece| path.join(piece));
    if path.is_file() {
      let bytes = std::fs::read(&path).map_err(|error| {
        Error::new(ErrorKind::InvalidOperation, format!("reading template {}: {error}", path.display()))
      })?;
      let text = String::from_utf8(bytes).map_err(|error| {
        Error::new(ErrorKind::InvalidOperation, format!("UnicodeDecodeError: template {}: {error}", path.display()))
      })?;
      return Ok(Some(text.replace("\r\n", "\n").replace('\r', "\n")));
    }
  }
  Ok(None)
}

/// Print a value as Jinja2 does under autoescaping: markup prints as is; anything else prints as
/// Python's `str()` of it, escaped with markupsafe's table.
fn markupsafe_formatter(out: &mut Output, state: &State, value: &Value) -> Result<(), Error> {
  if value.is_safe() || state.auto_escape() == AutoEscape::None {
    return out.write_str(&python_text(value)?).map_err(Error::from);
  }
  out.write_str(&markupsafe_escape(&python_text(value)?)).map_err(Error::from)
}

/// `markupsafe.escape`: `& < > ' "` as `&amp; &lt; &gt; &#39; &#34;`.
pub fn markupsafe_escape(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  for c in text.chars() {
    match c {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '\'' => out.push_str("&#39;"),
      '"' => out.push_str("&#34;"),
      c => out.push(c),
    }
  }
  out
}

/// Python's `str()` of a template value; an undefined value prints as nothing (Jinja2 `Undefined`).
fn python_text(value: &Value) -> Result<String, Error> {
  Ok(match value.kind() {
    ValueKind::Undefined => String::new(),
    ValueKind::String => value.as_str().expect("a string value has text").to_string(),
    _ => pyfmt::python_str(&to_json(value)?),
  })
}

fn to_json(value: &Value) -> Result<serde_json::Value, Error> {
  serde_json::to_value(value).map_err(|error| Error::new(ErrorKind::BadSerialization, error.to_string()))
}

/// Jinja2's `tojson` (`htmlsafe_json_dumps` with `sort_keys=True`): `json.dumps` text with `< > & '`
/// escaped as JSON unicode escapes, marked safe.
fn tojson(value: Value) -> Result<Value, Error> {
  let text = pyfmt::json_dumps(&to_json(&value)?, &pyfmt::JINJA_TOJSON)
    .replace('<', "\\u003c")
    .replace('>', "\\u003e")
    .replace('&', "\\u0026")
    .replace('\'', "\\u0027");
  Ok(Value::from_safe_string(text))
}

/// Jinja2's `urlencode` of a scalar: `urllib.parse.quote(str(value), safe="/")`.
fn urlencode(value: Value) -> Result<String, Error> {
  if matches!(value.kind(), ValueKind::Seq | ValueKind::Map | ValueKind::Iterable) {
    return Err(Error::new(ErrorKind::InvalidOperation, "urlencode of a mapping or sequence is not ported"));
  }
  let mut out = String::new();
  for byte in python_text(&value)?.bytes() {
    if byte.is_ascii_alphanumeric() || b"/_.-~".contains(&byte) {
      out.push(byte as char);
    } else {
      out.push_str(&format!("%{byte:02X}"));
    }
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn render_str(source: &str, context: serde_json::Value) -> String {
    let templating = Templating::new(Path::new("/nonexistent"));
    let environment = templating.environment(Vec::new());
    environment.render_str(source, Value::from_serialize(&context)).unwrap()
  }

  #[test]
  fn printing_follows_jinja2_under_autoescape() {
    let context = json!({"text": "<a href='x'>&\"/", "none": null, "yes": true, "number": 1.5, "items": ["a", 1]});
    assert_eq!(
      render_str("{{ text }}|{{ none }}|{{ yes }}|{{ number }}|{{ items }}|{{ missing }}", context),
      "&lt;a href=&#39;x&#39;&gt;&amp;&#34;/|None|True|1.5|[&#39;a&#39;, 1]|"
    );
  }

  #[test]
  fn tojson_matches_htmlsafe_json_dumps() {
    let context = json!({"value": {"b": "</script>&'", "a": [true, null, "\u{b7}"]}});
    assert_eq!(
      render_str("{{ value|tojson }}", context),
      concat!(r#"{"a": [true, null, ""#, "\\u00b7", r#""], "b": ""#, "\\u003c/script\\u003e\\u0026\\u0027", r#""}"#)
    );
  }

  #[test]
  fn urlencode_keeps_slashes() {
    assert_eq!(render_str("{{ v|urlencode }}", json!({"v": "a b/\u{b7}~"})), "a%20b/%C2%B7~");
  }
}

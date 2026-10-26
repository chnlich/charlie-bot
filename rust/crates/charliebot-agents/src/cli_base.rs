//! The module-level helpers of `src/runtime/agent_process/base.py` that the CLI backend types share: the binary lookup,
//! the child PATH and proxy edits, the Google key strip, and the Python value semantics of their translators.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_registry::backend::Env;
use serde_json::{Map, Value};

use crate::one_shot::frame_system_prompt;

/// Local addresses that bypass an injected proxy (`base.LOCAL_NO_PROXY_ENTRIES`).
pub const LOCAL_NO_PROXY_ENTRIES: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// The Google API-key variables a Google CLI must not inherit (`base.strip_google_api_keys`).
const GOOGLE_API_KEY_VARS: [&str; 2] = ["GEMINI_API_KEY", "GOOGLE_API_KEY"];

/// The image extensions a backend passes as image input (`base.IMAGE_MIME_BY_EXT`).
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// The MIME type of an upload filename's extension, or `None` when the backend does not pass it as an image. The
/// extension is the text after the last dot, lowercased, as the Python map's lookup is.
pub fn image_mime(extension: &str) -> Option<String> {
  let extension = extension.to_ascii_lowercase();
  if !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
    return None;
  }
  let mime = mime_guess::from_ext(&extension).first().expect("every accepted image extension has a MIME type");
  Some(mime.to_string())
}

/// The process environment the CLI backend types read at registration: PATH to find a CLI binary, and HOME for the
/// user-local bin directory the vendor installers target.
#[derive(Clone, Debug)]
pub struct CliHost {
  pub path: String,
  pub home: PathBuf,
}

impl CliHost {
  /// The host of the running server. HOME must be set, as `Path.home()` needs it.
  pub fn from_process() -> anyhow::Result<Self> {
    let home = std::env::var_os("HOME").context("HOME must be set for the CLI backend types")?;
    let path = match std::env::var("PATH") {
      Ok(path) => path,
      Err(std::env::VarError::NotPresent) => "/bin:/usr/bin".to_string(),
      Err(error) => return Err(error).context("PATH must be valid Unicode for the CLI backend types"),
    };
    Ok(Self { path, home: PathBuf::from(home) })
  }

  /// `Path.home() / ".local" / "bin"`: the directory the child PATH gains, and a binary's fallback directory.
  pub fn user_local_bin(&self) -> PathBuf {
    self.home.join(".local").join("bin")
  }

  /// `base.resolve_binary(name, USER_LOCAL_BIN)` against this host's PATH.
  pub fn resolve_binary(&self, name: &str) -> anyhow::Result<PathBuf> {
    resolve_binary(name, &self.path, &self.user_local_bin())
  }
}

/// The first executable `name` on `path`, else `fallback_dir/name` when that exists (`base.resolve_binary`).
pub fn resolve_binary(name: &str, path: &str, fallback_dir: &Path) -> anyhow::Result<PathBuf> {
  let cwd = std::env::current_dir().context("the current directory resolves for the binary search")?;
  match which::which_in(name, Some(path), cwd) {
    Ok(found) => return Ok(found),
    Err(which::Error::CannotFindBinaryPath) => {}
    Err(error) => return Err(error).with_context(|| format!("searching PATH for {name}")),
  }
  let fallback = fallback_dir.join(name);
  if fallback.exists() {
    return Ok(fallback);
  }
  bail!("{name} binary not found on PATH or at {}", fallback.display())
}

/// Prepends `dir` to the child PATH unless one of its entries is `dir` already (`base.prepend_path_dir`).
pub fn prepend_path_dir(env: &mut Env, dir: &str) {
  let current = env.get("PATH").cloned().unwrap_or_default();
  let mut entries: Vec<PathBuf> = std::env::split_paths(&current).collect();
  if entries.iter().any(|entry| entry == Path::new(dir)) {
    return;
  }
  entries.insert(0, PathBuf::from(dir));
  let joined = std::env::join_paths(entries).expect("a child PATH entry holds no path separator");
  env.insert("PATH".to_string(), joined.into_string().expect("a child PATH entry is valid Unicode"));
}

/// Merges `LOCAL_NO_PROXY_ENTRIES` into a NO_PROXY value, deduplicated and order-stable (`base.merge_local_no_proxy`).
/// Caller entries keep their positions and may repeat; each local entry appears once, appended in canonical order when
/// absent.
pub fn merge_local_no_proxy(value: &str) -> String {
  let mut entries: Vec<&str> = Vec::new();
  let mut seen_local: Vec<&str> = Vec::new();
  for raw_entry in value.split(',') {
    let entry = raw_entry.trim();
    if entry.is_empty() {
      continue;
    }
    if let Some(&local) = LOCAL_NO_PROXY_ENTRIES.iter().find(|local| **local == entry) {
      if seen_local.contains(&local) {
        continue;
      }
      seen_local.push(local);
      entries.push(local);
      continue;
    }
    entries.push(raw_entry);
  }
  entries.extend(LOCAL_NO_PROXY_ENTRIES.iter().copied().filter(|local| !seen_local.contains(local)));
  entries.join(",")
}

/// Points a child env's outbound traffic at `proxy_url`, in place (`base.apply_proxy_env`). HTTP_PROXY and HTTPS_PROXY
/// both carry the URL, and NO_PROXY gains the local exemptions.
pub fn apply_proxy_env(env: &mut Env, proxy_url: &str) {
  env.insert("HTTP_PROXY".to_string(), proxy_url.to_string());
  env.insert("HTTPS_PROXY".to_string(), proxy_url.to_string());
  let no_proxy = merge_local_no_proxy(env.get("NO_PROXY").map_or("", String::as_str));
  env.insert("NO_PROXY".to_string(), no_proxy);
}

/// The prompt with the instructions framed in, as `_effective_prompt` does for a backend with no system-prompt flag.
pub fn framed_prompt(instructions_content: Option<&str>, prompt: &str) -> String {
  match instructions_content {
    Some(content) => frame_system_prompt(content, prompt),
    None => prompt.to_string(),
  }
}

/// A copy of `env` without the Google API-key variables (`base.strip_google_api_keys`). A Google CLI that inherits one
/// bills through the key instead of its OAuth login.
pub fn strip_google_api_keys(env: &Env) -> Env {
  let mut stripped = env.clone();
  for name in GOOGLE_API_KEY_VARS {
    stripped.remove(name);
  }
  stripped
}

/// Python truthiness of a JSON value: null, false, zero, and empty strings, arrays and objects are false.
pub fn truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(flag) => *flag,
    Value::Number(number) => number.as_f64().is_none_or(|float| float != 0.0),
    Value::String(text) => !text.is_empty(),
    Value::Array(items) => !items.is_empty(),
    Value::Object(members) => !members.is_empty(),
  }
}

/// The string field `key` of a JSON object, or `default` when the object lacks it (`ev.get(key, default)`).
pub fn str_field<'a>(fields: &'a Map<String, Value>, key: &str, default: &'a str) -> anyhow::Result<&'a str> {
  match fields.get(key) {
    None => Ok(default),
    Some(Value::String(text)) => Ok(text),
    Some(other) => bail!("event field {key} must be a string, got {other}"),
  }
}

/// The string field `key` of a JSON object, which the object must carry (`ev[key]`).
pub fn required_str<'a>(fields: &'a Map<String, Value>, key: &str) -> anyhow::Result<&'a str> {
  match fields.get(key) {
    None => bail!("event lacks field {key}"),
    Some(Value::String(text)) => Ok(text),
    Some(other) => bail!("event field {key} must be a string, got {other}"),
  }
}

/// The integer field `key` of a JSON object, or 0 when the object lacks it (`stats.get(key, 0)`). A value that is not an
/// integer is an error, where Python would carry it into the event.
pub fn int_field(fields: &Map<String, Value>, key: &str) -> anyhow::Result<i64> {
  match fields.get(key) {
    None => Ok(0),
    Some(value) => value.as_i64().with_context(|| format!("event field {key} must be an integer, got {value}")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn merge_keeps_caller_entries_and_appends_each_local_entry_once() {
    assert_eq!(merge_local_no_proxy("internal.test,localhost,127.0.0.1"), "internal.test,localhost,127.0.0.1,::1");
    assert_eq!(merge_local_no_proxy(""), "localhost,127.0.0.1,::1");
    let once = merge_local_no_proxy("localhost, internal.test ,localhost");
    assert_eq!(once, "localhost, internal.test ,127.0.0.1,::1");
    assert_eq!(merge_local_no_proxy(&once), once);
  }

  #[test]
  fn apply_proxy_env_sets_both_proxies_and_leaves_the_other_keys() {
    let mut env = Env::from([("PATH".to_string(), "/usr/bin".to_string())]);
    apply_proxy_env(&mut env, "http://proxy.test:8080");
    assert_eq!(env["HTTP_PROXY"], "http://proxy.test:8080");
    assert_eq!(env["HTTPS_PROXY"], "http://proxy.test:8080");
    assert_eq!(env["NO_PROXY"], "localhost,127.0.0.1,::1");
    assert_eq!(env["PATH"], "/usr/bin");
  }

  #[test]
  fn strip_removes_only_the_google_api_keys() {
    let env = Env::from([
      ("GEMINI_API_KEY".to_string(), "gemini".to_string()),
      ("GOOGLE_API_KEY".to_string(), "google".to_string()),
      ("PATH".to_string(), "/usr/bin".to_string()),
    ]);
    let stripped = strip_google_api_keys(&env);
    assert_eq!(stripped, Env::from([("PATH".to_string(), "/usr/bin".to_string())]));
    assert_eq!(env.len(), 3);
  }

  #[test]
  fn image_extensions_map_to_the_client_facing_mime_strings() {
    assert_eq!(image_mime("png").as_deref(), Some("image/png"));
    assert_eq!(image_mime("JPG").as_deref(), Some("image/jpeg"));
    assert_eq!(image_mime("jpeg").as_deref(), Some("image/jpeg"));
    assert_eq!(image_mime("gif").as_deref(), Some("image/gif"));
    assert_eq!(image_mime("webp").as_deref(), Some("image/webp"));
    assert_eq!(image_mime("txt"), None);
    assert_eq!(image_mime(""), None);
  }

  #[test]
  fn prepend_adds_the_dir_once() {
    let mut env = Env::from([("PATH".to_string(), "/usr/bin".to_string())]);
    prepend_path_dir(&mut env, "/home/a/.local/bin");
    assert_eq!(env["PATH"], "/home/a/.local/bin:/usr/bin");
    prepend_path_dir(&mut env, "/home/a/.local/bin");
    assert_eq!(env["PATH"], "/home/a/.local/bin:/usr/bin");
  }
}

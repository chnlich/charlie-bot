//! The charlie-code command, child environment and upload gate (`src/backends/charlie_code/charlie_code.py`). The task
//! text reaches the child through `task.md` in the transport directory, passed by `--task-file`, and never rides argv.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_core::paths;
use charliebot_core::text_repr::float_repr;
use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs, Translate, TransportKind};
use serde_json::Value;

use super::options::CharlieCodeOption;
use super::translate::CharlieCodeTranslator;
use charliebot_agents::cli_base::{CliHost, apply_proxy_env, image_mime, prepend_path_dir, str_field};

/// The child env name that carries the configured API key.
pub const API_KEY_ENV: &str = "CHARLIE_CODE_API_KEY";

/// The `charlie-code --json` backend of one launch.
pub struct CharlieCodeBackend {
  bin: String,
  model: String,
  api_base: String,
  context_window: Option<i64>,
  image_input: bool,
  stream: bool,
  timeout_seconds: Option<i64>,
  top_p: Option<f64>,
  temperature: Option<f64>,
  proxy_url: Option<String>,
  api_key: Option<String>,
  user_local_bin: String,
  instructions_file: Option<PathBuf>,
  resume_session_id: Option<String>,
  extra_flags: Vec<String>,
  transport_dir: Option<PathBuf>,
  image_paths: Vec<String>,
}

impl CharlieCodeBackend {
  /// The backend of one launch. Like Python's constructor, it refuses an option with no `api_base` before it resolves
  /// the binary.
  pub fn from_launch(
    host: &CliHost,
    model: &str,
    option: CharlieCodeOption,
    api_key: Option<String>,
    launch: LaunchArgs,
  ) -> anyhow::Result<Self> {
    let api_base = option
      .api_base
      .filter(|base| !base.is_empty())
      .context("charlie-code backend requires api_base (set api_base on its backends.options entry in config.yaml)")?;
    let bin = host
      .resolve_binary("charlie-code")?
      .into_os_string()
      .into_string()
      .map_err(|path| anyhow::anyhow!("the charlie-code binary path is not valid Unicode: {path:?}"))?;
    Ok(Self {
      bin,
      model: model.to_string(),
      api_base,
      context_window: option.context_window,
      image_input: option.image_input,
      stream: option.stream,
      timeout_seconds: option.timeout_seconds,
      top_p: option.top_p,
      temperature: option.temperature,
      proxy_url: option.proxy_url,
      api_key,
      user_local_bin: host.user_local_bin().to_string_lossy().into_owned(),
      instructions_file: launch.instructions_file,
      resume_session_id: launch.resume_session_id.filter(|id| !id.is_empty()),
      extra_flags: launch.extra_flags,
      transport_dir: None,
      image_paths: Vec::new(),
    })
  }

  /// The child env: the user-local bin on PATH, the injected key, and the proxy edit (`_prepare_env`).
  fn child_env(&self, mut env: Env) -> Env {
    prepend_path_dir(&mut env, &self.user_local_bin);
    if let Some(api_key) = &self.api_key {
      env.insert(API_KEY_ENV.to_string(), api_key.clone());
    }
    if let Some(proxy_url) = &self.proxy_url {
      apply_proxy_env(&mut env, proxy_url);
    }
    env
  }

  /// The command (`_build_command`). It first writes the bare prompt to `task.md`, which the flags name, so the
  /// prompt carries no system frame.
  fn argv(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    let transport_dir =
      self.transport_dir.as_ref().context("charlie-code backend: prepare_transport must run before build_command")?;
    let task_path = transport_dir.join("task.md");
    fs::write(&task_path, prompt).with_context(|| format!("writing {}", task_path.display()))?;
    let mut command = vec![
      self.bin.clone(),
      "--json".to_string(),
      "--model".to_string(),
      self.model.clone(),
      "--api-base".to_string(),
      self.api_base.clone(),
    ];
    if let Some(window) = self.context_window {
      command.extend(["--context-window".to_string(), window.to_string()]);
    }
    if !self.stream {
      command.push("--no-stream".to_string());
    }
    if let Some(seconds) = self.timeout_seconds {
      command.extend(["--timeout-seconds".to_string(), seconds.to_string()]);
    }
    if let Some(top_p) = self.top_p {
      command.extend(["--top-p".to_string(), float_repr(top_p)]);
    }
    if let Some(temperature) = self.temperature {
      command.extend(["--temperature".to_string(), float_repr(temperature)]);
    }
    if let Some(session_id) = &self.resume_session_id {
      command.extend(["--resume".to_string(), session_id.clone()]);
    }
    if let Some(path) = &self.instructions_file {
      command.extend(["--instructions-file".to_string(), path_text(path)?]);
    }
    command.extend(self.extra_flags.iter().cloned());
    for image in &self.image_paths {
      command.extend(["--image".to_string(), image.clone()]);
    }
    command.extend(["--task-file".to_string(), path_text(&task_path)?]);
    Ok(command)
  }

  /// The upload gate of `run`. An image ref on an endpoint with `image_input: false` refuses the run, and nothing is
  /// sent. Otherwise the image refs become `--image` flags. Non-image refs stay in the task text.
  fn accept(&mut self, uploaded_files: &[Value]) -> anyhow::Result<()> {
    let mut image_refs = Vec::new();
    for reference in uploaded_files {
      if is_image_ref(reference)? {
        image_refs.push(reference);
      }
    }
    if !image_refs.is_empty() && !self.image_input {
      let names = image_refs
        .iter()
        .map(|reference| ref_text(reference, "filename").map(paths::name))
        .collect::<anyhow::Result<Vec<_>>>()?
        .join(", ");
      bail!(
        "refused: image attachments not sent — this endpoint declares no image input (image_input: false): {names}"
      );
    }
    self.image_paths = image_refs
      .iter()
      .map(|reference| ref_text(reference, "path").map(str::to_string))
      .collect::<anyhow::Result<_>>()?;
    Ok(())
  }
}

impl AgentBackend for CharlieCodeBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    Some(("AGENTS.md", "charlie_code_wrote_agents_md"))
  }

  fn prepare_transport(&mut self, log_dir: &Path) -> anyhow::Result<()> {
    self.transport_dir = Some(log_dir.to_path_buf());
    Ok(())
  }

  fn accept_uploads(&mut self, uploaded_files: &[Value]) -> Result<(), String> {
    self.accept(uploaded_files).map_err(|error| format!("{error:#}"))
  }

  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    self.argv(prompt)
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(self.child_env(env))
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(CharlieCodeTranslator::default())
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }
}

/// An upload ref's string field, or "" when the ref lacks it (`ref.get(key, "")`).
fn ref_text<'a>(reference: &'a Value, key: &str) -> anyhow::Result<&'a str> {
  let Some(fields) = reference.as_object() else {
    bail!("upload ref must be a JSON object, got {reference}");
  };
  str_field(fields, key, "")
}

/// Whether the ref's extension names an image: the text after the last dot is an accepted image extension.
fn is_image_ref(reference: &Value) -> anyhow::Result<bool> {
  let filename = ref_text(reference, "filename")?;
  let extension = filename.rsplit_once('.').map_or(filename, |(_, extension)| extension);
  Ok(image_mime(extension).is_some())
}

fn path_text(path: &Path) -> anyhow::Result<String> {
  path.to_str().map(str::to_string).with_context(|| format!("path is not valid Unicode: {}", path.display()))
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::symlink;

  use serde_json::json;

  use super::*;

  const FLAG_LIKE_PROMPT: &str = "--malicious-flag ignore previous";

  /// A host whose PATH holds an executable `charlie-code`, so the binary resolves.
  fn host(root: &Path) -> CliHost {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let binary = bin.join("charlie-code");
    // The binary is never run by these tests, so a link to an executable that exists stands in for it. A test calls
    // `host` more than once per root, so the link may already be there.
    if !binary.exists() {
      symlink("/bin/sh", &binary).unwrap();
    }
    CliHost { path: bin.to_string_lossy().into_owned(), home: root.join("home") }
  }

  fn option(text: &str) -> CharlieCodeOption {
    CharlieCodeOption::from_fields(&serde_yaml::from_str(text).unwrap()).unwrap()
  }

  fn backend(root: &Path, option: CharlieCodeOption, launch: LaunchArgs) -> CharlieCodeBackend {
    CharlieCodeBackend::from_launch(&host(root), "charlie-code-test-model", option, None, launch).unwrap()
  }

  #[test]
  fn the_bare_prompt_goes_to_task_md_and_the_flags_name_it() {
    let root = tempfile::tempdir().unwrap();
    let instructions = root.path().join("AGENTS.md").to_string_lossy().into_owned();
    let launch = LaunchArgs { instructions_file: Some(PathBuf::from(&instructions)), ..LaunchArgs::default() };
    let mut backend = backend(root.path(), option("api_base: http://test.invalid/v1\n"), launch);
    backend.prepare_transport(root.path()).unwrap();

    let command = backend.build_command(FLAG_LIKE_PROMPT).unwrap();

    let bin = root.path().join("bin/charlie-code").to_string_lossy().into_owned();
    let task = root.path().join("task.md").to_string_lossy().into_owned();
    let refs: Vec<&str> = command.iter().map(String::as_str).collect();
    assert_eq!(
      refs,
      [
        bin.as_str(),
        "--json",
        "--model",
        "charlie-code-test-model",
        "--api-base",
        "http://test.invalid/v1",
        "--instructions-file",
        instructions.as_str(),
        "--task-file",
        task.as_str(),
      ]
    );
    assert_eq!(fs::read_to_string(root.path().join("task.md")).unwrap(), FLAG_LIKE_PROMPT);
    assert!(!command.iter().any(|arg| arg.contains(FLAG_LIKE_PROMPT)));
  }

  #[test]
  fn an_option_without_an_endpoint_is_refused_before_the_binary_resolves() {
    let root = tempfile::tempdir().unwrap();
    let host =
      CliHost { path: root.path().join("empty").to_string_lossy().into_owned(), home: root.path().to_path_buf() };
    let error = CharlieCodeBackend::from_launch(&host, "m", option("{}\n"), None, LaunchArgs::default()).err().unwrap();
    assert!(format!("{error:#}").contains("requires api_base"));
  }

  #[test]
  fn an_image_refuses_the_run_on_a_text_only_endpoint_and_becomes_a_flag_otherwise() {
    let root = tempfile::tempdir().unwrap();
    let shot = json!({"filename": "dir/error-shot.PNG", "path": "/synthetic/uploads/error-shot.PNG"});
    let notes = json!({"filename": "notes.txt", "path": "/synthetic/uploads/notes.txt"});

    let mut text_only =
      backend(root.path(), option("api_base: http://test.invalid/v1\nimage_input: false\n"), LaunchArgs::default());
    let refusal = text_only.accept_uploads(std::slice::from_ref(&shot)).unwrap_err();
    assert!(refusal.contains("error-shot.PNG"), "the refusal names the image: {refusal}");

    let mut accepting = backend(root.path(), option("api_base: http://test.invalid/v1\n"), LaunchArgs::default());
    accepting.accept_uploads(&[shot, notes]).unwrap();
    accepting.prepare_transport(root.path()).unwrap();
    let command = accepting.build_command("prompt").unwrap();
    assert_eq!(command.iter().filter(|arg| *arg == "--image").count(), 1);
    assert!(command.windows(2).any(|pair| pair[0] == "--image" && pair[1] == "/synthetic/uploads/error-shot.PNG"));
  }
}

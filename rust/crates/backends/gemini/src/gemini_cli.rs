//! The gemini command and child environment.

use std::path::Path;

use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs, Translate, TransportKind};

use super::translate::GeminiTranslator;
use charliebot_agents::cli_base::{framed_prompt, strip_google_api_keys};

/// Gemini appends this short query to the prompt text it reads from stdin.
const STDIN_PROMPT_QUERY: &str = "Follow the task prompt supplied on stdin.";

/// The `gemini` stream-json backend of one launch.
pub struct GeminiBackend {
  gemini_bin: String,
  model: String,
  instructions_content: Option<String>,
  resume_session_id: Option<String>,
  extra_flags: Vec<String>,
}

impl GeminiBackend {
  /// The backend of one launch. `gemini_bin` is the resolved binary.
  pub fn from_launch(gemini_bin: String, model: &str, launch: LaunchArgs) -> Self {
    Self {
      gemini_bin,
      model: model.to_string(),
      instructions_content: launch.instructions_content.filter(|content| !content.is_empty()),
      resume_session_id: launch.resume_session_id.filter(|id| !id.is_empty()),
      extra_flags: launch.extra_flags,
    }
  }

  /// The stream-json command. Gemini reads the framed prompt from stdin as context, and `-p` supplies only a short
  /// instruction to follow that context.
  fn argv(&self) -> Vec<String> {
    let mut command = vec![
      self.gemini_bin.clone(),
      "-m".to_string(),
      self.model.clone(),
      "-p".to_string(),
      STDIN_PROMPT_QUERY.to_string(),
      "-o".to_string(),
      "stream-json".to_string(),
      "-y".to_string(),
      "--sandbox=false".to_string(),
    ];
    if let Some(session_id) = &self.resume_session_id {
      command.extend(["--resume".to_string(), session_id.clone()]);
    }
    command.extend(self.extra_flags.iter().cloned());
    command
  }
}

impl AgentBackend for GeminiBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    None
  }

  fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
    Ok(self.argv())
  }

  /// The environment with the Google API keys stripped, so the CLI bills through its OAuth login.
  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(strip_google_api_keys(&env))
  }

  fn stdin_prompt(&self, prompt: &str) -> Option<String> {
    Some(framed_prompt(self.instructions_content.as_deref(), prompt))
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(GeminiTranslator::new())
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn backend(launch: LaunchArgs) -> GeminiBackend {
    GeminiBackend::from_launch("/usr/bin/gemini".to_string(), "gemini-test-model", launch)
  }

  #[test]
  fn argv_splices_the_resume_and_extra_flags_and_has_a_small_stdin_query() {
    let backend = backend(LaunchArgs {
      extra_flags: vec!["--approval-mode".to_string(), "yolo".to_string()],
      buffer_limit: None,
      instructions_content: Some("Use concise answers.".to_string()),
      instructions_file: None,
      resume_session_id: Some("session-123".to_string()),
      log_dir: None,
      cgroup_session_id: None,
      remote_launch: None,
      backend_kwargs: serde_json::Map::new(),
      manager_turn: false,
    });
    let expected: Vec<&str> = vec![
      "/usr/bin/gemini",
      "-m",
      "gemini-test-model",
      "-p",
      STDIN_PROMPT_QUERY,
      "-o",
      "stream-json",
      "-y",
      "--sandbox=false",
      "--resume",
      "session-123",
      "--approval-mode",
      "yolo",
    ];
    assert_eq!(backend.argv(), expected);
    assert_eq!(
      AgentBackend::stdin_prompt(&backend, "Hello").as_deref(),
      Some("<system-instructions>\nUse concise answers.\n</system-instructions>\n\nHello")
    );
  }

  #[test]
  fn a_200_kib_prompt_uses_stdin_and_no_argv_element_carries_it() {
    const MAX_ARGUMENT_BYTES: usize = 128 * 1024;
    let prompt = "\u{fc}".repeat(100 * 1024);
    let backend = backend(LaunchArgs {
      extra_flags: Vec::new(),
      buffer_limit: None,
      instructions_content: None,
      instructions_file: None,
      resume_session_id: None,
      log_dir: None,
      cgroup_session_id: None,
      remote_launch: None,
      backend_kwargs: serde_json::Map::new(),
      manager_turn: false,
    });
    let argv = AgentBackend::build_command(&backend, &prompt).unwrap();
    assert_eq!(argv[3], "-p");
    assert_eq!(argv[4], STDIN_PROMPT_QUERY);
    assert!(argv.iter().all(|argument| argument.len() <= MAX_ARGUMENT_BYTES), "{argv:?}");
    assert!(argv.iter().all(|argument| !argument.contains(&prompt)), "{argv:?}");
    assert_eq!(AgentBackend::stdin_prompt(&backend, &prompt).as_deref(), Some(prompt.as_str()));
  }

  /// A 200 KiB prompt reaches the launcher stub unchanged on stdin, and no argv element exceeds 128 KiB.
  #[ignore = "integration"]
  #[test]
  fn a_200_kib_prompt_reaches_the_child_on_stdin_unchanged() {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    const MAX_ARGUMENT_BYTES: usize = 128 * 1024;
    let root = tempfile::tempdir().unwrap();
    let argv_file = root.path().join("argv");
    let prompt_file = root.path().join("prompt");
    let stub = root.path().join("gemini");
    fs::write(&stub, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ARGV_CAPTURE\"\ncat > \"$PROMPT_CAPTURE\"\n").unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let backend = GeminiBackend::from_launch(stub.display().to_string(), "gemini-test-model", LaunchArgs::default());
    let prompt = "\u{fc}".repeat(100 * 1024);
    let argv = AgentBackend::build_command(&backend, &prompt).unwrap();
    let stdin_prompt = AgentBackend::stdin_prompt(&backend, &prompt).unwrap();
    let mut child = Command::new(&argv[0])
      .args(&argv[1..])
      .env("ARGV_CAPTURE", &argv_file)
      .env("PROMPT_CAPTURE", &prompt_file)
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .unwrap();
    child.stdin.take().unwrap().write_all(stdin_prompt.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());

    let arguments: Vec<String> = fs::read_to_string(argv_file).unwrap().lines().map(str::to_string).collect();
    assert!(arguments.iter().all(|argument| argument.len() <= MAX_ARGUMENT_BYTES), "{arguments:?}");
    assert!(arguments.iter().all(|argument| !argument.contains(&prompt)), "{arguments:?}");
    assert_eq!(fs::read_to_string(prompt_file).unwrap(), prompt);
  }

  #[test]
  fn prepare_env_strips_both_google_keys_and_keeps_the_rest() {
    let env = Env::from([
      ("PATH".to_string(), "/usr/bin".to_string()),
      ("GEMINI_API_KEY".to_string(), "gemini-key".to_string()),
      ("GOOGLE_API_KEY".to_string(), "google-key".to_string()),
    ]);
    let prepared = backend(LaunchArgs {
      extra_flags: Vec::new(),
      buffer_limit: None,
      instructions_content: None,
      instructions_file: None,
      resume_session_id: None,
      log_dir: None,
      cgroup_session_id: None,
      remote_launch: None,
      backend_kwargs: serde_json::Map::new(),
      manager_turn: false,
    })
    .prepare_env(env)
    .unwrap();
    assert_eq!(prepared, Env::from([("PATH".to_string(), "/usr/bin".to_string())]));
  }
}

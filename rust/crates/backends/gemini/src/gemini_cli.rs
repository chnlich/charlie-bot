//! The gemini command and child environment (`src/backends/gemini/gemini_cli.py`).

use std::path::Path;

use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs, Translate, TransportKind};

use super::translate::GeminiTranslator;
use charliebot_agents::cli_base::{framed_prompt, strip_google_api_keys};

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

  /// The stream-json command. The prompt follows `-p` as one argv element, so a prompt that begins with a dash stays
  /// a prompt. Gemini has no system-prompt flag, so the instructions are framed into the prompt.
  fn argv(&self, prompt: &str) -> Vec<String> {
    let mut command = vec![
      self.gemini_bin.clone(),
      "-m".to_string(),
      self.model.clone(),
      "-p".to_string(),
      framed_prompt(self.instructions_content.as_deref(), prompt),
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

  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    Ok(self.argv(prompt))
  }

  /// Strips the Google API keys, so the CLI bills through its OAuth login.
  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(strip_google_api_keys(&env))
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(GeminiTranslator::default())
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
  fn argv_frames_the_instructions_and_splices_the_resume_and_extra_flags() {
    let backend = backend(LaunchArgs {
      instructions_content: Some("Use concise answers.".to_string()),
      resume_session_id: Some("session-123".to_string()),
      extra_flags: vec!["--approval-mode".to_string(), "yolo".to_string()],
      ..LaunchArgs::default()
    });
    let expected: Vec<&str> = vec![
      "/usr/bin/gemini",
      "-m",
      "gemini-test-model",
      "-p",
      "<system-instructions>\nUse concise answers.\n</system-instructions>\n\nHello",
      "-o",
      "stream-json",
      "-y",
      "--sandbox=false",
      "--resume",
      "session-123",
      "--approval-mode",
      "yolo",
    ];
    assert_eq!(backend.argv("Hello"), expected);
  }

  #[test]
  fn a_dash_prefixed_prompt_stays_the_value_of_p() {
    assert_eq!(
      backend(LaunchArgs::default()).argv("--watch-pid only local"),
      [
        "/usr/bin/gemini",
        "-m",
        "gemini-test-model",
        "-p",
        "--watch-pid only local",
        "-o",
        "stream-json",
        "-y",
        "--sandbox=false",
      ]
    );
  }

  #[test]
  fn prepare_env_strips_both_google_keys_and_keeps_the_rest() {
    let env = Env::from([
      ("PATH".to_string(), "/usr/bin".to_string()),
      ("GEMINI_API_KEY".to_string(), "gemini-key".to_string()),
      ("GOOGLE_API_KEY".to_string(), "google-key".to_string()),
    ]);
    let prepared = backend(LaunchArgs::default()).prepare_env(env).unwrap();
    assert_eq!(prepared, Env::from([("PATH".to_string(), "/usr/bin".to_string())]));
  }
}

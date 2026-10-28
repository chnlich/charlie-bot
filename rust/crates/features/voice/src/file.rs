//! The file transcription behind `charliebot voice transcribe`: one audio file through the online backend.

use std::path::Path;

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::credentials::Credentials;

use crate::config::VoiceSettings;
use crate::{decode, recording};

/// The transcript of one audio file, through `voice.default_backend` with the voice vocabulary and languages.
pub async fn transcribe_file(path: &Path, cfg: &CharlieBotConfig, credentials: &Credentials) -> Result<String, String> {
  let pcm = decode::load_pcm(path)?;
  let voice = cfg.section::<VoiceSettings>("voice");
  let transcript = recording::transcribe_recording(
    &recording::OnlineBackendFactory,
    &pcm,
    None,
    &voice.default_backend,
    credentials,
    &voice.vocabulary,
    &voice.languages,
  )
  .await
  .map_err(|error| match error {
    recording::RecordingError::BackendSelection(message) | recording::RecordingError::TranscriptionFailed(message) => {
      message
    }
  })?;
  Ok(transcript.text)
}

use charliebot_core::credentials::Credentials;
use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, watch};

use crate::audio::AudioQueue;
use crate::config::VoiceSettings;
use crate::transcription::{Backend, BackendEntry, TranscriptKind, TranscriptionError};

pub(crate) const VOICE_CHUNK_SAMPLES: usize = 2048;

pub(crate) struct RecordingTranscript {
  pub(crate) backend_id: String,
  pub(crate) text: String,
}

pub(crate) enum RecordingError {
  BackendSelection(String),
  TranscriptionFailed(String),
}

pub(crate) trait RecordingBackend: Send + Sync {
  fn id(&self) -> &str;
  fn unavailable_reason(&self) -> Option<String>;
  fn transcribe<'a>(
    &'a self,
    chunks: Vec<Vec<u8>>,
    vocabulary: &'a [String],
    languages: &'a [String],
  ) -> BoxFuture<'a, Result<String, String>>;
}

pub(crate) trait RecordingBackendFactory: Send + Sync {
  fn build(&self, entry: &BackendEntry, credentials: &Credentials) -> Box<dyn RecordingBackend>;
}

pub(crate) struct OnlineBackendFactory;

impl RecordingBackendFactory for OnlineBackendFactory {
  fn build(&self, entry: &BackendEntry, credentials: &Credentials) -> Box<dyn RecordingBackend> {
    Box::new(OnlineBackend(Backend::build(entry, credentials)))
  }
}

struct OnlineBackend(Backend);

impl RecordingBackend for OnlineBackend {
  fn id(&self) -> &str {
    self.0.id()
  }

  fn unavailable_reason(&self) -> Option<String> {
    self.0.unavailable_reason()
  }

  fn transcribe<'a>(
    &'a self,
    chunks: Vec<Vec<u8>>,
    vocabulary: &'a [String],
    languages: &'a [String],
  ) -> BoxFuture<'a, Result<String, String>> {
    Box::pin(async move {
      let queued_bytes = chunks.iter().map(Vec::len).sum();
      let audio = AudioQueue::new_recording(queued_bytes);
      for chunk in chunks {
        assert!(audio.put(chunk).await, "the upload recording queue fits every validated chunk");
      }
      audio.end(None).await;

      let (events, mut received) = mpsc::unbounded_channel();
      let (cancel, cancelled) = watch::channel(false);
      let mut worker = Box::pin(self.0.transcribe(audio, cancelled, events, vocabulary.to_vec(), languages.to_vec()));
      let _cancel = cancel;
      let mut final_text = None;
      let mut events_closed = false;
      let worker_result = loop {
        tokio::select! {
          event = received.recv(), if !events_closed => match event {
            Some(event) => {
              if event.kind == TranscriptKind::Final {
                final_text = Some(event.text);
              }
              event.acknowledged.send(()).expect("the backend waits for each recording event");
            }
            None => events_closed = true,
          },
          result = &mut worker => break result,
        }
      };
      worker_result.map_err(|error: TranscriptionError| error.to_string())?;
      final_text.ok_or_else(|| format!("{} produced no final event", self.0.id()))
    })
  }
}

pub(crate) async fn transcribe_recording(
  factory: &dyn RecordingBackendFactory,
  pcm_bytes: &[u8],
  backend_id: Option<&str>,
  settings: &VoiceSettings,
  credentials: &Credentials,
  vocabulary: &[String],
  languages: &[String],
) -> Result<RecordingTranscript, RecordingError> {
  let backend = select_backend(factory, backend_id, settings, credentials)?;
  let transcript = backend
    .transcribe(recording_chunks(pcm_bytes), vocabulary, languages)
    .await
    .map_err(|error| RecordingError::TranscriptionFailed(format!("voice transcription failed: {error}")))?;
  Ok(RecordingTranscript { backend_id: backend.id().to_string(), text: transcript })
}

fn select_backend(
  factory: &dyn RecordingBackendFactory,
  backend_id: Option<&str>,
  settings: &VoiceSettings,
  credentials: &Credentials,
) -> Result<Box<dyn RecordingBackend>, RecordingError> {
  let backend_id = match backend_id {
    Some(id) => id,
    None => settings.default_backend_id().map_err(RecordingError::BackendSelection)?,
  };
  let entry = settings.backend(backend_id).map_err(RecordingError::BackendSelection)?;
  let backend = factory.build(entry, credentials);
  if let Some(reason) = backend.unavailable_reason() {
    return Err(RecordingError::BackendSelection(format!("{backend_id} is unavailable: {reason}")));
  }
  Ok(backend)
}

fn recording_chunks(pcm_bytes: &[u8]) -> Vec<Vec<u8>> {
  let chunk_bytes = VOICE_CHUNK_SAMPLES * 2;
  pcm_bytes.chunks(chunk_bytes).map(<[u8]>::to_vec).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn recording_chunks_match_worklet_frames_and_keep_the_tail() {
    let pcm = vec![0_u8; (VOICE_CHUNK_SAMPLES + 10) * 2];

    let chunks = recording_chunks(&pcm);

    assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), [VOICE_CHUNK_SAMPLES * 2, 20]);
    assert_eq!(chunks.concat(), pcm);
  }
}

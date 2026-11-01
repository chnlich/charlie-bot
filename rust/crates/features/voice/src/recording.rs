use charliebot_core::credentials::Credentials;
use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, watch};

use crate::audio::AudioQueue;
use crate::transcription::{self, Backend, BackendSpec, TranscriptKind, TranscriptionError};

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
  fn backends(&self) -> &'static [BackendSpec];
  fn build(&self, backend_id: &str, credentials: &Credentials) -> Result<Box<dyn RecordingBackend>, String>;
}

pub(crate) struct OnlineBackendFactory;

impl RecordingBackendFactory for OnlineBackendFactory {
  fn backends(&self) -> &'static [BackendSpec] {
    transcription::BACKENDS
  }

  fn build(&self, backend_id: &str, credentials: &Credentials) -> Result<Box<dyn RecordingBackend>, String> {
    Ok(Box::new(OnlineBackend(Backend::build(backend_id, credentials)?)))
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
  default_backend: &str,
  credentials: &Credentials,
  vocabulary: &[String],
  languages: &[String],
) -> Result<RecordingTranscript, RecordingError> {
  let backend = select_backend(factory, backend_id, default_backend, credentials)?;
  let transcript = backend
    .transcribe(recording_chunks(pcm_bytes), vocabulary, languages)
    .await
    .map_err(|error| RecordingError::TranscriptionFailed(format!("voice transcription failed: {error}")))?;
  Ok(RecordingTranscript { backend_id: backend.id().to_string(), text: transcript })
}

fn select_backend(
  factory: &dyn RecordingBackendFactory,
  backend_id: Option<&str>,
  default_backend: &str,
  credentials: &Credentials,
) -> Result<Box<dyn RecordingBackend>, RecordingError> {
  let backend_id = backend_id.unwrap_or(default_backend);
  if !factory.backends().iter().any(|spec| spec.id == backend_id) {
    return Err(RecordingError::BackendSelection(transcription::unknown_backend(backend_id)));
  }
  let backend = factory.build(backend_id, credentials).map_err(RecordingError::BackendSelection)?;
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

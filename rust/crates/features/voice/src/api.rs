//! The voice upload route: `POST /api/voice/{session_id}`.
//!
//! The upload persists the recording before it selects an online backend. Selection failures answer 400, and
//! transcription failures answer 500; both leave the WAV on disk. The preview relay lives in relay.rs and shares the
//! `voice_transcribed` line written here.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequestParts, Path as AxumPath, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use charliebot_compat::form_body::{self, Field, FormModel};
use charliebot_compat::routing::post_only;
use charliebot_compat::{json_loads, responses};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::credentials::Credentials;
use charliebot_core::text_repr;
use charliebot_registry::FeatureContext;
use serde_json::{Value, json};

use crate::archive;
use crate::audio::{MAX_RECORDING_SAMPLES, SAMPLE_RATE};
use crate::config::VoiceSettings;
use crate::recording::{self, RecordingBackendFactory};
use crate::wav;

/// The recording's device facts, which the browser sends as the upload form's `devices` field and the relay's end
/// frame, and which `voice_transcribed` logs under these names.
pub(crate) const DEVICE_FIELDS: &[&str] =
  &["input_device", "capture_settings", "output_device", "communications_output_device", "device_error"];

const UPLOAD_FIELDS: &[Field] =
  &[Field::file("audio"), Field::optional_form("backend"), Field::optional_form("devices")];
const UPLOAD_FORM: FormModel = FormModel::new(UPLOAD_FIELDS);

/// The routes of `/api/voice`, with the feature context as their state.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/api/voice/{session_id}", post_only(upload_voice_recording)).with_state(Arc::clone(features))
}

/// Whether a request path falls under the voice upload routes, for the trailing-slash redirect the server answers.
pub(crate) fn matches(path: &str) -> bool {
  let Some(rest) = path.strip_prefix("/api/voice/") else {
    return false;
  };
  !rest.is_empty() && !rest.contains('/')
}

/// A voice request's failure: the HTTP status and the `error` text the response carries.
#[derive(Debug)]
pub(crate) struct VoiceRequestError {
  status: StatusCode,
  message: String,
}

impl VoiceRequestError {
  fn new(status: StatusCode, message: String) -> Self {
    Self { status, message }
  }

  fn bad_request(message: String) -> Self {
    Self::new(StatusCode::BAD_REQUEST, message)
  }
}

/// The form fields of one upload.
pub(crate) struct UploadForm {
  pub(crate) audio: Vec<u8>,
  pub(crate) backend: Option<String>,
  pub(crate) devices: Option<String>,
}

/// `POST /api/voice/{session_id}`: multipart fields carry the WAV, selected backend and device object.
async fn upload_voice_recording(State(features): State<Arc<FeatureContext>>, request: Request) -> Response {
  let (mut parts, stream) = request.into_parts();
  let form = match form_body::read(&parts.headers, stream).await {
    Ok(form) => form,
    Err(response) => return *response,
  };
  let AxumPath(session_id) = match AxumPath::<String>::from_request_parts(&mut parts, &features).await {
    Ok(path) => path,
    Err(response) => return response.into_response(),
  };
  if let Err(response) = form.validate(&UPLOAD_FORM) {
    return *response;
  }
  let audio = form.file("audio").expect("the required audio field validated");
  let audio = match tokio::fs::read(audio.path()).await {
    Ok(audio) => audio,
    Err(error) => return responses::internal_error("OSError", error.to_string()),
  };
  let upload = UploadForm {
    audio,
    backend: form.form("backend").map(str::to_string),
    devices: form.form("devices").map(str::to_string),
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let credentials = match features.credentials.get() {
    Ok(credentials) => credentials,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let factory = recording::OnlineBackendFactory;
  upload_recording(cfg, credentials, session_id, upload, &factory).await
}

/// Persist one validated recording before backend selection, then archive its final transcript.
async fn upload_recording(
  cfg: Arc<CharlieBotConfig>,
  credentials: Arc<Credentials>,
  session_id: String,
  upload: UploadForm,
  factory: &dyn RecordingBackendFactory,
) -> Response {
  let devices = match devices_form_field(upload.devices.as_deref()) {
    Ok(devices) => devices,
    Err(error) => return error_response(error),
  };
  let pcm = match wav_body_to_pcm(&upload.audio, MAX_RECORDING_SAMPLES) {
    Ok(pcm) => pcm,
    Err(error) => return error_response(error),
  };
  let persist_cfg = Arc::clone(&cfg);
  let persist_session_id = session_id.clone();
  let persist_pcm = pcm.clone();
  let persisted =
    tokio::task::spawn_blocking(move || archive::persist_recording(&persist_cfg, &persist_session_id, &persist_pcm))
      .await
      .expect("recording persistence does not panic");
  let audio_path = match persisted {
    Ok(path) => path,
    Err(error) => return responses::internal_error("OSError", error.to_string()),
  };
  let voice = cfg.section::<VoiceSettings>("voice");
  let transcript = match recording::transcribe_recording(
    factory,
    &pcm,
    upload.backend.as_deref(),
    &voice.default_backend,
    &credentials,
    &voice.vocabulary,
    &voice.languages,
  )
  .await
  {
    Ok(transcript) => transcript,
    Err(recording::RecordingError::BackendSelection(message)) => {
      return error_response(VoiceRequestError::bad_request(message));
    }
    Err(recording::RecordingError::TranscriptionFailed(message)) => {
      tracing::error!(session_id, selected_backend = ?upload.backend, error = %message, "voice_transcribe_failed");
      return error_response(VoiceRequestError::new(StatusCode::INTERNAL_SERVER_ERROR, message));
    }
  };
  let transcript_path = audio_path.clone();
  let transcript_text = transcript.text.clone();
  let written = tokio::task::spawn_blocking(move || archive::write_transcript(&transcript_path, &transcript_text))
    .await
    .expect("transcript persistence does not panic");
  if let Err(error) = written {
    return responses::internal_error("OSError", error.to_string());
  }
  log_voice_transcribed(&Transcribed {
    session_id: &session_id,
    audio_path: &audio_path,
    audio_bytes_size: pcm.len(),
    text: &transcript.text,
    produced_by: &transcript.backend_id,
    selected_backend: upload.backend.as_deref(),
    devices: devices.as_ref(),
    end_to_final_s: None,
  });
  responses::fast_json(StatusCode::OK, &json!({ "text": transcript.text }))
}

/// The upload form's `devices` field as its object; None when the field is absent. A present field must be a JSON
/// object with exactly the device keys: the browser always sends the whole object.
fn devices_form_field(raw: Option<&str>) -> Result<Option<Value>, VoiceRequestError> {
  let Some(raw) = raw else {
    return Ok(None);
  };
  let parsed = json_loads::loads_text(raw)
    .map_err(|error| VoiceRequestError::bad_request(format!("malformed devices form field: {error}")))?;
  let has_device_keys = parsed.as_object().is_some_and(|object| {
    object.len() == DEVICE_FIELDS.len() && DEVICE_FIELDS.iter().all(|field| object.contains_key(*field))
  });
  if !has_device_keys {
    return Err(VoiceRequestError::bad_request(format!(
      "devices form field must carry exactly {}",
      text_repr::value_repr(&json!(DEVICE_FIELDS))
    )));
  }
  Ok(Some(parsed))
}

/// The request's WAV as 16 kHz mono PCM16 frames, no longer than `max_samples`. Anything else is a 400.
fn wav_body_to_pcm(body: &[u8], max_samples: usize) -> Result<Vec<u8>, VoiceRequestError> {
  let wave = wav::read(body).map_err(|error| VoiceRequestError::bad_request(format!("malformed WAV body: {error}")))?;
  if (wave.channels, wave.sample_width, wave.frame_rate) != (1, 2, SAMPLE_RATE as u32) {
    return Err(VoiceRequestError::bad_request(format!(
      "voice upload must be 16 kHz mono PCM16 WAV, got {}ch/{}bit/{}Hz",
      wave.channels,
      wave.sample_width * 8,
      wave.frame_rate
    )));
  }
  if wave.frames.len() > max_samples * 2 {
    return Err(VoiceRequestError::bad_request(format!(
      "voice recording exceeds the {}s limit",
      max_samples / SAMPLE_RATE
    )));
  }
  Ok(wave.frames.to_vec())
}

fn error_response(error: VoiceRequestError) -> Response {
  responses::fast_json(error.status, &json!({ "error": error.message }))
}

/// The fields of one `voice_transcribed` line. Both archive paths log it; the relay adds `end_to_final_s`.
pub(crate) struct Transcribed<'a> {
  pub(crate) session_id: &'a str,
  pub(crate) audio_path: &'a Path,
  pub(crate) audio_bytes_size: usize,
  pub(crate) text: &'a str,
  pub(crate) produced_by: &'a str,
  /// The backend the browser selected, as sent; None when the form named none.
  pub(crate) selected_backend: Option<&'a str>,
  /// The recording's device object; None, every device field logs as null, for a page loaded before devices existed.
  pub(crate) devices: Option<&'a Value>,
  pub(crate) end_to_final_s: Option<f64>,
}

pub(crate) fn log_voice_transcribed(line: &Transcribed<'_>) {
  let device = |field: &str| device_text(line.devices.and_then(|devices| devices.get(field)));
  let preview: String = line.text.chars().take(80).collect();
  let selected_backend = line.selected_backend.unwrap_or("null");
  tracing::info!(
    target: "charliebot_voice",
    session_id = line.session_id,
    audio_path = %line.audio_path.display(),
    audio_bytes_size = line.audio_bytes_size,
    transcription_length = line.text.chars().count(),
    transcription_preview = %preview,
    backend = line.produced_by,
    selected_backend,
    input_device = %device("input_device"),
    capture_settings = %device("capture_settings"),
    output_device = %device("output_device"),
    communications_output_device = %device("communications_output_device"),
    device_error = %device("device_error"),
    end_to_final_s = line.end_to_final_s,
    "voice_transcribed"
  );
}

/// One device field's text as the parity log comparison reads it from both servers.
/// A string reads as itself, a scalar in its JSON spelling (null, true, 3), a container in Python's repr, and an
/// absent field as null.
fn device_text(value: Option<&Value>) -> String {
  match value {
    None | Some(Value::Null) => "null".to_string(),
    Some(Value::String(text)) => text.clone(),
    Some(Value::Bool(flag)) => flag.to_string(),
    Some(Value::Number(number)) => number.to_string(),
    Some(container) => text_repr::value_repr(container),
  }
}

#[cfg(test)]
mod tests {
  use std::future::Future;
  use std::path::{Path, PathBuf};
  use std::sync::{Arc, Mutex};

  use axum::body::to_bytes;
  use charliebot_core::credentials::Credentials;
  use charliebot_core::test_support::EventSink;
  use futures_util::future::BoxFuture;

  use super::*;

  const SESSION_ID: &str = "session-a";
  const BACKEND_ID: &str = "gemini";
  const FINAL_TEXT: &str = "synthetic remote words";
  const DEVICES_JSON: &str = r#"{"input_device": "Test Microphone", "capture_settings": {"echoCancellation": true, "noiseSuppression": false, "autoGainControl": true, "sampleRate": 16000}, "output_device": "Test Speakers", "communications_output_device": "Test Comms Headset", "device_error": null}"#;

  type Event = serde_json::Map<String, Value>;

  #[derive(Default)]
  struct FakeState {
    calls: Vec<FakeCall>,
    unavailable: Option<String>,
    fail: bool,
  }

  struct FakeCall {
    chunks: Vec<Vec<u8>>,
    vocabulary: Vec<String>,
    languages: Vec<String>,
    wavs_on_disk: usize,
  }

  struct FakeFactory {
    state: Arc<Mutex<FakeState>>,
    sessions_dir: PathBuf,
  }

  impl RecordingBackendFactory for FakeFactory {
    fn backend_ids(&self) -> &'static [&'static str] {
      recording::BACKEND_IDS
    }

    fn build(
      &self,
      backend_id: &str,
      _credentials: &Credentials,
    ) -> Result<Box<dyn recording::RecordingBackend>, String> {
      Ok(Box::new(FakeBackend {
        id: backend_id.to_string(),
        state: Arc::clone(&self.state),
        sessions_dir: self.sessions_dir.clone(),
      }))
    }
  }

  struct FakeBackend {
    id: String,
    state: Arc<Mutex<FakeState>>,
    sessions_dir: PathBuf,
  }

  impl recording::RecordingBackend for FakeBackend {
    fn id(&self) -> &str {
      &self.id
    }

    fn unavailable_reason(&self) -> Option<String> {
      self.state.lock().expect("the fake lock is not poisoned").unavailable.clone()
    }

    fn transcribe<'a>(
      &'a self,
      chunks: Vec<Vec<u8>>,
      vocabulary: &'a [String],
      languages: &'a [String],
    ) -> BoxFuture<'a, Result<String, String>> {
      Box::pin(async move {
        let wavs_on_disk = files_under(&self.sessions_dir)
          .iter()
          .filter(|path| path.extension().is_some_and(|extension| extension == "wav"))
          .count();
        let mut state = self.state.lock().expect("the fake lock is not poisoned");
        state.calls.push(FakeCall {
          chunks,
          vocabulary: vocabulary.to_vec(),
          languages: languages.to_vec(),
          wavs_on_disk,
        });
        if state.fail {
          return Err("fake transcription failure".to_string());
        }
        Ok(FINAL_TEXT.to_string())
      })
    }
  }

  fn synthetic_config(home: &tempfile::TempDir) -> (Arc<CharlieBotConfig>, Arc<Credentials>) {
    std::fs::write(home.path().join("config.yaml"), "").expect("write the synthetic config");
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("src.features.voice", crate::register).expect("register the voice config section");
    let cfg = Arc::new(registry.load_config(home.path()).expect("load the synthetic config"));
    let credentials = Arc::new(charliebot_core::credentials::load_credentials(home.path()).expect("load credentials"));
    (cfg, credentials)
  }

  fn fake_factory(cfg: &CharlieBotConfig) -> (FakeFactory, Arc<Mutex<FakeState>>) {
    let state = Arc::new(Mutex::new(FakeState::default()));
    (FakeFactory { state: Arc::clone(&state), sessions_dir: cfg.sessions_dir() }, state)
  }

  fn wav_body(samples: usize) -> Vec<u8> {
    let directory = tempfile::tempdir().expect("make a scratch directory");
    let path = directory.path().join("recording.wav");
    archive::write_wav(&path, &vec![0_u8; samples * 2]).expect("write a synthetic WAV");
    std::fs::read(path).expect("read the synthetic WAV")
  }

  fn upload(backend: Option<&str>, devices: Option<&str>, samples: usize) -> UploadForm {
    UploadForm { audio: wav_body(samples), backend: backend.map(str::to_string), devices: devices.map(str::to_string) }
  }

  async fn post(
    cfg: &Arc<CharlieBotConfig>,
    credentials: &Arc<Credentials>,
    state: &Arc<Mutex<FakeState>>,
    backend: Option<&str>,
    devices: Option<&str>,
    samples: usize,
  ) -> (Response, Vec<Event>) {
    let factory = FakeFactory { state: Arc::clone(state), sessions_dir: cfg.sessions_dir() };
    capture_events_async(upload_recording(
      Arc::clone(cfg),
      Arc::clone(credentials),
      SESSION_ID.to_string(),
      upload(backend, devices, samples),
      &factory,
    ))
    .await
  }

  fn voice_dir(cfg: &CharlieBotConfig) -> PathBuf {
    cfg.sessions_dir().join(SESSION_ID).join("voice")
  }

  fn files_under(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
      return Vec::new();
    };
    entries
      .flat_map(|entry| {
        let path = entry.expect("read a directory entry").path();
        if path.is_dir() { files_under(&path) } else { vec![path] }
      })
      .collect()
  }

  fn transcribed(events: &[Event]) -> Vec<&Event> {
    events.iter().filter(|event| event["event"] == "voice_transcribed").collect()
  }

  async fn body_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("read the response body");
    String::from_utf8(bytes.to_vec()).expect("the response is UTF-8")
  }

  async fn capture_events_async<T>(future: impl Future<Output = T>) -> (T, Vec<Event>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let capture = charliebot_core::test_support::install_events(Arc::new(CaptureSink(Arc::clone(&events))));
    let result = future.await;
    drop(capture);
    let captured = events.lock().expect("the capture lock is not poisoned").clone();
    (result, captured)
  }

  struct CaptureSink(Arc<Mutex<Vec<Event>>>);

  impl EventSink for CaptureSink {
    fn event(&self, event: &tracing::Event<'_>) {
      let mut fields = Event::new();
      event.record(&mut FieldCapture(&mut fields));
      self.0.lock().expect("the capture lock is not poisoned").push(fields);
    }
  }

  struct FieldCapture<'a>(&'a mut Event);

  impl FieldCapture<'_> {
    fn key(field: &tracing::field::Field) -> String {
      if field.name() == "message" { "event".to_string() } else { field.name().to_string() }
    }
  }

  impl tracing::field::Visit for FieldCapture<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
      self.0.insert(Self::key(field), Value::String(format!("{value:?}")));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
      self.0.insert(Self::key(field), Value::String(value.to_string()));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
      self.0.insert(Self::key(field), Value::from(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
      self.0.insert(Self::key(field), Value::from(value));
    }
  }

  #[tokio::test]
  async fn test_a_live_backend_id_returns_its_final_text_after_the_recording_is_on_disk() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);
    let samples = recording::VOICE_CHUNK_SAMPLES + 10;

    let (response, logs) = post(&cfg, &credentials, &state, Some(BACKEND_ID), None, samples).await;

    assert_eq!(body_text(response).await, r#"{"text":"synthetic remote words"}"#);
    let state = state.lock().expect("the fake lock is not poisoned");
    let [call] = state.calls.as_slice() else { panic!("one live backend call was recorded") };
    assert_eq!(call.chunks.iter().map(Vec::len).collect::<Vec<_>>(), [recording::VOICE_CHUNK_SAMPLES * 2, 20]);
    assert_eq!(call.chunks.concat(), vec![0_u8; samples * 2]);
    assert!(call.vocabulary.is_empty());
    assert!(call.languages.is_empty());
    assert_eq!(call.wavs_on_disk, 1);
    let wavs: Vec<PathBuf> = files_under(&voice_dir(&cfg))
      .into_iter()
      .filter(|path| path.extension().is_some_and(|extension| extension == "wav"))
      .collect();
    assert_eq!(wavs.len(), 1);
    assert_eq!(std::fs::read_to_string(wavs[0].with_extension("txt")).expect("read the transcript"), FINAL_TEXT);
    let transcribed = transcribed(&logs);
    assert_eq!(transcribed.len(), 1);
    assert_eq!(transcribed[0]["backend"], BACKEND_ID);
    assert_eq!(transcribed[0]["selected_backend"], BACKEND_ID);
  }

  #[tokio::test]
  async fn test_a_transcription_failure_returns_the_error_and_leaves_the_wav_on_disk() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);
    state.lock().expect("the fake lock is not poisoned").fail = true;

    let (response, logs) = post(&cfg, &credentials, &state, Some(BACKEND_ID), None, 160).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_text(response).await;
    assert!(body.starts_with(r#"{"error":"voice transcription failed"#));
    assert!(body.contains("fake transcription failure"));
    let files = files_under(&voice_dir(&cfg));
    assert_eq!(files.iter().filter(|path| path.extension().is_some_and(|extension| extension == "wav")).count(), 1);
    assert!(!files.iter().any(|path| path.extension().is_some_and(|extension| extension == "txt")));
    assert!(transcribed(&logs).is_empty());
  }

  #[tokio::test]
  async fn test_an_unavailable_selection_is_a_400_that_keeps_the_recording() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);
    state.lock().expect("the fake lock is not poisoned").unavailable = Some("needs gemini.api_key".to_string());

    let (response, logs) = post(&cfg, &credentials, &state, Some(BACKEND_ID), None, 160).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, r#"{"error":"gemini is unavailable: needs gemini.api_key"}"#);
    assert!(state.lock().expect("the fake lock is not poisoned").calls.is_empty());
    assert_eq!(
      files_under(&voice_dir(&cfg))
        .iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "wav"))
        .count(),
      1
    );
    assert!(transcribed(&logs).is_empty());
  }

  #[tokio::test]
  async fn test_an_unknown_backend_id_is_a_400_that_keeps_the_recording() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);

    let (response, logs) = post(&cfg, &credentials, &state, Some("does-not-exist"), None, 160).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("does-not-exist"));
    assert_eq!(
      files_under(&voice_dir(&cfg))
        .iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "wav"))
        .count(),
      1
    );
    assert!(transcribed(&logs).is_empty());
  }

  #[tokio::test]
  async fn test_an_absent_backend_field_selects_the_default_backend() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);

    let (response, logs) = post(&cfg, &credentials, &state, None, None, 160).await;

    assert_eq!(body_text(response).await, r#"{"text":"synthetic remote words"}"#);
    assert_eq!(state.lock().expect("the fake lock is not poisoned").calls.len(), 1);
    let transcribed = transcribed(&logs);
    assert_eq!(transcribed.len(), 1);
    assert_eq!(transcribed[0]["backend"], BACKEND_ID);
    assert_eq!(transcribed[0]["selected_backend"], "null");
  }

  #[test]
  fn test_the_confirm_probe_endpoint_no_longer_exists() {
    assert!(!matches("/api/voice/session-a/confirm"));
  }

  #[tokio::test]
  async fn test_a_devices_field_is_logged_field_for_field() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);

    let (response, logs) = post(&cfg, &credentials, &state, Some(BACKEND_ID), Some(DEVICES_JSON), 160).await;

    assert_eq!(body_text(response).await, r#"{"text":"synthetic remote words"}"#);
    let transcribed = transcribed(&logs);
    assert_eq!(transcribed.len(), 1);
    let line = transcribed[0];
    assert_eq!(line["input_device"], "Test Microphone");
    assert_eq!(
      line["capture_settings"],
      "{'echoCancellation': True, 'noiseSuppression': False, 'autoGainControl': True, 'sampleRate': 16000}"
    );
    assert_eq!(line["output_device"], "Test Speakers");
    assert_eq!(line["communications_output_device"], "Test Comms Headset");
    assert_eq!(line["device_error"], "null");
    assert_eq!(line["backend"], BACKEND_ID);
    assert_eq!(line["selected_backend"], BACKEND_ID);
    assert_eq!(line["transcription_preview"], FINAL_TEXT);
  }

  #[tokio::test]
  async fn test_an_absent_devices_field_logs_all_five_as_none() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);

    let (response, logs) = post(&cfg, &credentials, &state, Some(BACKEND_ID), None, 160).await;

    assert_eq!(response.status(), StatusCode::OK);
    let line = transcribed(&logs)[0];
    for field in ["input_device", "capture_settings", "output_device", "communications_output_device", "device_error"] {
      assert_eq!(line[field], "null", "{field}");
    }
  }

  #[tokio::test]
  async fn test_malformed_devices_json_is_a_400_that_persists_nothing() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);

    let (response, logs) = post(&cfg, &credentials, &state, Some(BACKEND_ID), Some("not json at all"), 160).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("malformed devices form field"));
    assert!(transcribed(&logs).is_empty());
    assert!(files_under(&cfg.sessions_dir()).is_empty());
  }

  #[tokio::test]
  async fn test_a_devices_field_with_a_wrong_key_set_is_a_400() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);
    let short = r#"{"input_device": "Test Microphone", "capture_settings": null, "communications_output_device": null, "device_error": null}"#;

    let (response, logs) = post(&cfg, &credentials, &state, Some(BACKEND_ID), Some(short), 160).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("exactly"));
    assert!(transcribed(&logs).is_empty());
    assert!(files_under(&cfg.sessions_dir()).is_empty());
  }
}

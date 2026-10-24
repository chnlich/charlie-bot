//! The voice upload routes: `POST /api/voice/{session_id}` (the fallback upload) and `POST /api/voice/{session_id}/confirm`
//! (the recognition probe).
//!
//! Port of `src/features/voice/api.py`'s upload half. The upload persists the recording before it decodes it, so a
//! decode failure or a model that is not ready (503, which persists nothing) leaves the audio on disk. A selected
//! non-live backend decodes the upload itself; every other backend value keeps the local decode. The preview relay
//! lives in relay.rs and shares the `voice_transcribed` line written here.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequestParts, Path as AxumPath, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use charliebot_compat::fastapi_form::{self, Field, FormModel};
use charliebot_compat::routing::post_only;
use charliebot_compat::{python_json, responses};
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::FeatureContext;
use serde_json::{Value, json};

use crate::archive;
use crate::audio::{MAX_RECORDING_SAMPLES, SAMPLE_RATE};
use crate::config::VoiceSettings;
use crate::provisioning::SpeechModelsNotReadyError;
use crate::transcriber::{self, SpeechModelBundle};
use crate::wav;

/// The local transcription backend's id: the id a local decode logs as its producer.
const LOCAL_BACKEND_ID: &str = "local";
/// The recognition probe decodes the recording's opening 10 s only.
const CONFIRM_MAX_SECONDS: usize = 10;
/// The recording's device facts, which the browser sends as the upload form's `devices` field and the relay's end
/// frame, and which `voice_transcribed` logs under these names.
pub(crate) const DEVICE_FIELDS: &[&str] =
  &["input_device", "capture_settings", "output_device", "communications_output_device", "device_error"];

const UPLOAD_FIELDS: &[Field] =
  &[Field::file("audio"), Field::optional_form("backend"), Field::optional_form("devices")];
const UPLOAD_FORM: FormModel = FormModel::new(UPLOAD_FIELDS);

/// The routes of `/api/voice`, with the feature context as their state.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/voice/{session_id}/confirm", post_only(confirm_voice_recording))
    .route("/api/voice/{session_id}", post_only(upload_voice_recording))
    .with_state(Arc::clone(features))
}

/// Whether a request path falls under the voice upload routes, for the trailing-slash redirect the server answers.
pub(crate) fn matches(path: &str) -> bool {
  let Some(rest) = path.strip_prefix("/api/voice/") else {
    return false;
  };
  let session_id = rest.strip_suffix("/confirm").unwrap_or(rest);
  !session_id.is_empty() && !session_id.contains('/')
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

/// The form fields of one upload, read before the handler decides which backend decodes them.
pub(crate) struct UploadForm {
  pub(crate) audio: Vec<u8>,
  pub(crate) backend: Option<String>,
  pub(crate) devices: Option<String>,
}

/// A non-live transcription backend the upload can select: one whole-clip decode and no partials.
pub(crate) trait ClipBackend {
  /// The backend's id, which the `voice_transcribed` line logs as its producer.
  fn id(&self) -> &str;
  /// Why the backend cannot run now, e.g. `needs fake.api_key`; None when it is ready.
  fn unavailable_reason(&self) -> Option<String>;
  /// The final text of one recording: mono 16 kHz PCM16, with the config's hints.
  fn transcribe(&self, pcm: &[u8], vocabulary: &[String], languages: &[String]) -> anyhow::Result<String>;
}

/// The non-live backends the upload can select, by id. Every other id answers None: the local id, the live
/// backends (which the preview relay owns), and the unknown ids keep the local decode.
pub(crate) trait ClipBackends {
  fn clip_backend(&self, id: &str) -> Option<Box<dyn ClipBackend>>;
}

/// The registry this build serves: the local decode and the live Gemini and Muse streams, so no id is a clip backend.
struct NoClipBackends;

impl ClipBackends for NoClipBackends {
  fn clip_backend(&self, _id: &str) -> Option<Box<dyn ClipBackend>> {
    None
  }
}

/// `POST /api/voice/{session_id}/confirm`: decode the recording's opening clip as the probe; nothing is persisted.
async fn confirm_voice_recording(State(features): State<Arc<FeatureContext>>, request: Request) -> Response {
  let (mut parts, body) = request.into_parts();
  let AxumPath(session_id) = match AxumPath::<String>::from_request_parts(&mut parts, &features).await {
    Ok(path) => path,
    Err(response) => return response.into_response(),
  };
  let body = match axum::body::to_bytes(body, usize::MAX).await {
    Ok(body) => body,
    Err(error) => return responses::internal_error("ClientDisconnect", error.to_string()),
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let repo_root = match features.startup.require::<RepoRoot>() {
    Ok(repo_root) => repo_root,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let pcm = match wav_body_to_pcm(&body, CONFIRM_MAX_SECONDS * SAMPLE_RATE) {
    Ok(pcm) => pcm,
    Err(error) => return error_response(error),
  };
  let decoded = tokio::task::spawn_blocking(move || decode_probe(&cfg, repo_root.path(), &session_id, &pcm))
    .await
    .expect("the probe decode does not panic");
  match decoded {
    Ok(text) => responses::fast_json(StatusCode::OK, &json!({ "text": text })),
    Err(error) => error_response(error),
  }
}

/// `POST /api/voice/{session_id}`: the fallback upload. The multipart form carries the WAV as `audio`, the browser's
/// selected `backend`, and its `devices` object as JSON.
async fn upload_voice_recording(State(features): State<Arc<FeatureContext>>, request: Request) -> Response {
  let (mut parts, stream) = request.into_parts();
  let form = match fastapi_form::read(&parts.headers, stream).await {
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
  let repo_root = match features.startup.require::<RepoRoot>() {
    Ok(repo_root) => repo_root,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  tokio::task::spawn_blocking(move || {
    upload_recording(
      &cfg,
      &session_id,
      upload,
      || acquire_speech_bundle(&cfg, repo_root.path()),
      |bundle, pcm| transcribe_with_bundle(&session_id, &bundle, pcm),
      &NoClipBackends,
    )
  })
  .await
  .expect("the upload does not panic")
}

/// The upload after its form is read: the devices field, the WAV, the backend dispatch, the persist-before-decode
/// order, the transcript and the log line. `speech` acquires the resident models (a 503 when they are not ready) and
/// `decode` transcribes the persisted recording with them; the route passes the sherpa bundle, the tests pass fakes.
fn upload_recording<B>(
  cfg: &CharlieBotConfig,
  session_id: &str,
  upload: UploadForm,
  speech: impl FnOnce() -> Result<B, VoiceRequestError>,
  decode: impl FnOnce(B, &[u8]) -> Result<String, VoiceRequestError>,
  clips: &dyn ClipBackends,
) -> Response {
  let devices = match devices_form_field(upload.devices.as_deref()) {
    Ok(devices) => devices,
    Err(error) => return error_response(error),
  };
  let pcm = match wav_body_to_pcm(&upload.audio, MAX_RECORDING_SAMPLES) {
    Ok(pcm) => pcm,
    Err(error) => return error_response(error),
  };
  let clip = match selected_clip_backend(clips, upload.backend.as_deref()) {
    Ok(clip) => clip,
    Err(error) => return error_response(error),
  };
  if let Some(clip) = clip {
    return upload_via_clip_backend(cfg, session_id, clip.as_ref(), upload.backend.as_deref(), &pcm, devices.as_ref());
  }
  // The models come first, so a not-ready answer (503) persists nothing and the client's retry leaves no orphan WAV.
  let bundle = match speech() {
    Ok(bundle) => bundle,
    Err(error) => return error_response(error),
  };
  let audio_path = match archive::persist_recording(cfg, session_id, &pcm) {
    Ok(path) => path,
    Err(error) => return responses::internal_error("OSError", error.to_string()),
  };
  // A decode failure (500) leaves the WAV on disk: the recording survives every later failure.
  let text = match decode(bundle, &pcm) {
    Ok(text) => text,
    Err(error) => return error_response(error),
  };
  if let Err(error) = archive::write_transcript(&audio_path, &text) {
    return responses::internal_error("OSError", error.to_string());
  }
  log_voice_transcribed(&Transcribed {
    session_id,
    audio_path: &audio_path,
    audio_bytes_size: pcm.len(),
    text: &text,
    produced_by: LOCAL_BACKEND_ID,
    selected_backend: upload.backend.as_deref(),
    devices: devices.as_ref(),
    end_to_final_s: None,
  });
  responses::fast_json(StatusCode::OK, &json!({ "text": text }))
}

/// The clip backend's upload: persist the WAV, decode it with the backend, then write the transcript. A failed decode
/// answers 500 and leaves the WAV for the browser's retry.
fn upload_via_clip_backend(
  cfg: &CharlieBotConfig,
  session_id: &str,
  clip: &dyn ClipBackend,
  selected_backend: Option<&str>,
  pcm: &[u8],
  devices: Option<&Value>,
) -> Response {
  let audio_path = match archive::persist_recording(cfg, session_id, pcm) {
    Ok(path) => path,
    Err(error) => return responses::internal_error("OSError", error.to_string()),
  };
  let voice = cfg.section::<VoiceSettings>("voice");
  let text = match clip.transcribe(pcm, &voice.vocabulary, &voice.languages) {
    Ok(text) => text,
    Err(error) => {
      tracing::error!(session_id, backend = clip.id(), exception = %format!("{error:#}"), "voice_cloud_transcribe_failed");
      return error_response(VoiceRequestError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("voice transcription failed: {error:#}"),
      ));
    }
  };
  if let Err(error) = archive::write_transcript(&audio_path, &text) {
    return responses::internal_error("OSError", error.to_string());
  }
  log_voice_transcribed(&Transcribed {
    session_id,
    audio_path: &audio_path,
    audio_bytes_size: pcm.len(),
    text: &text,
    produced_by: clip.id(),
    selected_backend,
    devices,
    end_to_final_s: None,
  });
  responses::fast_json(StatusCode::OK, &json!({ "text": text }))
}

/// The registered non-live backend the form's `backend` names; None for the local decode. An absent or empty value,
/// the local id, a live id and an unknown id keep the local decode. A selected non-live backend that is unavailable is
/// an error: the selection never falls back to the local model.
fn selected_clip_backend(
  clips: &dyn ClipBackends,
  backend: Option<&str>,
) -> Result<Option<Box<dyn ClipBackend>>, VoiceRequestError> {
  let Some(id) = backend.filter(|id| !id.is_empty() && *id != LOCAL_BACKEND_ID) else {
    return Ok(None);
  };
  let Some(candidate) = clips.clip_backend(id) else {
    return Ok(None);
  };
  if let Some(reason) = candidate.unavailable_reason() {
    return Err(VoiceRequestError::bad_request(format!("{id} is unavailable: {reason}")));
  }
  Ok(Some(candidate))
}

/// The upload form's `devices` field as its object; None when the field is absent. A present field must be a JSON
/// object with exactly the device keys: the browser always sends the whole object.
fn devices_form_field(raw: Option<&str>) -> Result<Option<Value>, VoiceRequestError> {
  let Some(raw) = raw else {
    return Ok(None);
  };
  let parsed = python_json::loads_text(raw)
    .map_err(|error| VoiceRequestError::bad_request(format!("malformed devices form field: {error}")))?;
  let has_device_keys = parsed.as_object().is_some_and(|object| {
    object.len() == DEVICE_FIELDS.len() && DEVICE_FIELDS.iter().all(|field| object.contains_key(*field))
  });
  if !has_device_keys {
    return Err(VoiceRequestError::bad_request(format!(
      "devices form field must carry exactly {}",
      python_repr(&json!(DEVICE_FIELDS))
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

/// The resident speech bundle. A not-ready model answers 503; any other failure answers 500.
fn acquire_speech_bundle(
  cfg: &CharlieBotConfig,
  repo_root: &Path,
) -> Result<Arc<SpeechModelBundle>, VoiceRequestError> {
  transcriber::get_transcription_bundle(cfg, repo_root).map_err(|error| {
    match error.downcast_ref::<SpeechModelsNotReadyError>() {
      Some(not_ready) => VoiceRequestError::new(StatusCode::SERVICE_UNAVAILABLE, not_ready.to_string()),
      None => {
        tracing::error!(exception = %format!("{error:#}"), "voice_bundle_acquire_failed");
        VoiceRequestError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("speech inference failed: {error:#}"))
      }
    }
  })
}

fn transcribe_with_bundle(
  session_id: &str,
  bundle: &SpeechModelBundle,
  pcm: &[u8],
) -> Result<String, VoiceRequestError> {
  transcriber::transcribe_pcm_offline(bundle, pcm).map_err(|error| {
    tracing::error!(session_id, exception = %format!("{error:#}"), "voice_decode_failed");
    VoiceRequestError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("speech inference failed: {error:#}"))
  })
}

fn decode_probe(
  cfg: &CharlieBotConfig,
  repo_root: &Path,
  session_id: &str,
  pcm: &[u8],
) -> Result<String, VoiceRequestError> {
  let bundle = acquire_speech_bundle(cfg, repo_root)?;
  transcribe_with_bundle(session_id, &bundle, pcm)
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

/// One device field's text as the parity log comparison reads it from both servers: a string as itself, a scalar in
/// its JSON spelling (null, true, 3), a container in Python's repr, and an absent field as null.
fn device_text(value: Option<&Value>) -> String {
  match value {
    None | Some(Value::Null) => "null".to_string(),
    Some(Value::String(text)) => text.clone(),
    Some(Value::Bool(flag)) => flag.to_string(),
    Some(Value::Number(number)) => number.to_string(),
    Some(container) => python_repr(container),
  }
}

/// Python's `repr` of a JSON value, for the containers the log and the error texts show.
pub(crate) fn python_repr(value: &Value) -> String {
  match value {
    Value::Null => "None".to_string(),
    Value::Bool(true) => "True".to_string(),
    Value::Bool(false) => "False".to_string(),
    Value::Number(number) => number.to_string(),
    Value::String(text) => charliebot_core::pyfmt::str_repr(text),
    Value::Array(values) => format!("[{}]", values.iter().map(python_repr).collect::<Vec<_>>().join(", ")),
    Value::Object(values) => format!(
      "{{{}}}",
      values
        .iter()
        .map(|(key, value)| format!("{}: {}", charliebot_core::pyfmt::str_repr(key), python_repr(value)))
        .collect::<Vec<_>>()
        .join(", ")
    ),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;
  use std::sync::Mutex;

  use axum::body::to_bytes;
  use charliebot_core::test_support::EventSink;

  const SESSION_ID: &str = "session-a";
  const BACKEND_ID: &str = "fake-remote";
  const DECODED_TEXT: &str = "synthetic decoded words";
  const REMOTE_TEXT: &str = "synthetic remote words";
  const DEVICES_JSON: &str = r#"{"input_device": "Test Microphone", "capture_settings": {"echoCancellation": true, "noiseSuppression": false, "autoGainControl": true, "sampleRate": 16000}, "output_device": "Test Speakers", "communications_output_device": "Test Comms Headset", "device_error": null}"#;

  type Event = serde_json::Map<String, Value>;

  /// A WAV body of 160 silent 16 kHz mono frames: the recording the upload tests post.
  fn wav_body() -> Vec<u8> {
    let dir = tempfile::tempdir().expect("make a scratch directory");
    let path = dir.path().join("body.wav");
    archive::write_wav(&path, &[0_u8; 320]).expect("write a synthetic WAV");
    std::fs::read(path).expect("read the synthetic WAV")
  }

  /// A config loaded from a temporary home whose `config.yaml` is `yaml`, with the voice package registered.
  fn synthetic_config(home: &Path, yaml: &str) -> CharlieBotConfig {
    std::fs::write(home.join("config.yaml"), yaml).expect("write the synthetic config");
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("src.features.voice", crate::register).expect("register the voice config section");
    registry.load_config(home).expect("load the synthetic config")
  }

  fn upload(backend: Option<&str>, devices: Option<&str>) -> UploadForm {
    UploadForm { audio: wav_body(), backend: backend.map(str::to_string), devices: devices.map(str::to_string) }
  }

  /// Runs the upload with a local decode that returns `DECODED_TEXT`, capturing the logs of the call.
  fn post_local(cfg: &CharlieBotConfig, form: UploadForm) -> (Response, Vec<Event>) {
    capture_events(|| {
      upload_recording(
        cfg,
        SESSION_ID,
        form,
        || Ok::<(), VoiceRequestError>(()),
        |(), _| Ok(DECODED_TEXT.to_string()),
        &NoClipBackends,
      )
    })
  }

  fn voice_dir(cfg: &CharlieBotConfig) -> PathBuf {
    cfg.sessions_dir().join(SESSION_ID).join("voice")
  }

  /// Every file under `dir`, recursively; empty when `dir` does not exist.
  fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
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

  /// Runs `call` with the shared test capture recording each event's fields, the way the Python tests capture structlog.
  fn capture_events<T>(call: impl FnOnce() -> T) -> (T, Vec<Event>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let result = {
      let _capture = charliebot_core::test_support::install_events(Arc::new(CaptureSink(Arc::clone(&events))));
      call()
    };
    let captured = events.lock().expect("the capture lock is never poisoned").clone();
    (result, captured)
  }

  struct CaptureSink(Arc<Mutex<Vec<Event>>>);

  impl EventSink for CaptureSink {
    fn event(&self, event: &tracing::Event<'_>) {
      let mut fields = Event::new();
      event.record(&mut FieldCapture(&mut fields));
      self.0.lock().expect("the capture lock is never poisoned").push(fields);
    }
  }

  struct FieldCapture<'a>(&'a mut Event);

  impl FieldCapture<'_> {
    /// The JSON log line names the event `event`; tracing keeps it in the `message` field.
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

  /// The fake non-live backend, registered for the cloud upload cases. It records each decode and the WAVs on disk
  /// at decode time, the persist-before-decode proof.
  #[derive(Default)]
  struct RemoteState {
    calls: Vec<RemoteCall>,
    unavailable: Option<String>,
    fail: bool,
  }

  #[derive(Debug)]
  struct RemoteCall {
    pcm: Vec<u8>,
    vocabulary: Vec<String>,
    languages: Vec<String>,
    wavs_on_disk: usize,
  }

  struct FakeRemote {
    state: Arc<Mutex<RemoteState>>,
    sessions_dir: PathBuf,
    backend_id: String,
  }

  impl ClipBackends for FakeRemote {
    fn clip_backend(&self, id: &str) -> Option<Box<dyn ClipBackend>> {
      (id == self.backend_id).then(|| {
        Box::new(FakeClip {
          state: Arc::clone(&self.state),
          sessions_dir: self.sessions_dir.clone(),
          backend_id: self.backend_id.clone(),
        }) as Box<dyn ClipBackend>
      })
    }
  }

  struct FakeClip {
    state: Arc<Mutex<RemoteState>>,
    sessions_dir: PathBuf,
    backend_id: String,
  }

  impl ClipBackend for FakeClip {
    fn id(&self) -> &str {
      &self.backend_id
    }

    fn unavailable_reason(&self) -> Option<String> {
      self.state.lock().expect("the fake's lock is never poisoned").unavailable.clone()
    }

    fn transcribe(&self, pcm: &[u8], vocabulary: &[String], languages: &[String]) -> anyhow::Result<String> {
      let wavs_on_disk =
        files_under(&self.sessions_dir).iter().filter(|path| path.extension().is_some_and(|ext| ext == "wav")).count();
      let mut state = self.state.lock().expect("the fake's lock is never poisoned");
      state.calls.push(RemoteCall {
        pcm: pcm.to_vec(),
        vocabulary: vocabulary.to_vec(),
        languages: languages.to_vec(),
        wavs_on_disk,
      });
      if state.fail {
        anyhow::bail!("fake transcription failure");
      }
      Ok(REMOTE_TEXT.to_string())
    }
  }

  fn remote(sessions_dir: &Path) -> (FakeRemote, Arc<Mutex<RemoteState>>) {
    remote_for(BACKEND_ID, sessions_dir)
  }

  fn remote_for(backend_id: &str, sessions_dir: &Path) -> (FakeRemote, Arc<Mutex<RemoteState>>) {
    let state = Arc::new(Mutex::new(RemoteState::default()));
    (
      FakeRemote {
        state: Arc::clone(&state),
        sessions_dir: sessions_dir.to_path_buf(),
        backend_id: backend_id.to_string(),
      },
      state,
    )
  }

  #[tokio::test]
  async fn a_devices_field_is_logged_field_for_field() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "");

    let (clips, _) = remote(&cfg.sessions_dir());
    let (response, logs) = capture_events(|| {
      upload_recording(
        &cfg,
        SESSION_ID,
        upload(Some(BACKEND_ID), Some(DEVICES_JSON)),
        || -> Result<(), VoiceRequestError> { panic!("the live backend must not acquire a local speech model") },
        |(), _| unreachable!("the live backend owns transcription"),
        &clips,
      )
    });

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
    assert_eq!(line["transcription_preview"], REMOTE_TEXT);
  }

  #[tokio::test]
  async fn an_absent_devices_field_logs_all_five_as_null() {
    // A page loaded before the devices existed keeps its old script across a server restart and sends no devices.
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "");

    let (clips, _) = remote(&cfg.sessions_dir());
    let (response, logs) = capture_events(|| {
      upload_recording(
        &cfg,
        SESSION_ID,
        upload(Some(BACKEND_ID), None),
        || -> Result<(), VoiceRequestError> { panic!("the live backend must not acquire a local speech model") },
        |(), _| unreachable!("the live backend owns transcription"),
        &clips,
      )
    });

    assert_eq!(response.status(), StatusCode::OK);
    let line = transcribed(&logs)[0];
    for field in ["input_device", "capture_settings", "output_device", "communications_output_device", "device_error"] {
      assert_eq!(line[field], "null", "{field}");
    }
  }

  #[tokio::test]
  async fn malformed_devices_json_is_a_400_that_persists_nothing() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "");

    let (response, logs) = post_local(&cfg, upload(Some(LOCAL_BACKEND_ID), Some("not json at all")));

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("malformed devices form field"));
    assert!(transcribed(&logs).is_empty());
    assert!(files_under(&cfg.sessions_dir()).is_empty());
  }

  #[tokio::test]
  async fn a_devices_field_with_a_wrong_key_set_is_a_400() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "");
    let short = r#"{"input_device": "Test Microphone", "capture_settings": null, "communications_output_device": null, "device_error": null}"#;

    let (response, logs) = post_local(&cfg, upload(Some(LOCAL_BACKEND_ID), Some(short)));

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("exactly"));
    assert!(transcribed(&logs).is_empty());
    assert!(files_under(&cfg.sessions_dir()).is_empty());
  }

  #[tokio::test]
  async fn test_a_live_backend_id_returns_its_final_text_after_the_recording_is_on_disk() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "voice:\n  vocabulary: [CharlieBot]\n  languages: [zh]\n");
    let (clips, state) = remote(&cfg.sessions_dir());
    let forbidden_speech =
      || -> Result<(), VoiceRequestError> { panic!("the cloud path must not acquire the local speech bundle") };

    let (response, logs) = capture_events(|| {
      upload_recording(
        &cfg,
        SESSION_ID,
        upload(Some(BACKEND_ID), None),
        forbidden_speech,
        |(), _| unreachable!("no local decode"),
        &clips,
      )
    });

    assert_eq!(body_text(response).await, r#"{"text":"synthetic remote words"}"#);
    let state = state.lock().expect("the fake's lock is never poisoned");
    assert_eq!(state.calls.len(), 1);
    let call = &state.calls[0];
    assert_eq!(call.pcm, vec![0_u8; 320]);
    assert_eq!(call.vocabulary, ["CharlieBot"]);
    assert_eq!(call.languages, ["zh"]);
    assert_eq!(call.wavs_on_disk, 1, "the recording was persisted before the backend ran");
    let wavs: Vec<PathBuf> = files_under(&voice_dir(&cfg))
      .into_iter()
      .filter(|path| path.extension().is_some_and(|ext| ext == "wav"))
      .collect();
    assert_eq!(wavs.len(), 1);
    assert_eq!(std::fs::read_to_string(wavs[0].with_extension("txt")).expect("read the transcript"), REMOTE_TEXT);
    let transcribed = transcribed(&logs);
    assert_eq!(transcribed.len(), 1);
    assert_eq!(transcribed[0]["backend"], BACKEND_ID);
    assert_eq!(transcribed[0]["selected_backend"], BACKEND_ID);
  }

  #[tokio::test]
  async fn test_a_transcription_failure_returns_the_error_and_leaves_the_wav_on_disk() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "");
    let (clips, state) = remote(&cfg.sessions_dir());
    state.lock().expect("the fake's lock is never poisoned").fail = true;

    let (response, logs) = capture_events(|| {
      upload_recording(
        &cfg,
        SESSION_ID,
        upload(Some(BACKEND_ID), None),
        || Ok::<(), VoiceRequestError>(()),
        |(), _| unreachable!("no local decode"),
        &clips,
      )
    });

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_text(response).await;
    assert!(body.contains("voice transcription failed"));
    assert!(body.contains("fake transcription failure"));
    let files = files_under(&voice_dir(&cfg));
    assert_eq!(files.iter().filter(|path| path.extension().is_some_and(|ext| ext == "wav")).count(), 1);
    assert!(!files.iter().any(|path| path.extension().is_some_and(|ext| ext == "txt")));
    assert!(transcribed(&logs).is_empty());
  }

  #[tokio::test]
  async fn test_an_unavailable_selection_is_a_400_that_keeps_the_recording() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "");
    let (clips, state) = remote(&cfg.sessions_dir());
    state.lock().expect("the fake's lock is never poisoned").unavailable = Some("needs fake.api_key".to_string());

    let (response, logs) = capture_events(|| {
      upload_recording(
        &cfg,
        SESSION_ID,
        upload(Some(BACKEND_ID), None),
        || Ok::<(), VoiceRequestError>(()),
        |(), _| unreachable!("no local decode"),
        &clips,
      )
    });

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, r#"{"error":"fake-remote is unavailable: needs fake.api_key"}"#);
    assert!(state.lock().expect("the fake's lock is never poisoned").calls.is_empty());
    assert_eq!(
      files_under(&voice_dir(&cfg)).iter().filter(|path| path.extension().is_some_and(|ext| ext == "wav")).count(),
      1
    );
    assert!(transcribed(&logs).is_empty());
  }

  #[tokio::test]
  async fn test_an_unknown_backend_id_is_a_400_that_keeps_the_recording() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "");

    let (response, _logs) = post_local(&cfg, upload(Some("does-not-exist"), None));

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("does-not-exist"));
    assert_eq!(
      files_under(&voice_dir(&cfg)).iter().filter(|path| path.extension().is_some_and(|ext| ext == "wav")).count(),
      1
    );
  }

  #[tokio::test]
  async fn test_an_absent_backend_field_selects_the_default_backend() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "voice:\n  default_backend: gemini\n");
    let (clips, state) = remote_for("gemini", &cfg.sessions_dir());

    let (response, logs) = capture_events(|| {
      upload_recording(
        &cfg,
        SESSION_ID,
        upload(None, None),
        || Ok::<(), VoiceRequestError>(()),
        |(), _| Ok(DECODED_TEXT.to_string()),
        &clips,
      )
    });

    assert_eq!(body_text(response).await, r#"{"text":"synthetic remote words"}"#);
    assert_eq!(state.lock().expect("the fake's lock is never poisoned").calls.len(), 1);
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
  async fn live_unknown_local_and_absent_ids_keep_the_local_path() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let cfg = synthetic_config(home.path(), "");
    let (clips, state) = remote(&cfg.sessions_dir());

    for backend in [Some("gemini"), Some("does-not-exist"), Some(LOCAL_BACKEND_ID), None] {
      let (response, logs) = capture_events(|| {
        upload_recording(
          &cfg,
          SESSION_ID,
          upload(backend, None),
          || Ok::<(), VoiceRequestError>(()),
          |(), _| Ok(DECODED_TEXT.to_string()),
          &clips,
        )
      });

      assert_eq!(body_text(response).await, r#"{"text":"synthetic decoded words"}"#, "{backend:?}");
      let transcribed = transcribed(&logs);
      assert_eq!(transcribed[0]["backend"], LOCAL_BACKEND_ID, "{backend:?}");
      assert_eq!(transcribed[0]["selected_backend"], backend.unwrap_or("null"), "{backend:?}");
    }
    assert!(state.lock().expect("the fake's lock is never poisoned").calls.is_empty());
  }

  #[test]
  fn a_registered_non_live_id_is_the_only_one_the_cloud_path_selects() {
    let (clips, _) = remote(Path::new("/nonexistent"));
    assert!(selected_clip_backend(&clips, Some(BACKEND_ID)).expect("the id selects").is_some());
    assert!(selected_clip_backend(&clips, Some(LOCAL_BACKEND_ID)).expect("local keeps the local path").is_none());
    assert!(
      selected_clip_backend(&NoClipBackends, Some(BACKEND_ID)).expect("an unknown id keeps the local path").is_none()
    );
  }
}

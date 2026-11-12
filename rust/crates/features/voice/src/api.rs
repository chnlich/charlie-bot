//! The voice HTTP routes: the upload `POST /api/voice/{session_id}`, the startup report
//! `POST /api/voice/{session_id}/startup`, the recent recordings list `GET /api/voice/recordings`, and the
//! Recent recordings page `GET /voice-recordings`.
//!
//! The upload persists the recording before it selects an online backend. Selection failures answer 400, and
//! transcription failures answer 500; both leave the WAV on disk, and a transcription failure is recorded in the
//! run's `.json`. The preview relay lives in relay.rs and shares the `voice_transcribed` line written here. The
//! startup report validates one recording run's startup record, logs it as one `voice_startup` line, and persists
//! it as the run's `startup` part. Every route carries the run's id, validated as eight lowercase hex characters
//! before it reaches a path.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequestParts, Path as AxumPath, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use charliebot_compat::form_body::{self, Field, FormModel};
use charliebot_compat::request_body::{
  self, BodyModel, Field as BodyField, FieldType, flag_field, integer_field, optional_text_field_ref, text_field_ref,
};
use charliebot_compat::responses;
use charliebot_compat::routing::{get_only, post_only};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::credentials::Credentials;
use charliebot_core::json_loads;
use charliebot_core::text_repr;
use charliebot_registry::FeatureContext;
use charliebot_registry::templating::Templating;
use serde_json::{Value, json};

use crate::audio::{MAX_RECORDING_SAMPLES, SAMPLE_RATE};
use crate::config::VoiceSettings;
use crate::recording::{self, RecordingBackendFactory};
use crate::recordings;
use crate::wav;

/// The recording's device facts. The browser sends them as the upload form's `devices` field and as the relay's
/// end frame; `voice_transcribed` logs them under these names.
pub(crate) const DEVICE_FIELDS: &[&str] =
  &["input_device", "capture_settings", "output_device", "communications_output_device", "device_error"];

const UPLOAD_FIELDS: &[Field] =
  &[Field::file("audio"), Field::form("run_id"), Field::optional_form("backend"), Field::optional_form("devices")];
const UPLOAD_FORM: FormModel = FormModel::new(UPLOAD_FIELDS);

/// The Recent recordings page's template, in the voice package's template directory.
const RECORDINGS_PAGE_TEMPLATE: &str = "voice/recordings.html";

/// The Recent recordings page's path (`RECORDINGS_PAGE_PATH`): the page route and the home card's link.
pub(crate) const RECORDINGS_PAGE_PATH: &str = "/voice-recordings";

/// The routes of the voice feature, with the feature context as their state.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/voice/{session_id}", post_only(upload_voice_recording))
    .route("/api/voice/{session_id}/startup", post_only(post_voice_startup))
    .route("/api/voice/recordings", get_only(list_voice_recordings))
    .route(RECORDINGS_PAGE_PATH, get_only(recordings_page))
    .with_state(Arc::clone(features))
}

/// Whether a request path falls under the voice routes, for the trailing-slash redirect the server answers.
pub(crate) fn matches(path: &str) -> bool {
  if path == RECORDINGS_PAGE_PATH {
    return true;
  }
  let Some(rest) = path.strip_prefix("/api/voice/") else {
    return false;
  };
  let mut segments = rest.split('/');
  if !segments.next().is_some_and(|segment| !segment.is_empty()) {
    return false;
  }
  match segments.next() {
    None => true,
    Some("startup") | Some("recordings") => segments.next().is_none(),
    Some(_) => false,
  }
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

/// The form fields of one upload. `run_id` is the recording run's id, which the browser makes at the click.
pub(crate) struct UploadForm {
  pub(crate) audio: Vec<u8>,
  pub(crate) run_id: String,
  pub(crate) backend: Option<String>,
  pub(crate) devices: Option<String>,
}

/// The recording upload route: multipart fields carry the WAV, the run id, selected backend and device object, at
/// `POST /api/voice/{session_id}`.
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
  let run_id = form.form("run_id").expect("the required run_id field validated").to_string();
  let upload = UploadForm {
    audio,
    run_id,
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

// --- Startup report ---------------------------------------------------------
// One recording run reports its startup record once. The record holds the last of the three startup steps the
// run reached, how the recording ended, and the browser's audio and microphone state at that moment. The route
// keeps no state.

/// The step names of the startup report's `reached` field, in their progression order.
const STARTUP_STEPS: &[&str] = &["mic_opening", "no_frames", "silent", "ready"];
/// How a recording ended, as the startup report's `end` field names it.
const STARTUP_ENDS: &[&str] = &["stop", "startup_error", "left", "page_closed"];
/// The browser's microphone permission at the click.
const MIC_PERMISSIONS: &[&str] = &["granted", "prompt", "denied"];
/// How the audio engine's resume after both startup branches settled ended: not needed, started, or timed out.
const AUDIO_CONTEXT_RESUMES: &[&str] = &["not_needed", "started", "timed_out"];
/// The microphone track's readyState at the send.
const TRACK_READY_STATES: &[&str] = &["live", "ended"];
/// The longest string the startup line keeps, in code points, the same cut the diag route's `error` takes.
const MAX_STARTUP_TEXT_CHARS: usize = 300;

/// The device object the browser collected, the same five fields `voice_transcribed` logs.
const STARTUP_DEVICE_FIELDS: &[BodyField] = &[
  BodyField::required("input_device", FieldType::NullableString),
  BodyField::required("capture_settings", FieldType::Dictionary),
  BodyField::required("output_device", FieldType::NullableString),
  BodyField::required("communications_output_device", FieldType::NullableString),
  BodyField::required("device_error", FieldType::NullableString),
];
const STARTUP_DEVICES: BodyModel = BodyModel::required(STARTUP_DEVICE_FIELDS);

/// The startup body's fields in their declaration order, which is also the log line's field order; an unknown field
/// is refused. `run_id` identifies the recording run and reaches no log line.
const STARTUP_FIELDS: &[BodyField] = &[
  BodyField::required("run_id", FieldType::String),
  BodyField::required("backend", FieldType::String),
  BodyField::required("reached", FieldType::Literal(STARTUP_STEPS)),
  BodyField::required("end", FieldType::Literal(STARTUP_ENDS)),
  BodyField::required("error", FieldType::NullableString),
  BodyField::required("mic_open_ms", FieldType::NullableInteger),
  BodyField::required("graph_ready_ms", FieldType::NullableInteger),
  BodyField::required("first_frame_ms", FieldType::NullableInteger),
  BodyField::required("first_sound_ms", FieldType::NullableInteger),
  BodyField::required("end_ms", FieldType::Integer),
  BodyField::required("frames", FieldType::Integer),
  BodyField::required("samples", FieldType::Integer),
  BodyField::required("audio_context_state", FieldType::NullableString),
  BodyField::required("audio_context_rate", FieldType::NullableInteger),
  BodyField::required("audio_context_settled_state", FieldType::NullableString),
  BodyField::required("audio_context_resume", FieldType::NullableLiteral(AUDIO_CONTEXT_RESUMES)),
  BodyField::required("mic_permission", FieldType::NullableLiteral(MIC_PERMISSIONS)),
  BodyField::required("track_ready_state", FieldType::NullableLiteral(TRACK_READY_STATES)),
  BodyField::required("track_muted", FieldType::NullableBoolean),
  BodyField::required("devices", FieldType::NullableModel { class_name: "VoiceDevices", model: &STARTUP_DEVICES }),
  BodyField::required("user_agent", FieldType::String),
  BodyField::required(request_body::CLIENT_TS, FieldType::String),
];

const STARTUP_BODY: BodyModel = BodyModel::required(STARTUP_FIELDS).forbid_extra();

/// The startup report route: validate one startup record, persist it as the run's `startup` part, log one
/// `voice_startup` line, answer `{"ok": true}`. A validation failure answers 422 and logs nothing. The route
/// answers `POST /api/voice/{session_id}/startup`.
async fn post_voice_startup(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request,
) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let values = match body.validate(&STARTUP_BODY) {
    Ok(Some(Value::Object(values))) => values,
    Ok(_) => unreachable!("a required model validates a body to its fields"),
    Err(response) => return *response,
  };
  let run_id = text_field_ref(&values, "run_id").to_string();
  if !recordings::valid_run_id(&run_id) {
    return error_response(VoiceRequestError::bad_request(format!(
      "run_id must be eight lowercase hex characters, got {}",
      text_repr::str_repr(&run_id)
    )));
  }
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let mut startup = values.clone();
  startup.shift_remove("run_id");
  let startup_session_id = session_id.clone();
  let persisted = tokio::task::spawn_blocking(move || {
    recordings::write_startup_part(&cfg, &startup_session_id, &run_id, Value::Object(startup))
  })
  .await
  .expect("the startup persistence does not panic");
  if let Err(error) = persisted {
    return responses::internal_error("OSError", error.to_string());
  }
  log_voice_startup(&session_id, &VoiceStartup::from_values(&values));
  responses::fast_json(StatusCode::OK, &json!({"ok": true}))
}

/// The validated startup body, borrowed from the validated values.
struct VoiceStartup<'a> {
  backend: &'a str,
  reached: &'a str,
  end: &'a str,
  error: Option<&'a str>,
  mic_open_ms: Option<i128>,
  graph_ready_ms: Option<i128>,
  first_frame_ms: Option<i128>,
  first_sound_ms: Option<i128>,
  end_ms: i128,
  frames: i128,
  samples: i128,
  audio_context_state: Option<&'a str>,
  audio_context_rate: Option<i128>,
  audio_context_settled_state: Option<&'a str>,
  audio_context_resume: Option<&'a str>,
  mic_permission: Option<&'a str>,
  track_ready_state: Option<&'a str>,
  track_muted: Option<bool>,
  devices: Option<&'a Value>,
  user_agent: &'a str,
  client_ts: &'a str,
}

impl<'a> VoiceStartup<'a> {
  /// The startup body, with its fields read from `values` in model declaration order.
  fn from_values(values: &'a serde_json::Map<String, Value>) -> Self {
    Self {
      backend: text_field_ref(values, "backend"),
      reached: text_field_ref(values, "reached"),
      end: text_field_ref(values, "end"),
      error: optional_text_field_ref(values, "error"),
      mic_open_ms: integer_field(values, "mic_open_ms"),
      graph_ready_ms: integer_field(values, "graph_ready_ms"),
      first_frame_ms: integer_field(values, "first_frame_ms"),
      first_sound_ms: integer_field(values, "first_sound_ms"),
      end_ms: integer_field(values, "end_ms").expect("a required integer validates to a value"),
      frames: integer_field(values, "frames").expect("a required integer validates to a value"),
      samples: integer_field(values, "samples").expect("a required integer validates to a value"),
      audio_context_state: optional_text_field_ref(values, "audio_context_state"),
      audio_context_rate: integer_field(values, "audio_context_rate"),
      audio_context_settled_state: optional_text_field_ref(values, "audio_context_settled_state"),
      audio_context_resume: optional_text_field_ref(values, "audio_context_resume"),
      mic_permission: optional_text_field_ref(values, "mic_permission"),
      track_ready_state: optional_text_field_ref(values, "track_ready_state"),
      track_muted: flag_field(values, "track_muted"),
      devices: values.get("devices").filter(|value| !value.is_null()),
      user_agent: text_field_ref(values, "user_agent"),
      client_ts: text_field_ref(values, request_body::CLIENT_TS),
    }
  }
}

/// The one `voice_startup` line: the body's fields in declaration order. The device object is written as
/// `voice_transcribed` writes its five device fields, and an absent optional as null.
fn log_voice_startup(session_id: &str, startup: &VoiceStartup<'_>) {
  let device = |field: &str| device_text(startup.devices.and_then(|devices| devices.get(field)));
  tracing::info!(
    target: "charliebot_voice",
    session_id,
    backend = %cut_startup_text(startup.backend),
    reached = %cut_startup_text(startup.reached),
    end = %cut_startup_text(startup.end),
    error = %Nullable(startup.error.map(cut_startup_text)),
    mic_open_ms = %Nullable(startup.mic_open_ms),
    graph_ready_ms = %Nullable(startup.graph_ready_ms),
    first_frame_ms = %Nullable(startup.first_frame_ms),
    first_sound_ms = %Nullable(startup.first_sound_ms),
    end_ms = %startup.end_ms,
    frames = %startup.frames,
    samples = %startup.samples,
    audio_context_state = %Nullable(startup.audio_context_state.map(cut_startup_text)),
    audio_context_rate = %Nullable(startup.audio_context_rate),
    audio_context_settled_state = %Nullable(startup.audio_context_settled_state.map(cut_startup_text)),
    audio_context_resume = %Nullable(startup.audio_context_resume.map(cut_startup_text)),
    mic_permission = %Nullable(startup.mic_permission.map(cut_startup_text)),
    track_ready_state = %Nullable(startup.track_ready_state.map(cut_startup_text)),
    track_muted = %Nullable(startup.track_muted),
    input_device = %device("input_device"),
    capture_settings = %device("capture_settings"),
    output_device = %device("output_device"),
    communications_output_device = %device("communications_output_device"),
    device_error = %device("device_error"),
    user_agent = %cut_startup_text(startup.user_agent),
    client_ts = %cut_startup_text(startup.client_ts),
    "voice_startup"
  );
}

/// A string field cut to its first `MAX_STARTUP_TEXT_CHARS` code points.
fn cut_startup_text(text: &str) -> &str {
  match text.char_indices().nth(MAX_STARTUP_TEXT_CHARS) {
    Some((end, _)) => &text[..end],
    None => text,
  }
}

/// An optional log value: the value as itself, or `null` when it is absent, the spelling the diag route logs.
struct Nullable<T>(Option<T>);

impl<T: std::fmt::Display> std::fmt::Display for Nullable<T> {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.0 {
      Some(value) => value.fmt(formatter),
      None => formatter.write_str("null"),
    }
  }
}

/// The upload of one validated recording: persisted under its run's stem before backend selection, its final
/// transcript archived after, a transcription failure recorded in the run's `.json`.
async fn upload_recording(
  cfg: Arc<CharlieBotConfig>,
  credentials: Arc<Credentials>,
  session_id: String,
  upload: UploadForm,
  factory: &dyn RecordingBackendFactory,
) -> Response {
  if !recordings::valid_run_id(&upload.run_id) {
    return error_response(VoiceRequestError::bad_request(format!(
      "run_id must be eight lowercase hex characters, got {}",
      text_repr::str_repr(&upload.run_id)
    )));
  }
  let devices = match devices_form_field(upload.devices.as_deref()) {
    Ok(devices) => devices,
    Err(error) => return error_response(error),
  };
  let pcm = match wav_body_to_pcm(&upload.audio, MAX_RECORDING_SAMPLES) {
    Ok(pcm) => pcm,
    Err(error) => return error_response(error),
  };
  let voice = cfg.section::<VoiceSettings>("voice");
  // The backend the upload transcribes with: the browser's choice, or the configured default.
  let backend = upload.backend.clone().unwrap_or_else(|| voice.default_backend.clone());
  let persist_cfg = Arc::clone(&cfg);
  let persist_session_id = session_id.clone();
  let persist_run_id = upload.run_id.clone();
  let persist_pcm = pcm.clone();
  let persist_backend = backend.clone();
  let persist_devices = devices.clone();
  let persisted = tokio::task::spawn_blocking(move || {
    recordings::store_upload_audio(
      &persist_cfg,
      &persist_session_id,
      &persist_run_id,
      &persist_pcm,
      Some(&persist_backend),
      persist_devices.as_ref(),
    )
  })
  .await
  .expect("recording persistence does not panic");
  let audio_path = match persisted {
    Ok(path) => path,
    Err(error) => return responses::internal_error("OSError", error.to_string()),
  };
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
      let failure_cfg = Arc::clone(&cfg);
      let failure_session_id = session_id.clone();
      let failure_run_id = upload.run_id.clone();
      let failure_message = message.clone();
      let recorded = tokio::task::spawn_blocking(move || {
        recordings::write_upload_failure(&failure_cfg, &failure_session_id, &failure_run_id, &failure_message)
      })
      .await
      .expect("the failure recording does not panic");
      if let Err(error) = recorded {
        return responses::internal_error("OSError", error.to_string());
      }
      return error_response(VoiceRequestError::new(StatusCode::INTERNAL_SERVER_ERROR, message));
    }
  };
  let transcript_cfg = Arc::clone(&cfg);
  let transcript_session_id = session_id.clone();
  let transcript_run_id = upload.run_id.clone();
  let transcript_text = transcript.text.clone();
  let written = tokio::task::spawn_blocking(move || {
    recordings::write_upload_transcript(&transcript_cfg, &transcript_session_id, &transcript_run_id, &transcript_text)
  })
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

// --- Recent recordings -------------------------------------------------------
// The list route answers the JSON the API consumers read; the page route renders the same rows as HTML. Both
// scan the sessions' voice directories on the blocking pool.

/// The recent recordings JSON at `GET /api/voice/recordings?limit=N`: the newest rows across every session
/// directory, one per run.
async fn list_voice_recordings(State(features): State<Arc<FeatureContext>>, request: Request) -> Response {
  let (parts, _) = request.into_parts();
  let limit = match recordings::parse_list_limit(parts.uri.query().unwrap_or("")) {
    Ok(limit) => limit,
    Err(message) => return responses::fast_json(StatusCode::BAD_REQUEST, &json!({ "error": message })),
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  match tokio::task::spawn_blocking(move || recordings::list_recordings(&cfg, limit)).await {
    Ok(Ok(rows)) => responses::fast_json(StatusCode::OK, &json!({ "recordings": rows })),
    Ok(Err(error)) => responses::internal_error("OSError", error.to_string()),
    Err(error) => responses::internal_error("RuntimeError", error.to_string()),
  }
}

/// The Recent recordings page at `GET /voice-recordings`: the same rows as the list route, rendered as HTML.
async fn recordings_page(State(features): State<Arc<FeatureContext>>, parts: Parts) -> Response {
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let rendered = tokio::task::spawn_blocking(move || {
    let rows = recordings::list_recordings(&cfg, recordings::DEFAULT_LIST_LIMIT)
      .map_err(|error| ("OSError".to_string(), error.to_string()))?;
    let templating =
      features.startup.require::<Templating>().map_err(|error| ("RuntimeError".to_string(), error.to_string()))?;
    templating
      .render_page(&features, RECORDINGS_PAGE_TEMPLATE, &json!({ "recordings": rows }), &parts)
      .map_err(|error| (error.kind().to_string(), error.to_string()))
  })
  .await
  .expect("rendering the recordings page does not panic");
  match rendered {
    Ok(response) => response,
    Err((kind, detail)) => responses::internal_error(&kind, detail),
  }
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
/// A string reads as itself, a scalar in its JSON spelling (null, true, 3), a container in its repr spelling, and an
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
  use charliebot_core::hot_reload::HotReload;
  use charliebot_core::test_support::EventSink;
  use charliebot_registry::context::StartupState;
  use charliebot_registry::store_hooks;
  use charliebot_store::Store;
  use futures_util::future::BoxFuture;

  use super::*;

  const SESSION_ID: &str = "session-a";
  const BACKEND_ID: &str = "gemini";
  const RUN_ID: &str = "0a1b2c3d";
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
    fn backends(&self) -> &'static [crate::transcription::BackendSpec] {
      crate::transcription::BACKENDS
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
    registry.register_package("features.voice", crate::register).expect("register the voice config section");
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
    crate::wav::write(&path, &vec![0_u8; samples * 2]).expect("write a synthetic WAV");
    std::fs::read(path).expect("read the synthetic WAV")
  }

  fn upload(backend: Option<&str>, devices: Option<&str>, samples: usize) -> UploadForm {
    UploadForm {
      audio: wav_body(samples),
      run_id: RUN_ID.to_string(),
      backend: backend.map(str::to_string),
      devices: devices.map(str::to_string),
    }
  }

  /// The feature context over an isolated home, the state the startup route and the list route read.
  fn test_features() -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().expect("a temporary home is available");
    std::fs::write(home.path().join("config.yaml"), "").expect("write the synthetic config");
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("features.voice", crate::register).expect("register the voice config section");
    let registry = Arc::new(registry);
    let config_home = home.path().to_path_buf();
    let config_registry = Arc::clone(&registry);
    let config = Arc::new(HotReload::new(
      home.path().join("config.yaml"),
      "voice_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credentials_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join(charliebot_core::config::CREDENTIALS_FILENAME),
      "voice_test_credentials_reload_failed",
      Box::new(move || charliebot_core::credentials::load_credentials(&credentials_home)),
    ));
    let cfg = registry.load_config(home.path()).expect("load the synthetic config");
    let store = Arc::new(
      Store::new(cfg, registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
        .expect("the store opens over the temporary home"),
    );
    let features =
      Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(StartupState::default()) });
    (home, features)
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

  #[test]
  fn test_the_matcher_recognizes_the_voice_paths_only() {
    assert!(matches("/api/voice/session-a"));
    assert!(matches("/api/voice/session-a/startup"));
    assert!(matches("/api/voice/recordings"));
    assert!(matches("/voice-recordings"));
    assert!(!matches("/api/voice/"));
    assert!(!matches("/api/voice/session-a/startup/extra"));
    assert!(!matches("/api/voice/session-a/confirm"));
    assert!(!matches("/api/voice/recordings/extra"));
  }

  fn startup_body() -> Value {
    json!({
      "run_id": RUN_ID,
      "backend": "gemini",
      "reached": "ready",
      "end": "stop",
      "error": null,
      "mic_open_ms": 180,
      "graph_ready_ms": 95,
      "first_frame_ms": 230,
      "first_sound_ms": 260,
      "end_ms": 5000,
      "frames": 212,
      "samples": 27136,
      "audio_context_state": "running",
      "audio_context_rate": 48000,
      "audio_context_settled_state": "suspended",
      "audio_context_resume": "started",
      "mic_permission": "granted",
      "track_ready_state": "live",
      "track_muted": false,
      "devices": {
        "input_device": "Test Microphone",
        "capture_settings": {"echoCancellation": true, "noiseSuppression": false, "autoGainControl": true, "sampleRate": 16000},
        "output_device": "Test Speakers",
        "communications_output_device": "Test Comms Headset",
        "device_error": null
      },
      "user_agent": "Mozilla/5.0 (Test) Chrome/1",
      "client_ts": "2026-11-04T10:00:00Z"
    })
  }

  fn startup_request(body: &Value) -> Request {
    Request::builder()
      .method("POST")
      .header("Content-Type", "application/json")
      .body(axum::body::Body::from(body.to_string()))
      .expect("build a synthetic startup request")
  }

  async fn post_startup(features: &Arc<FeatureContext>, body: Value) -> (Response, Vec<Event>) {
    let request = startup_request(&body);
    capture_events_async(post_voice_startup(State(Arc::clone(features)), AxumPath(SESSION_ID.to_string()), request))
      .await
  }

  fn startup_lines(logs: &[Event]) -> Vec<&Event> {
    logs.iter().filter(|event| event["event"] == "voice_startup").collect()
  }

  #[tokio::test]
  async fn test_a_startup_report_answers_ok_and_logs_one_line_in_table_order() {
    let (_home, features) = test_features();
    let (response, logs) = post_startup(&features, startup_body()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, r#"{"ok":true}"#);
    let lines = startup_lines(&logs);
    assert_eq!(lines.len(), 1);
    let line = lines[0];
    let names: Vec<&str> = line.keys().map(String::as_str).filter(|name| *name != "event").collect();
    assert_eq!(
      names,
      [
        "session_id",
        "backend",
        "reached",
        "end",
        "error",
        "mic_open_ms",
        "graph_ready_ms",
        "first_frame_ms",
        "first_sound_ms",
        "end_ms",
        "frames",
        "samples",
        "audio_context_state",
        "audio_context_rate",
        "audio_context_settled_state",
        "audio_context_resume",
        "mic_permission",
        "track_ready_state",
        "track_muted",
        "input_device",
        "capture_settings",
        "output_device",
        "communications_output_device",
        "device_error",
        "user_agent",
        "client_ts",
      ]
    );
    assert_eq!(line["session_id"], SESSION_ID);
    assert_eq!(line["backend"], "gemini");
    assert_eq!(line["reached"], "ready");
    assert_eq!(line["end"], "stop");
    assert_eq!(line["error"], "null");
    assert_eq!(line["mic_open_ms"], "180");
    assert_eq!(line["graph_ready_ms"], "95");
    assert_eq!(line["first_frame_ms"], "230");
    assert_eq!(line["first_sound_ms"], "260");
    assert_eq!(line["end_ms"], "5000");
    assert_eq!(line["frames"], "212");
    assert_eq!(line["samples"], "27136");
    assert_eq!(line["audio_context_state"], "running");
    assert_eq!(line["audio_context_rate"], "48000");
    assert_eq!(line["audio_context_settled_state"], "suspended");
    assert_eq!(line["audio_context_resume"], "started");
    assert_eq!(line["mic_permission"], "granted");
    assert_eq!(line["track_ready_state"], "live");
    assert_eq!(line["track_muted"], "false");
    assert_eq!(line["input_device"], "Test Microphone");
    assert_eq!(line["output_device"], "Test Speakers");
    assert_eq!(line["communications_output_device"], "Test Comms Headset");
    assert_eq!(line["device_error"], "null");
    assert_eq!(line["user_agent"], "Mozilla/5.0 (Test) Chrome/1");
    assert_eq!(line["client_ts"], "2026-11-04T10:00:00Z");
  }

  #[tokio::test]
  async fn test_an_absent_optional_and_devices_log_as_null() {
    let (_home, features) = test_features();
    let mut body = startup_body();
    body["reached"] = json!("mic_opening");
    body["end"] = json!("left");
    for field in ["mic_open_ms", "graph_ready_ms", "first_frame_ms", "first_sound_ms"] {
      body[field] = Value::Null;
    }
    body["audio_context_state"] = Value::Null;
    body["audio_context_rate"] = Value::Null;
    body["audio_context_settled_state"] = Value::Null;
    body["audio_context_resume"] = Value::Null;
    body["mic_permission"] = Value::Null;
    body["track_ready_state"] = Value::Null;
    body["track_muted"] = Value::Null;
    body["devices"] = Value::Null;

    let (response, logs) = post_startup(&features, body).await;

    assert_eq!(response.status(), StatusCode::OK);
    let lines = startup_lines(&logs);
    assert_eq!(lines.len(), 1);
    let line = lines[0];
    for field in ["mic_open_ms", "graph_ready_ms", "first_frame_ms", "first_sound_ms"] {
      assert_eq!(line[field], "null", "{field}");
    }
    for field in ["input_device", "capture_settings", "output_device", "communications_output_device", "device_error"] {
      assert_eq!(line[field], "null", "{field}");
    }
    assert_eq!(line["track_muted"], "null");
    assert_eq!(line["reached"], "mic_opening");
    assert_eq!(line["end"], "left");
  }

  #[tokio::test]
  async fn test_an_extra_field_is_a_422_that_logs_nothing() {
    let (_home, features) = test_features();
    let mut body = startup_body();
    body["stale"] = json!(true);

    let (response, logs) = post_startup(&features, body).await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(startup_lines(&logs).is_empty());
  }

  #[tokio::test]
  async fn test_a_wrong_field_type_is_a_422_that_logs_nothing() {
    let (_home, features) = test_features();
    let mut body = startup_body();
    body["backend"] = json!(5);

    let (response, logs) = post_startup(&features, body).await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(response).await.contains("\"loc\":[\"body\",\"backend\"]"));
    assert!(startup_lines(&logs).is_empty());
  }

  #[tokio::test]
  async fn test_a_long_string_field_is_cut_to_300_code_points() {
    let (_home, features) = test_features();
    let mut body = startup_body();
    let long = "é".to_string() + &"𐍈".repeat(174) + &"x".repeat(175);
    assert_eq!(long.chars().count(), 350);
    body["user_agent"] = json!(long);

    let (response, logs) = post_startup(&features, body).await;

    assert_eq!(response.status(), StatusCode::OK);
    let lines = startup_lines(&logs);
    assert_eq!(lines.len(), 1);
    let logged = lines[0]["user_agent"].as_str().expect("the user agent logs as text");
    assert_eq!(logged.chars().count(), MAX_STARTUP_TEXT_CHARS);
    assert_eq!(logged, long.chars().take(MAX_STARTUP_TEXT_CHARS).collect::<String>());
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

  #[tokio::test]
  async fn test_an_upload_transcription_failure_is_recorded_and_the_retry_keeps_one_row() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);
    state.lock().expect("the fake lock is not poisoned").fail = true;

    let (response, logs) = post(&cfg, &credentials, &state, Some(BACKEND_ID), None, 160).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let rows = crate::recordings::list_recordings(&cfg, crate::recordings::DEFAULT_LIST_LIMIT)
      .expect("the list answers over the failed upload");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["failure"]["stage"], "upload");
    assert!(rows[0]["failure"]["error"].as_str().expect("the error text").contains("fake transcription failure"));
    assert_eq!(rows[0]["transcript"], Value::Null);
    assert_eq!(rows[0]["audio_path_kind"], "upload");
    assert!(rows[0]["audio_url"].is_string(), "the failed upload keeps its WAV");
    assert!(transcribed(&logs).is_empty());

    state.lock().expect("the fake lock is not poisoned").fail = false;
    let (response, logs) = post(&cfg, &credentials, &state, Some(BACKEND_ID), None, 160).await;

    assert_eq!(body_text(response).await, r#"{"text":"synthetic remote words"}"#);
    assert_eq!(transcribed(&logs).len(), 1);
    let rows = crate::recordings::list_recordings(&cfg, crate::recordings::DEFAULT_LIST_LIMIT)
      .expect("the list answers over the retried upload");
    assert_eq!(rows.len(), 1, "one run, one row");
    assert_eq!(rows[0]["transcript"], FINAL_TEXT);
    assert_eq!(rows[0]["failure"], Value::Null);
    let files = files_under(&voice_dir(&cfg));
    assert_eq!(files.len(), 3, "one stem holds the wav, the txt and the json: {files:?}");
  }

  #[tokio::test]
  async fn test_an_upload_with_an_invalid_run_id_is_a_400_that_persists_nothing() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let (cfg, credentials) = synthetic_config(&home);
    let (_factory, state) = fake_factory(&cfg);
    let upload = UploadForm {
      audio: wav_body(160),
      run_id: "0A1B2C3D".to_string(),
      backend: Some(BACKEND_ID.to_string()),
      devices: None,
    };

    let (response, logs) = capture_events_async(upload_recording(
      Arc::clone(&cfg),
      Arc::clone(&credentials),
      SESSION_ID.to_string(),
      upload,
      &FakeFactory { state, sessions_dir: cfg.sessions_dir() },
    ))
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("eight lowercase hex"));
    assert!(transcribed(&logs).is_empty());
    assert!(files_under(&cfg.sessions_dir()).is_empty(), "no path was built from the invalid run id");
  }

  #[tokio::test]
  async fn test_a_startup_report_persists_the_runs_startup_part() {
    let (_home, features) = test_features();
    let mut body = startup_body();
    body["reached"] = json!("no_frames");
    body["end"] = json!("startup_error");
    body["error"] = json!("mic: no frames arrived");
    body["mic_permission"] = json!("denied");

    let (response, logs) = post_startup(&features, body).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(startup_lines(&logs).len(), 1, "the voice_startup line stays as it is");
    let cfg = features.config.get().expect("the config loads");
    let rows = crate::recordings::list_recordings(&cfg, crate::recordings::DEFAULT_LIST_LIMIT)
      .expect("the list answers over the startup report");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["reached"], "no_frames");
    assert_eq!(row["end"], "startup_error");
    assert_eq!(row["startup_error"], "mic: no frames arrived");
    assert_eq!(row["mic_permission"], "denied");
    for field in ["audio_url", "duration_s", "digital_silence", "peak_dbfs", "rms_dbfs", "transcript"] {
      assert_eq!(row[field], Value::Null, "{field} of a run without audio");
    }
  }

  #[tokio::test]
  async fn test_a_startup_report_with_an_invalid_run_id_is_a_400_before_any_path_is_built() {
    let (_home, features) = test_features();
    let mut body = startup_body();
    body["run_id"] = json!("zzzzzzzz");

    let (response, logs) = post_startup(&features, body).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(startup_lines(&logs).is_empty());
    let cfg = features.config.get().expect("the config loads");
    assert!(files_under(&cfg.sessions_dir()).is_empty(), "no path was built from the invalid run id");
  }
}

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use charliebot_compat::ws_client::{self, Message, Options, Sender, WEBSOCKETS_16_DEFAULTS};
use charliebot_core::credentials::Credentials;
use charliebot_core::str_methods;
use charliebot_core::text_repr::{self, DEFAULT_JSON};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::audio::{AudioQueue, SAMPLE_RATE};

const GEMINI_MODEL: &str = "models/gemini-3.5-transcribe-live";
const GEMINI_ENDPOINT: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
const MUSE_MODEL: &str = "muse-voice-transcribe-1.0";
const MUSE_ENDPOINT: &str = "wss://api.meta.ai/v1/asr/realtime";
const WS_CLIENT_CLOSE_TIMEOUT: Duration = Duration::from_millis(1_000);
const GEMINI_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const MUSE_LEAD_CAP_SECONDS: f64 = 4.0;
const AUDIO_BYTES_PER_SECOND: f64 = SAMPLE_RATE as f64 * 2.0;

#[derive(Debug)]
pub(crate) enum TranscriptionError {
  Rejected(String),
  Failure(String),
}

impl fmt::Display for TranscriptionError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      TranscriptionError::Rejected(message) | TranscriptionError::Failure(message) => formatter.write_str(message),
    }
  }
}

impl std::error::Error for TranscriptionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranscriptKind {
  Partial,
  Final,
}

pub(crate) struct TranscriptEvent {
  pub(crate) kind: TranscriptKind,
  pub(crate) text: String,
  pub(crate) acknowledged: oneshot::Sender<()>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BackendKind {
  Gemini,
  Muse,
}

/// One transcription backend the feature offers.
///
/// The catalog below is the one home for a backend's id, label, credential, endpoint and preview support; config,
/// selection and relay read it instead of restating a fact.
pub(crate) struct BackendSpec {
  pub(crate) id: &'static str,
  pub(crate) label: &'static str,
  pub(crate) credential: (&'static str, &'static str),
  pub(crate) live_partials: bool,
  kind: BackendKind,
  endpoint: &'static str,
}

pub(crate) const BACKENDS: &[BackendSpec] = &[
  BackendSpec {
    id: "gemini",
    label: "Gemini 3.5 Transcribe Live",
    credential: ("gemini", "api_key"),
    live_partials: true,
    kind: BackendKind::Gemini,
    endpoint: GEMINI_ENDPOINT,
  },
  BackendSpec {
    id: "muse",
    label: "Muse Voice Transcribe",
    credential: ("meta", "model_api_key"),
    live_partials: true,
    kind: BackendKind::Muse,
    endpoint: MUSE_ENDPOINT,
  },
];

#[cfg(test)]
fn spec_of(kind: BackendKind) -> &'static BackendSpec {
  BACKENDS.iter().find(|spec| spec.kind == kind).expect("the catalog holds every backend kind")
}

/// The catalog's ids in order, as the refusal messages list them.
pub(crate) fn known_ids() -> String {
  BACKENDS.iter().map(|spec| spec.id).collect::<Vec<_>>().join(", ")
}

/// The refusal for an id outside the catalog.
pub(crate) fn unknown_backend(id: &str) -> String {
  format!("unknown transcription backend {}; known: {}", text_repr::str_repr(id), known_ids())
}

/// The credential an entry needs, as the config surface and the refusals name it.
pub(crate) fn needed_credential(spec: &BackendSpec) -> String {
  format!("needs {}.{}", spec.credential.0, spec.credential.1)
}

fn missing_credential(spec: &BackendSpec) -> String {
  format!("{}.{} is not set in credentials.yaml", spec.credential.0, spec.credential.1)
}

/// The entry's credential, when the profile's value is truthy: a numeric zero or boolean false counts as unset.
fn credential_token(credentials: &Credentials, spec: &BackendSpec) -> Option<String> {
  credentials.get(spec.credential.0, spec.credential.1).filter(|value| value.is_truthy()).map(|value| value.value_str())
}

pub(crate) struct Backend {
  spec: &'static BackendSpec,
  token: Option<String>,
  endpoint: String,
  clock: Arc<dyn Clock>,
}

impl Backend {
  pub(crate) fn build(id: &str, credentials: &Credentials) -> Result<Self, String> {
    let spec = BACKENDS.iter().find(|spec| spec.id == id).ok_or_else(|| unknown_backend(id))?;
    Ok(Self {
      spec,
      token: credential_token(credentials, spec),
      endpoint: spec.endpoint.to_string(),
      clock: Arc::new(EventLoopClock),
    })
  }

  #[cfg(test)]
  pub(crate) fn gemini_for_test(token: &str, endpoint_url: &str) -> Self {
    Self::for_test(BackendKind::Gemini, token, endpoint_url, Arc::new(EventLoopClock))
  }

  #[cfg(test)]
  fn muse_for_test(token: &str, endpoint_url: &str, clock: Arc<dyn Clock>) -> Self {
    Self::for_test(BackendKind::Muse, token, endpoint_url, clock)
  }

  #[cfg(test)]
  fn for_test(kind: BackendKind, token: &str, endpoint_url: &str, clock: Arc<dyn Clock>) -> Self {
    Self { spec: spec_of(kind), token: Some(token.to_string()), endpoint: endpoint_url.to_string(), clock }
  }

  pub(crate) fn id(&self) -> &'static str {
    self.spec.id
  }

  pub(crate) fn unavailable_reason(&self) -> Option<String> {
    self.token.is_none().then(|| needed_credential(self.spec))
  }

  pub(crate) async fn transcribe(
    &self,
    audio: Arc<AudioQueue>,
    mut cancelled: watch::Receiver<bool>,
    events: mpsc::UnboundedSender<TranscriptEvent>,
    vocabulary: Vec<String>,
    languages: Vec<String>,
  ) -> Result<(), TranscriptionError> {
    match self.spec.kind {
      BackendKind::Gemini => {
        let token = self.token.as_deref().ok_or_else(|| TranscriptionError::Rejected(missing_credential(self.spec)))?;
        let url = format!("{}?key={token}", self.endpoint);
        let options = Options {
          open_timeout: Some(GEMINI_OPEN_TIMEOUT),
          close_timeout: Some(WS_CLIENT_CLOSE_TIMEOUT),
          max_size: None,
          ..WEBSOCKETS_16_DEFAULTS
        };
        let settings = ws_client::Settings::from_env();
        let connected = tokio::select! {
          result = ws_client::connect(&url, &options, &settings) => result,
          _ = wait_cancelled(&mut cancelled) => return Ok(()),
        };
        let (sender, receiver) =
          connected.map_err(|error| TranscriptionError::Failure(error.to_string().replace(token, "[redacted]")))?;
        run_gemini(sender, receiver, audio, &mut cancelled, events, vocabulary, languages).await
      }
      BackendKind::Muse => {
        let token = self.token.as_deref().ok_or_else(|| TranscriptionError::Rejected(missing_credential(self.spec)))?;
        let options = Options { close_timeout: Some(WS_CLIENT_CLOSE_TIMEOUT), ..WEBSOCKETS_16_DEFAULTS };
        let settings = ws_client::Settings::from_env();
        let connected = tokio::select! {
          result = ws_client::connect(&self.endpoint, &options, &settings) => result,
          _ = wait_cancelled(&mut cancelled) => return Ok(()),
        };
        let (sender, receiver) = connected.map_err(|error| TranscriptionError::Failure(error.to_string()))?;
        run_muse(
          sender,
          receiver,
          &mut cancelled,
          MuseSession {
            token: token.to_string(),
            audio,
            events,
            vocabulary,
            languages,
            clock: Arc::clone(&self.clock),
          },
        )
        .await
      }
    }
  }
}

pub(crate) trait Clock: Send + Sync {
  fn monotonic(&self) -> f64;
  fn sleep<'a>(&'a self, seconds: f64) -> BoxFuture<'a, ()>;
}

struct EventLoopClock;

impl Clock for EventLoopClock {
  fn monotonic(&self) -> f64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
  }

  fn sleep<'a>(&'a self, seconds: f64) -> BoxFuture<'a, ()> {
    Box::pin(tokio::time::sleep(Duration::from_secs_f64(seconds)))
  }
}

async fn run_gemini(
  sender: Sender,
  mut receiver: ws_client::Receiver,
  audio: Arc<AudioQueue>,
  cancelled: &mut watch::Receiver<bool>,
  events: mpsc::UnboundedSender<TranscriptEvent>,
  vocabulary: Vec<String>,
  languages: Vec<String>,
) -> Result<(), TranscriptionError> {
  let sender = Arc::new(sender);
  let result =
    run_gemini_session(Arc::clone(&sender), &mut receiver, audio, cancelled, events, vocabulary, languages).await;
  close_sender(&sender, result).await
}

async fn run_gemini_session(
  sender: Arc<Sender>,
  receiver: &mut ws_client::Receiver,
  audio: Arc<AudioQueue>,
  cancelled: &mut watch::Receiver<bool>,
  events: mpsc::UnboundedSender<TranscriptEvent>,
  vocabulary: Vec<String>,
  languages: Vec<String>,
) -> Result<(), TranscriptionError> {
  let setup_send = tokio::select! {
    result = sender.send(Message::Text(dumps_default(&gemini_setup(&vocabulary, &languages)))) => result,
    _ = wait_cancelled(cancelled) => return Ok(()),
  };
  setup_send.map_err(|error| TranscriptionError::Failure(error.to_string()))?;
  let setup_reply = tokio::select! {
    result = tokio::time::timeout(GEMINI_OPEN_TIMEOUT, receiver.recv()) => result
      .map_err(|_| TranscriptionError::Failure("timed out waiting for setup reply".to_string()))?,
    _ = wait_cancelled(cancelled) => return Ok(()),
  };
  let setup_reply = receive_result(setup_reply)?;
  let Some(raw) = setup_reply else {
    return Err(TranscriptionError::Failure("connection closed during setup".to_string()));
  };
  let reply_text = message_text(raw)?;
  let reply: Value = parse_json(&reply_text).map_err(TranscriptionError::Rejected)?;
  if reply.get("setupComplete").is_none() {
    return Err(TranscriptionError::Rejected(format!(
      "setup reply was not setupComplete: {}",
      sorted_object_keys(&reply)
    )));
  }

  let sender_task = tokio::spawn(gemini_send_audio(Arc::clone(&sender), audio, cancelled.clone()));
  let mut sender_task = Some(sender_task);
  let result = loop {
    tokio::select! {
      incoming = receiver.recv() => {
        let incoming = match receive_result(incoming) {
          Ok(incoming) => incoming,
          Err(error) => break Err(error),
        };
        let Some(raw) = incoming else {
          break Err(TranscriptionError::Failure("connection closed before a final transcription".to_string()));
        };
        let raw = match message_text(raw) {
          Ok(raw) => raw,
          Err(error) => break Err(error),
        };
        let message = match parse_json(&raw) {
          Ok(message) => message,
          Err(error) => break Err(TranscriptionError::Failure(error)),
        };
        let content = message.get("serverContent").unwrap_or(&Value::Null);
        if let Some(text) = content.get("interimInputTranscription").and_then(|value| value.get("text")).and_then(Value::as_str) {
          if !emit(&events, TranscriptKind::Partial, normalize_transcript(text.to_string())).await {
            break Ok(());
          }
          continue;
        }
        if let Some(text) = content.get("inputTranscription").and_then(|value| value.get("text")).and_then(Value::as_str) {
          if !emit(&events, TranscriptKind::Final, normalize_transcript(text.to_string())).await {
            break Ok(());
          }
          if let Some(task) = sender_task.take() {
            abort_sender_task(task).await?;
          }
          break Ok(());
        }
      }
      result = async { sender_task.as_mut().expect("sender task is present").await }, if sender_task.is_some() => {
        sender_task = None;
        match result {
          Ok(Ok(())) => {}
          Ok(Err(error)) => break Err(TranscriptionError::Failure(error)),
          Err(error) if error.is_cancelled() => {}
          Err(error) => break Err(TranscriptionError::Failure(error.to_string())),
        }
      }
      _ = wait_cancelled(cancelled) => {
        if let Some(task) = sender_task.take() {
          abort_sender_task(task).await?;
        }
        break Ok(());
      }
    }
  };
  if let Some(task) = sender_task {
    abort_sender_task(task).await?;
  }
  result
}

async fn gemini_send_audio(
  sender: Arc<Sender>,
  audio: Arc<AudioQueue>,
  mut cancelled: watch::Receiver<bool>,
) -> Result<(), String> {
  send_json(&sender, json!({ "realtimeInput": { "activityStart": {} } })).await?;
  while let Some(chunk) = audio_or_cancel(&audio, &mut cancelled).await {
    let frame = json!({
      "realtimeInput": {
        "audio": {
          "data": base64::engine::general_purpose::STANDARD.encode(chunk),
          "mimeType": format!("audio/pcm;rate={SAMPLE_RATE}")
        }
      }
    });
    send_json(&sender, frame).await?;
  }
  if !*cancelled.borrow() {
    send_json(&sender, json!({ "realtimeInput": { "activityEnd": {} } })).await?;
  }
  Ok(())
}

fn gemini_setup(vocabulary: &[String], languages: &[String]) -> Value {
  let language_codes: Vec<&str> = languages
    .iter()
    .filter_map(|code| match code.as_str() {
      "zh" => Some("cmn-Hans-CN"),
      "en" => Some("en-US"),
      _ => None,
    })
    .collect();
  json!({
    "setup": {
      "model": GEMINI_MODEL,
      "generationConfig": { "responseModalities": ["TEXT"] },
      "inputAudioTranscription": {
        "languageCodes": language_codes,
        "customVocabulary": vocabulary,
        "mode": "VERBATIM"
      },
      "realtimeInputConfig": {
        "automaticActivityDetection": { "disabled": true }
      }
    }
  })
}

struct MuseSession {
  token: String,
  audio: Arc<AudioQueue>,
  events: mpsc::UnboundedSender<TranscriptEvent>,
  vocabulary: Vec<String>,
  languages: Vec<String>,
  clock: Arc<dyn Clock>,
}

async fn run_muse(
  sender: Sender,
  receiver: ws_client::Receiver,
  cancelled: &mut watch::Receiver<bool>,
  session: MuseSession,
) -> Result<(), TranscriptionError> {
  let sender = Arc::new(sender);
  let result = run_muse_session(Arc::clone(&sender), receiver, cancelled, session).await;
  close_sender(&sender, result).await
}

async fn run_muse_session(
  sender: Arc<Sender>,
  mut receiver: ws_client::Receiver,
  cancelled: &mut watch::Receiver<bool>,
  session: MuseSession,
) -> Result<(), TranscriptionError> {
  let handshake = muse_handshake(&session.token, &session.vocabulary, &session.languages);
  let handshake_send = tokio::select! {
    result = sender.send(Message::Text(dumps_default(&handshake))) => result,
    _ = wait_cancelled(cancelled) => return Ok(()),
  };
  handshake_send.map_err(|error| TranscriptionError::Failure(error.to_string()))?;
  let raw_handshake = tokio::select! {
    result = receiver.recv() => result,
    _ = wait_cancelled(cancelled) => return Ok(()),
  };
  let handshake_message = receive_result(raw_handshake).map_err(|error| match error {
    TranscriptionError::Failure(message) => {
      TranscriptionError::Rejected(format!("handshake reply is not JSON: {message}"))
    }
    rejected => rejected,
  })?;
  let Some(raw) = handshake_message else {
    return Err(TranscriptionError::Rejected("handshake reply is not JSON: connection closed".to_string()));
  };
  let raw = message_text(raw).map_err(|error| match error {
    TranscriptionError::Failure(message) => {
      TranscriptionError::Rejected(format!("handshake reply is not JSON: {message}"))
    }
    rejected => rejected,
  })?;
  let reply: Value =
    parse_json(&raw).map_err(|error| TranscriptionError::Rejected(format!("handshake reply is not JSON: {error}")))?;
  if reply.get("type").and_then(Value::as_str) == Some("error") || reply.get("error").is_some() {
    let message = match reply.get("message") {
      Some(Value::String(message)) => message.clone(),
      Some(value) => text_repr::value_repr(value),
      None => text_repr::value_repr(&reply),
    };
    return Err(TranscriptionError::Rejected(format!("handshake rejected: {message}")));
  }

  let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<Result<Value, TranscriptionError>>();
  let reader = tokio::spawn(muse_read_events(receiver, inbound_tx));
  let sender_task = tokio::spawn(muse_send_audio(Arc::clone(&sender), session.audio, cancelled.clone(), session.clock));
  let mut reader = Some(reader);
  let mut sender_task = Some(sender_task);
  let mut pending_final: Option<String> = None;
  let result = loop {
    tokio::select! {
      incoming = inbound_rx.recv() => {
        match incoming {
          Some(Ok(message)) => match message.get("type").and_then(Value::as_str) {
            Some("transcript") if message.get("final").and_then(Value::as_bool) == Some(true) => {
              pending_final = Some(message.get("text").and_then(Value::as_str).unwrap_or("").to_string());
            }
            Some("transcript") => {
              let text = message.get("text").and_then(Value::as_str).unwrap_or("").to_string();
              if !emit(&session.events, TranscriptKind::Partial, text).await {
                break Ok(());
              }
            }
            Some("speechComplete") => {
              let text = message.get("transcript").and_then(Value::as_str).filter(|text| !text.is_empty())
                .or(pending_final.as_deref()).unwrap_or("").to_string();
              if !emit(&session.events, TranscriptKind::Final, text).await {
                break Ok(());
              }
              break Ok(());
            }
            _ => {}
          },
          Some(Err(error)) => break Err(error),
          None => {
            if let Some(text) = pending_final.take() {
              if !emit(&session.events, TranscriptKind::Final, text).await {
                break Ok(());
              }
              break Ok(());
            }
            break Err(TranscriptionError::Failure("stream closed without a final transcript".to_string()));
          }
        }
      }
      result = async { sender_task.as_mut().expect("sender task is present").await }, if sender_task.is_some() => {
        sender_task = None;
        match result {
          Ok(Ok(())) => {}
          Ok(Err(error)) => break Err(TranscriptionError::Failure(error)),
          Err(error) if error.is_cancelled() => {}
          Err(error) => break Err(TranscriptionError::Failure(error.to_string())),
        }
      }
      _ = wait_cancelled(cancelled) => break Ok(()),
    }
  };
  if let Some(task) = sender_task {
    abort_sender_task(task).await?;
  }
  if let Some(task) = reader.take() {
    abort_unit_task(task).await?;
  }
  result
}

async fn muse_read_events(
  mut receiver: ws_client::Receiver,
  events: mpsc::UnboundedSender<Result<Value, TranscriptionError>>,
) {
  while let Some(message) = receiver.recv().await {
    match message {
      Ok(Message::Text(raw)) => {
        let parsed = parse_json(&raw).map_err(TranscriptionError::Failure);
        if events.send(parsed).is_err() {
          return;
        }
      }
      Ok(Message::Binary(raw)) => {
        let parsed = String::from_utf8(raw)
          .map_err(|error| TranscriptionError::Failure(error.to_string()))
          .and_then(|raw| parse_json(&raw).map_err(TranscriptionError::Failure));
        if events.send(parsed).is_err() {
          return;
        }
      }
      Ok(Message::Close(frame)) => {
        let result = close_error(frame.as_ref());
        if let Some(error) = result {
          let _ = events.send(Err(error));
        }
        return;
      }
      Err(error) => {
        let result = match &error {
          ws_client::Error::ConnectionClosed { received, .. } => close_error(received.as_ref()),
          _ => Some(TranscriptionError::Failure(error.to_string())),
        };
        if let Some(error) = result {
          let _ = events.send(Err(error));
        }
        return;
      }
    }
  }
}

async fn muse_send_audio(
  sender: Arc<Sender>,
  audio: Arc<AudioQueue>,
  mut cancelled: watch::Receiver<bool>,
  clock: Arc<dyn Clock>,
) -> Result<(), String> {
  let mut session_start = None;
  let mut sent_seconds = 0.0;
  while let Some(chunk) = audio_or_cancel(&audio, &mut cancelled).await {
    let start = *session_start.get_or_insert_with(|| clock.monotonic());
    let target = start + (sent_seconds - MUSE_LEAD_CAP_SECONDS).max(0.0);
    let delay = target - clock.monotonic();
    if delay > 0.0 {
      tokio::select! {
        _ = clock.sleep(delay) => {}
        _ = wait_cancelled(&mut cancelled) => return Ok(()),
      }
    }
    if *cancelled.borrow() {
      return Ok(());
    }
    sender.send(Message::Binary(chunk.clone())).await.map_err(|error| error.to_string())?;
    sent_seconds += chunk.len() as f64 / AUDIO_BYTES_PER_SECOND;
  }
  if !*cancelled.borrow() {
    send_json(&sender, json!({ "type": "endStream" })).await?;
  }
  Ok(())
}

fn muse_handshake(token: &str, vocabulary: &[String], languages: &[String]) -> Value {
  let mut handshake = json!({
    "mode": "PUSH_TO_TALK",
    "authorization": { "accessToken": token },
    "audioEncoding": "PCM_16KHZ",
    "model": MUSE_MODEL,
    "partialMode": "CUMULATIVE",
    "emitAudioProgress": false
  });
  if !vocabulary.is_empty() {
    handshake["keywords"] = json!(vocabulary);
  }
  let language_bias: Vec<&str> = languages
    .iter()
    .filter_map(|language| match language.as_str() {
      "zh" => Some("Mandarin Chinese"),
      "en" => Some("English"),
      _ => None,
    })
    .collect();
  if !language_bias.is_empty() {
    handshake["languageBias"] = json!(language_bias);
  }
  handshake
}

async fn send_json(sender: &Sender, value: Value) -> Result<(), String> {
  sender.send(Message::Text(dumps_default(&value))).await.map_err(|error| error.to_string())
}

fn dumps_default(value: &Value) -> String {
  text_repr::json_dumps(value, &DEFAULT_JSON)
}

fn parse_json(raw: &str) -> Result<Value, String> {
  serde_json::from_str(raw).map_err(|error| error.to_string())
}

fn message_text(message: Message) -> Result<String, TranscriptionError> {
  match message {
    Message::Text(text) => Ok(text),
    Message::Binary(bytes) => String::from_utf8(bytes).map_err(|error| TranscriptionError::Failure(error.to_string())),
    Message::Close(frame) => Err(
      close_error(frame.as_ref())
        .unwrap_or_else(|| TranscriptionError::Failure("connection closed before a final transcription".to_string())),
    ),
  }
}

fn receive_result(incoming: Option<Result<Message, ws_client::Error>>) -> Result<Option<Message>, TranscriptionError> {
  match incoming {
    Some(Ok(Message::Close(frame))) => match close_error(frame.as_ref()) {
      Some(error) => Err(error),
      None => Ok(None),
    },
    Some(Ok(message)) => Ok(Some(message)),
    Some(Err(ws_client::Error::ConnectionClosed { received, .. })) => match close_error(received.as_ref()) {
      Some(error) => Err(error),
      None => Ok(None),
    },
    Some(Err(error)) => Err(TranscriptionError::Failure(error.to_string())),
    None => Ok(None),
  }
}

fn close_error(frame: Option<&ws_client::CloseFrame>) -> Option<TranscriptionError> {
  match frame {
    Some(frame) if matches!(frame.code, 1000 | 1001 | 1005) => None,
    Some(frame) => {
      Some(TranscriptionError::Failure(format!("connection closed mid-stream (code {}: {})", frame.code, frame.reason)))
    }
    None => Some(TranscriptionError::Failure("connection closed mid-stream (code None: )".to_string())),
  }
}

async fn emit(events: &mpsc::UnboundedSender<TranscriptEvent>, kind: TranscriptKind, text: String) -> bool {
  let (acknowledged, wait) = oneshot::channel();
  if events.send(TranscriptEvent { kind, text, acknowledged }).is_err() {
    return false;
  }
  wait.await.is_ok()
}

async fn audio_or_cancel(audio: &AudioQueue, cancelled: &mut watch::Receiver<bool>) -> Option<Vec<u8>> {
  if *cancelled.borrow() {
    return None;
  }
  tokio::select! {
    frame = audio.next_audio() => frame,
    _ = wait_cancelled(cancelled) => None,
  }
}

async fn wait_cancelled(cancelled: &mut watch::Receiver<bool>) {
  if *cancelled.borrow() {
    return;
  }
  let _ = cancelled.changed().await;
}

async fn abort_sender_task(task: JoinHandle<Result<(), String>>) -> Result<(), TranscriptionError> {
  task.abort();
  match task.await {
    Ok(Ok(())) => Ok(()),
    Ok(Err(error)) => Err(TranscriptionError::Failure(error)),
    Err(error) if error.is_cancelled() => Ok(()),
    Err(error) => Err(TranscriptionError::Failure(error.to_string())),
  }
}

async fn abort_unit_task(task: JoinHandle<()>) -> Result<(), TranscriptionError> {
  task.abort();
  match task.await {
    Ok(()) => Ok(()),
    Err(error) if error.is_cancelled() => Ok(()),
    Err(error) => Err(TranscriptionError::Failure(error.to_string())),
  }
}

async fn close_sender(sender: &Sender, result: Result<(), TranscriptionError>) -> Result<(), TranscriptionError> {
  let close = match sender.close().await {
    Ok(()) | Err(ws_client::Error::ConnectionClosed { .. }) => Ok(()),
    Err(ws_client::Error::Protocol(message))
      if message == "WebSocket protocol error: Connection reset without closing handshake" =>
    {
      Ok(())
    }
    Err(error) => Err(TranscriptionError::Failure(error.to_string())),
  };
  match (result, close) {
    (Ok(()), Ok(())) => Ok(()),
    (Err(error), Ok(())) => Err(error),
    (Ok(()), Err(error)) => Err(error),
    (Err(error), Err(close_error)) => {
      tracing::debug!(target: "charliebot_voice", error = %close_error, "voice_preview_close_failed");
      Err(error)
    }
  }
}

fn sorted_object_keys(value: &Value) -> String {
  let mut keys: Vec<&str> =
    value.as_object().map(|object| object.keys().map(String::as_str).collect()).unwrap_or_default();
  keys.sort_unstable();
  format!("[{}]", keys.iter().map(|key| text_repr::str_repr(key)).collect::<Vec<_>>().join(", "))
}

fn is_cjk(char: char) -> bool {
  let code = char as u32;
  [
    (0x3000, 0x303f),
    (0x3040, 0x30ff),
    (0x3400, 0x4dbf),
    (0x4e00, 0x9fff),
    (0xf900, 0xfaff),
    (0xff00, 0xffef),
    (0x20000, 0x2fa1f),
  ]
  .iter()
  .any(|(low, high)| (*low..=*high).contains(&code))
}

fn normalize_transcript(text: String) -> String {
  let source: Vec<char> = text.chars().collect();
  let mut compact = Vec::with_capacity(source.len());
  for (index, char) in source.iter().copied().enumerate() {
    if char == ' '
      && compact.last().is_some_and(|previous| is_cjk(*previous))
      && source.get(index + 1).is_some_and(|next| is_cjk(*next))
    {
      continue;
    }
    compact.push(char);
  }

  let mut before_marks_removed = Vec::with_capacity(compact.len());
  let mut index = 0;
  while index < compact.len() {
    if compact[index] == ' ' {
      let start = index;
      while index < compact.len() && compact[index] == ' ' {
        index += 1;
      }
      if let Some(mark) = compact.get(index).copied() {
        let closes_clause = is_clause_mark(mark)
          && compact.get(index + 1).is_none_or(|next| str_methods::is_whitespace(*next) || !next.is_ascii());
        if closes_clause {
          continue;
        }
      }
      before_marks_removed.extend_from_slice(&compact[start..index]);
      continue;
    }
    before_marks_removed.push(compact[index]);
    index += 1;
  }

  let mut out = Vec::with_capacity(before_marks_removed.len());
  for (index, char) in before_marks_removed.iter().copied().enumerate() {
    if char == ' '
      && index > 0
      && is_ascii_clause_mark(before_marks_removed[index - 1])
      && before_marks_removed.get(index + 1).is_some_and(|next| !next.is_ascii())
    {
      continue;
    }
    out.push(char);
  }
  for index in 0..out.len() {
    if index == 0 || !is_cjk(out[index - 1]) {
      continue;
    }
    out[index] = match out[index] {
      ',' => '，',
      '?' => '？',
      '!' => '！',
      ';' => '；',
      ':' => '：',
      char => char,
    };
  }
  out.into_iter().collect()
}

fn is_clause_mark(char: char) -> bool {
  matches!(char, ',' | '?' | '!' | ';' | ':' | '.' | '、' | '。' | '，' | '？' | '！' | '；' | '：')
}

fn is_ascii_clause_mark(char: char) -> bool {
  matches!(char, ',' | '?' | '!' | ';' | ':' | '.')
}

#[cfg(test)]
mod tests {
  use super::*;
  use futures_util::{SinkExt, StreamExt};
  use std::sync::Mutex as StdMutex;
  use tokio::net::{TcpListener, TcpStream};
  use tokio_tungstenite::tungstenite::Message as ServerMessage;
  use tokio_tungstenite::tungstenite::protocol::CloseFrame as ServerCloseFrame;
  use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
  use tokio_tungstenite::{WebSocketStream, accept_async};

  type FakeSocket = WebSocketStream<TcpStream>;

  struct VirtualClock {
    state: StdMutex<(f64, Vec<f64>)>,
  }

  impl VirtualClock {
    fn new() -> Self {
      Self { state: StdMutex::new((0.0, Vec::new())) }
    }

    fn sleeps(&self) -> Vec<f64> {
      self.state.lock().expect("virtual clock lock").1.clone()
    }
  }

  impl Clock for VirtualClock {
    fn monotonic(&self) -> f64 {
      self.state.lock().expect("virtual clock lock").0
    }

    fn sleep<'a>(&'a self, seconds: f64) -> BoxFuture<'a, ()> {
      Box::pin(async move {
        let mut state = self.state.lock().expect("virtual clock lock");
        state.1.push(seconds);
        state.0 += seconds;
      })
    }
  }

  async fn fake_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a loopback fake WebSocket server");
    let endpoint = format!("ws://{}", listener.local_addr().expect("read the fake server address"));
    (listener, endpoint)
  }

  async fn accept_fake(listener: TcpListener) -> FakeSocket {
    let (stream, _) = listener.accept().await.expect("accept the backend connection");
    accept_async(stream).await.expect("complete the fake WebSocket handshake")
  }

  async fn next_text(socket: &mut FakeSocket) -> String {
    match socket.next().await.expect("backend sends the expected frame").expect("read backend frame") {
      ServerMessage::Text(text) => text.to_string(),
      message => panic!("expected a text frame, got {message:?}"),
    }
  }

  fn start_transcription(
    backend: Backend,
    audio: Arc<AudioQueue>,
  ) -> (JoinHandle<Result<(), String>>, mpsc::UnboundedReceiver<TranscriptEvent>, watch::Sender<bool>) {
    let (cancel, cancelled) = watch::channel(false);
    let (events, receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
      backend
        .transcribe(audio, cancelled, events, vec!["Ada".to_string()], vec!["zh".to_string(), "fr".to_string()])
        .await
        .map_err(|error| error.to_string())
    });
    (task, receiver, cancel)
  }

  async fn next_event(receiver: &mut mpsc::UnboundedReceiver<TranscriptEvent>) -> TranscriptEvent {
    tokio::time::timeout(Duration::from_secs(3), receiver.recv())
      .await
      .expect("backend emits a transcript event")
      .expect("transcript event channel remains open")
  }

  #[test]
  fn normalization_matches_unicode_whitespace_rules() {
    let cases = [
      ("请 帮 我 查 一 下", "请帮我查一下"),
      ("等 一 下 ok ?", "等一下 ok?"),
      ("好 了  ?", "好了？"),
      ("第 一 , 下 一 个", "第一，下一个"),
      ("ok ， 好", "ok，好"),
      ("看 一 下 cd ./dir", "看一下 cd ./dir"),
      ("ok , 然后", "ok,然后"),
      ("第 一 行 . 结 束", "第一行.结束"),
      ("ok, jabra", "ok, jabra"),
      ("好 了 吗 ?", "好了吗？"),
      ("用 冒 号 : 分 隔", "用冒号：分隔"),
      ("值 是 1 : 2", "值是 1: 2"),
      ("下 一 行 .", "下一行."),
      ("cron job 的 session", "cron job 的 session"),
      ("x\u{1c} ,好", "x\u{1c},好"),
    ];
    for (input, expected) in cases {
      assert_eq!(normalize_transcript(input.to_string()), expected, "{input:?}");
    }
  }

  #[test]
  fn wire_json_uses_default_escaping_and_spacing() {
    assert_eq!(dumps_default(&json!({ "text": "语音" })), r#"{"text": "\u8bed\u97f3"}"#);
  }

  #[test]
  fn normalizer_matches_every_gemini_case() {
    let cases = [
      ("请 帮 我 查 一 下", "请帮我查一下"),
      ("等 一 下 ok ?", "等一下 ok?"),
      ("好 了  ?", "好了？"),
      ("第 一 , 下 一 个", "第一，下一个"),
      ("ok ， 好", "ok，好"),
      ("看 一 下 cd ./dir", "看一下 cd ./dir"),
      ("ok , 然后", "ok,然后"),
      ("第 一 行 . 结 束", "第一行.结束"),
      ("ok, jabra", "ok, jabra"),
      ("好 了 吗 ?", "好了吗？"),
      ("用 冒 号 : 分 隔", "用冒号：分隔"),
      ("值 是 1 : 2", "值是 1: 2"),
      ("下 一 行 .", "下一行."),
      ("cron job 的 session", "cron job 的 session"),
      ("今天不错，明天再见！", "今天不错，明天再见！"),
      ("跑 3.5 公里", "跑 3.5 公里"),
      ("x\u{1c} ,好", "x\u{1c},好"),
      ("", ""),
    ];
    for (input, expected) in cases {
      assert_eq!(normalize_transcript(input.to_string()), expected, "{input:?}");
    }
  }

  #[tokio::test]
  async fn gemini_loopback_preserves_setup_audio_partial_and_final_frames() {
    let (listener, endpoint) = fake_listener().await;
    let fake = tokio::spawn(async move {
      let mut socket = accept_fake(listener).await;
      let mut sent = vec![next_text(&mut socket).await];
      socket.send(ServerMessage::Text(r#"{"setupComplete":{}}"#.into())).await.expect("send the setup reply");
      loop {
        let Some(message) = socket.next().await else {
          break;
        };
        match message.expect("read Gemini frame") {
          ServerMessage::Text(text) => {
            let text = text.to_string();
            let is_end = serde_json::from_str::<Value>(&text)
              .expect("Gemini sends JSON")
              .pointer("/realtimeInput/activityEnd")
              .is_some();
            sent.push(text);
            if is_end {
              socket
                .send(ServerMessage::Text(
                  r#"{"serverContent":{"interimInputTranscription":{"text":"请 查 一 下"}}}"#.into(),
                ))
                .await
                .expect("send the partial");
              socket
                .send(ServerMessage::Text(r#"{"serverContent":{"inputTranscription":{"text":"任务完成"}}}"#.into()))
                .await
                .expect("send the final");
            }
          }
          ServerMessage::Close(_) => {
            socket.flush().await.expect("flush the Gemini close reply");
            break;
          }
          message => panic!("unexpected Gemini frame: {message:?}"),
        }
      }
      sent
    });

    let audio = AudioQueue::new();
    assert!(audio.put(vec![1, 2]).await);
    audio.end(None).await;
    let backend = Backend::gemini_for_test("synthetic-key", &endpoint);
    let (task, mut events, _cancel) = start_transcription(backend, audio);
    let mut transcript = Vec::new();
    for _ in 0..2 {
      let event = next_event(&mut events).await;
      transcript.push((event.kind, event.text.clone()));
      event.acknowledged.send(()).expect("relay acknowledges the event");
    }
    assert_eq!(
      transcript,
      [(TranscriptKind::Partial, "请查一下".to_string()), (TranscriptKind::Final, "任务完成".to_string()),]
    );
    tokio::time::timeout(Duration::from_secs(3), task)
      .await
      .expect("Gemini session completes")
      .expect("Gemini task succeeds")
      .expect("Gemini transcription succeeds");
    let sent = tokio::time::timeout(Duration::from_secs(3), fake)
      .await
      .expect("fake Gemini server completes")
      .expect("fake Gemini task succeeds");
    assert_eq!(
      sent,
      [
        r#"{"setup": {"model": "models/gemini-3.5-transcribe-live", "generationConfig": {"responseModalities": ["TEXT"]}, "inputAudioTranscription": {"languageCodes": ["cmn-Hans-CN"], "customVocabulary": ["Ada"], "mode": "VERBATIM"}, "realtimeInputConfig": {"automaticActivityDetection": {"disabled": true}}}}"#,
        r#"{"realtimeInput": {"activityStart": {}}}"#,
        r#"{"realtimeInput": {"audio": {"data": "AQI=", "mimeType": "audio/pcm;rate=16000"}}}"#,
        r#"{"realtimeInput": {"activityEnd": {}}}"#,
      ]
    );
  }

  #[tokio::test]
  async fn muse_loopback_streams_partial_and_final_with_a_four_second_lead_cap() {
    let (listener, endpoint) = fake_listener().await;
    let chunks = vec![vec![1; 96_000], vec![2; 96_000], vec![3; 96_000]];
    let fake_chunks = chunks.clone();
    let fake = tokio::spawn(async move {
      let mut socket = accept_fake(listener).await;
      let handshake = next_text(&mut socket).await;
      socket
        .send(ServerMessage::Text(r#"{"type":"sessionStarted"}"#.into()))
        .await
        .expect("send the Muse session reply");
      let mut binary = Vec::new();
      let mut sent = vec![handshake];
      loop {
        let Some(message) = socket.next().await else {
          break;
        };
        match message.expect("read Muse frame") {
          ServerMessage::Binary(bytes) => binary.push(bytes.to_vec()),
          ServerMessage::Text(text) => {
            let text = text.to_string();
            let is_end = text == r#"{"type": "endStream"}"#;
            sent.push(text);
            if is_end {
              socket
                .send(ServerMessage::Text(r#"{"type":"transcript","text":"synthetic partial"}"#.into()))
                .await
                .expect("send the Muse partial");
              socket
                .send(ServerMessage::Text(r#"{"type":"speechComplete","transcript":"synthetic final"}"#.into()))
                .await
                .expect("send the Muse final");
            }
          }
          ServerMessage::Close(_) => {
            socket.flush().await.expect("flush the Muse close reply");
            break;
          }
          message => panic!("unexpected Muse frame: {message:?}"),
        }
      }
      (sent, binary, fake_chunks)
    });

    let clock = Arc::new(VirtualClock::new());
    let audio = AudioQueue::new();
    for chunk in &chunks {
      assert!(audio.put(chunk.clone()).await);
    }
    audio.end(None).await;
    let backend = Backend::muse_for_test("synthetic-token", &endpoint, clock.clone());
    let (task, mut events, _cancel) = start_transcription(backend, audio);
    let mut transcript = Vec::new();
    for _ in 0..2 {
      let event = next_event(&mut events).await;
      transcript.push((event.kind, event.text.clone()));
      event.acknowledged.send(()).expect("relay acknowledges the event");
    }
    assert_eq!(
      transcript,
      [
        (TranscriptKind::Partial, "synthetic partial".to_string()),
        (TranscriptKind::Final, "synthetic final".to_string()),
      ]
    );
    tokio::time::timeout(Duration::from_secs(3), task)
      .await
      .expect("Muse session completes")
      .expect("Muse task succeeds")
      .expect("Muse transcription succeeds");
    let (sent, binary, expected_binary) = tokio::time::timeout(Duration::from_secs(3), fake)
      .await
      .expect("fake Muse server completes")
      .expect("fake Muse task succeeds");
    assert_eq!(
      sent[0],
      r#"{"mode": "PUSH_TO_TALK", "authorization": {"accessToken": "synthetic-token"}, "audioEncoding": "PCM_16KHZ", "model": "muse-voice-transcribe-1.0", "partialMode": "CUMULATIVE", "emitAudioProgress": false, "keywords": ["Ada"], "languageBias": ["Mandarin Chinese"]}"#
    );
    assert_eq!(sent[1], r#"{"type": "endStream"}"#);
    assert_eq!(binary, expected_binary);
    assert_eq!(clock.sleeps(), [2.0]);
  }

  #[tokio::test]
  async fn muse_abnormal_close_uses_mid_stream_text() {
    let (listener, endpoint) = fake_listener().await;
    let fake = tokio::spawn(async move {
      let mut socket = accept_fake(listener).await;
      let _ = next_text(&mut socket).await;
      socket
        .send(ServerMessage::Text(r#"{"type":"sessionStarted"}"#.into()))
        .await
        .expect("send the Muse session reply");
      socket
        .send(ServerMessage::Close(Some(ServerCloseFrame {
          code: CloseCode::from(1008),
          reason: "synthetic refusal".into(),
        })))
        .await
        .expect("send the abnormal close");
    });
    let backend = Backend::muse_for_test("synthetic-token", &endpoint, Arc::new(EventLoopClock));
    let (task, _events, _cancel) = start_transcription(backend, AudioQueue::new());
    assert_eq!(
      tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("Muse close is handled")
        .expect("Muse task completes")
        .unwrap_err(),
      "connection closed mid-stream (code 1008: synthetic refusal)"
    );
    fake.abort();
  }

  #[tokio::test]
  async fn gemini_clean_close_without_final_has_text() {
    let (listener, endpoint) = fake_listener().await;
    let fake = tokio::spawn(async move {
      let mut socket = accept_fake(listener).await;
      let _ = next_text(&mut socket).await;
      socket.send(ServerMessage::Text(r#"{"setupComplete":{}}"#.into())).await.expect("send the setup reply");
      socket
        .send(ServerMessage::Close(Some(ServerCloseFrame { code: CloseCode::Normal, reason: String::new().into() })))
        .await
        .expect("send the clean close");
    });
    let backend = Backend::gemini_for_test("synthetic-key", &endpoint);
    let (task, _events, _cancel) = start_transcription(backend, AudioQueue::new());
    assert_eq!(
      tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("Gemini close is handled")
        .expect("Gemini task completes")
        .unwrap_err(),
      "connection closed before a final transcription"
    );
    fake.abort();
  }

  #[tokio::test]
  async fn both_backend_cancellations_wait_for_the_one_second_close_timeout() {
    for backend_kind in [BackendKind::Gemini, BackendKind::Muse] {
      let (listener, endpoint) = fake_listener().await;
      let (ready, wait_until_open) = tokio::sync::oneshot::channel();
      let fake = tokio::spawn(async move {
        let mut socket = accept_fake(listener).await;
        let _ = next_text(&mut socket).await;
        let reply = match backend_kind {
          BackendKind::Gemini => r#"{"setupComplete":{}}"#,
          BackendKind::Muse => r#"{"type":"sessionStarted"}"#,
        };
        socket.send(ServerMessage::Text(reply.into())).await.expect("send the session reply");
        if matches!(backend_kind, BackendKind::Gemini) {
          let activity_start = next_text(&mut socket).await;
          assert!(activity_start.contains("activityStart"));
        }
        let audio = socket.next().await.expect("client sends audio").expect("audio frame is valid");
        assert!(matches!(audio, ServerMessage::Text(_) | ServerMessage::Binary(_)));
        ready.send(()).expect("notify the test that the session is open");
        std::future::pending::<()>().await;
      });
      let backend = match backend_kind {
        BackendKind::Gemini => Backend::gemini_for_test("synthetic-key", &endpoint),
        BackendKind::Muse => Backend::muse_for_test("synthetic-token", &endpoint, Arc::new(EventLoopClock)),
      };
      let audio = AudioQueue::new();
      assert!(audio.put(vec![0, 0]).await);
      let (task, _events, cancel) = start_transcription(backend, audio);
      tokio::time::timeout(Duration::from_secs(3), wait_until_open)
        .await
        .expect("backend connection opens")
        .expect("fake server reports open");
      let started = tokio::time::Instant::now();
      cancel.send_replace(true);
      tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("backend cancellation finishes")
        .expect("backend task completes")
        .expect("cancelled backend returns cleanly");
      let elapsed = started.elapsed();
      assert!(elapsed >= Duration::from_millis(800), "close wait was only {elapsed:?}");
      assert!(elapsed < Duration::from_secs(2), "close wait exceeded its one-second limit: {elapsed:?}");
      fake.abort();
    }
  }
}

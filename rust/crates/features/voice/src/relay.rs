use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::response::Response;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::text_repr;
use charliebot_registry::{FeatureContext, WebSocketRequest};
use futures_util::future::BoxFuture;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;

use crate::api::{DEVICE_FIELDS, Transcribed, log_voice_transcribed, value_repr};
use crate::archive;
use crate::audio::{AudioQueue, PREVIEW_QUEUE_SECONDS};
use crate::config::VoiceSettings;
use crate::transcription::{self, Backend, TranscriptEvent, TranscriptKind};

type SocketWriter = Arc<Mutex<SplitSink<WebSocket, Message>>>;

struct RelaySettings {
  config: Arc<CharlieBotConfig>,
  vocabulary: Vec<String>,
  languages: Vec<String>,
  session_id: String,
}

pub(crate) fn handler(features: Arc<FeatureContext>, request: WebSocketRequest) -> BoxFuture<'static, Response> {
  Box::pin(async move {
    let session_id = request.segments.first().expect("the route captures its session id").clone();
    let backend_id = query_value(&request.query, "backend").unwrap_or_default();
    let backend = match features.credentials.get() {
      Ok(credentials) => Backend::build(&backend_id, &credentials),
      Err(error) => Err(error.to_string()),
    };
    let refusal = match &backend {
      Err(error) => Some(error.clone()),
      Ok(backend) => backend
        .unavailable_reason()
        .map(|reason| format!("voice backend {} is unavailable: {reason}", text_repr::str_repr(backend.id()))),
    };
    let config = features.config.get();
    request.upgrade.max_message_size(16_777_216).max_frame_size(16_777_216).on_upgrade(move |socket| async move {
      let (writer, reader) = socket.split();
      let writer = Arc::new(Mutex::new(writer));
      if let Some(message) = refusal {
        refuse_preview(&writer, &message).await;
        return;
      }
      let backend = backend.expect("a backend without a refusal is built");
      let config = match config {
        Ok(config) => config,
        Err(error) => {
          let message = error.to_string();
          let _ = send_frame(&writer, json!({ "type": "error", "message": message })).await;
          close_socket(&writer).await;
          return;
        }
      };
      let settings = config.section::<VoiceSettings>("voice");
      let vocabulary = settings.vocabulary.clone();
      let languages = settings.languages.clone();
      relay_preview(writer, reader, RelaySettings { config, vocabulary, languages, session_id }, backend).await;
    })
  })
}

async fn relay_preview(
  writer: SocketWriter,
  reader: SplitStream<WebSocket>,
  settings: RelaySettings,
  backend: Backend,
) {
  relay_preview_with_queue(writer, reader, settings, backend, AudioQueue::new()).await;
}

async fn relay_preview_with_queue(
  writer: SocketWriter,
  mut reader: SplitStream<WebSocket>,
  settings: RelaySettings,
  backend: Backend,
  queue: Arc<AudioQueue>,
) {
  let RelaySettings { config, vocabulary, languages, session_id } = settings;
  let (cancel, _) = watch::channel(false);
  let (events, event_receiver) = mpsc::unbounded_channel::<TranscriptEvent>();
  let backend_id = backend.id().to_string();
  let mut streamer = Some(tokio::spawn(stream_preview(StreamContext {
    writer: Arc::clone(&writer),
    queue: Arc::clone(&queue),
    config: Arc::clone(&config),
    session_id: session_id.clone(),
    backend_id: backend_id.clone(),
    backend,
    cancel: cancel.clone(),
    events,
    event_receiver,
    vocabulary,
    languages,
  })));

  let outcome = loop {
    match reader.next().await {
      Some(Ok(Message::Binary(bytes))) => {
        if !queue.put(bytes.to_vec()).await {
          break ReceiveOutcome::Error(format!(
            "voice preview buffer overflowed (over {PREVIEW_QUEUE_SECONDS}s of audio unconsumed)"
          ));
        }
      }
      Some(Ok(Message::Text(text))) => match preview_end_devices(text.as_str()) {
        Ok(devices) => {
          queue.end(devices).await;
          break ReceiveOutcome::Ended;
        }
        Err(message) => break ReceiveOutcome::Error(message),
      },
      Some(Ok(Message::Close(_))) | None => break ReceiveOutcome::Disconnect,
      Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
      Some(Err(_)) => break ReceiveOutcome::Disconnect,
    }
  };

  match outcome {
    ReceiveOutcome::Disconnect => {
      cancel.send_replace(true);
      await_streamer(streamer.take()).await;
    }
    ReceiveOutcome::Error(message) => {
      cancel.send_replace(true);
      await_streamer(streamer.take()).await;
      let _ = send_frame(&writer, json!({ "type": "error", "message": message })).await;
      drain_socket(&mut reader).await;
      close_socket(&writer).await;
    }
    ReceiveOutcome::Ended => {
      settle_sealed_socket(&writer, &mut reader, &queue, &cancel, &mut streamer, &session_id, backend_id).await;
      close_socket(&writer).await;
    }
  }
}

enum ReceiveOutcome {
  Ended,
  Error(String),
  Disconnect,
}

struct StreamContext {
  writer: SocketWriter,
  queue: Arc<AudioQueue>,
  config: Arc<CharlieBotConfig>,
  session_id: String,
  backend_id: String,
  backend: Backend,
  cancel: watch::Sender<bool>,
  events: mpsc::UnboundedSender<TranscriptEvent>,
  event_receiver: mpsc::UnboundedReceiver<TranscriptEvent>,
  vocabulary: Vec<String>,
  languages: Vec<String>,
}

async fn stream_preview(context: StreamContext) {
  let StreamContext {
    writer,
    queue,
    config,
    session_id,
    backend_id,
    backend,
    cancel,
    events,
    mut event_receiver,
    vocabulary,
    languages,
  } = context;
  let mut cancellation = cancel.subscribe();
  let worker_queue = Arc::clone(&queue);
  let worker_cancellation = cancellation.clone();
  let mut worker = Some(tokio::spawn(async move {
    backend.transcribe(worker_queue, worker_cancellation, events, vocabulary, languages).await
  }));
  let mut worker_result = None;
  loop {
    tokio::select! {
      event = event_receiver.recv() => {
        let Some(event) = event else {
          break;
        };
        let sent = match event.kind {
          TranscriptKind::Partial => send_frame(&writer, json!({ "type": "partial", "text": event.text })).await,
          TranscriptKind::Final => {
            if !queue.ended().await {
              let reason = "backend produced its final before the recording's end frame";
              tracing::warn!(target: "charliebot_voice", session_id, backend = %backend_id, reason, "voice_preview_failed");
              let sent = send_frame(&writer, json!({ "type": "error", "message": reason })).await;
              let _ = event.acknowledged.send(());
              cancel.send_replace(true);
              await_backend_worker(worker.take().expect("backend worker is present"), &session_id, &backend_id).await;
              if let Err(error) = sent {
                log_socket_lost(&session_id, &backend_id, &error);
              }
              return;
            }
            archive_final(&writer, &queue, &config, &session_id, &backend_id, &event).await
          }
        };
          let _ = event.acknowledged.send(());
        if let Err(error) = sent {
          log_socket_lost(&session_id, &backend_id, &error);
          cancel.send_replace(true);
          await_backend_worker(worker.take().expect("backend worker is present"), &session_id, &backend_id).await;
          return;
        }
      }
      result = worker.as_mut().expect("backend worker is present") => {
        worker_result = Some(result);
        worker = None;
        break;
      }
      _ = cancellation.changed() => {
        if *cancellation.borrow() {
          worker_result = Some(worker.take().expect("backend worker is present").await);
          break;
        }
      }
    }
  }
  if worker_result.is_none() {
    worker_result = Some(worker.take().expect("backend worker is present").await);
  }
  match worker_result.expect("the worker result is collected") {
    Ok(Ok(())) => {}
    Ok(Err(error)) => {
      if !*cancellation.borrow() {
        let reason = error.to_string();
        tracing::warn!(target: "charliebot_voice", session_id, backend = %backend_id, reason = %reason, "voice_preview_failed");
        if let Err(error) = send_frame(&writer, json!({ "type": "error", "message": reason })).await {
          log_socket_lost(&session_id, &backend_id, &error);
        }
      }
    }
    Err(error) if error.is_cancelled() && *cancellation.borrow() => {}
    Err(error) => {
      let reason = error.to_string();
      tracing::warn!(target: "charliebot_voice", session_id, backend = %backend_id, reason = %reason, "voice_preview_failed");
      if let Err(error) = send_frame(&writer, json!({ "type": "error", "message": reason })).await {
        log_socket_lost(&session_id, &backend_id, &error);
      }
    }
  }
}

async fn archive_final(
  writer: &SocketWriter,
  queue: &AudioQueue,
  config: &Arc<CharlieBotConfig>,
  session_id: &str,
  backend_id: &str,
  event: &TranscriptEvent,
) -> Result<(), String> {
  let pcm_bytes = queue.recording_pcm().await;
  let audio_bytes_size = pcm_bytes.len();
  let text = event.text.clone();
  let cfg = Arc::clone(config);
  let session_id = session_id.to_string();
  let archive_session_id = session_id.clone();
  let archive =
    tokio::task::spawn_blocking(move || archive::archive_voice_pair(&cfg, &archive_session_id, &pcm_bytes, &text))
      .await;
  let audio_path = match archive {
    Ok(Ok(audio_path)) => audio_path,
    Ok(Err(_error)) => {
      tracing::error!(target: "charliebot_voice", session_id, backend = %backend_id, "voice_preview_archive_failed");
      let _ = send_frame(writer, json!({ "type": "error", "message": "recording archive failed" })).await;
      return Ok(());
    }
    Err(_error) => {
      tracing::error!(target: "charliebot_voice", session_id, backend = %backend_id, "voice_preview_archive_failed");
      let _ = send_frame(writer, json!({ "type": "error", "message": "recording archive failed" })).await;
      return Ok(());
    }
  };
  let end_to_final_s = queue.seconds_since_end().await;
  let devices = queue.devices().await;
  log_voice_transcribed(&Transcribed {
    session_id: &session_id,
    audio_path: &audio_path,
    audio_bytes_size,
    text: &event.text,
    produced_by: backend_id,
    selected_backend: Some(backend_id),
    devices: devices.as_ref(),
    end_to_final_s: Some(end_to_final_s),
  });
  send_frame(writer, json!({ "type": "final", "text": event.text })).await
}

async fn settle_sealed_socket(
  writer: &SocketWriter,
  reader: &mut SplitStream<WebSocket>,
  queue: &AudioQueue,
  cancel: &watch::Sender<bool>,
  streamer: &mut Option<JoinHandle<()>>,
  session_id: &str,
  backend_id: String,
) {
  loop {
    let Some(streamer_task) = streamer.as_mut() else {
      return;
    };
    tokio::select! {
      biased;
      result = streamer_task => {
        if let Err(error) = result {
          tracing::error!(target: "charliebot_voice", error = %error, "voice_preview_streamer_failed");
        }
        *streamer = None;
        return;
      }
      incoming = reader.next() => {
        match incoming {
          Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
            cancel.send_replace(true);
            await_streamer(streamer.take()).await;
            log_abandoned(queue, session_id, &backend_id).await;
            return;
          }
          Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
          Some(Ok(Message::Binary(_) | Message::Text(_))) => {
            cancel.send_replace(true);
            await_streamer(streamer.take()).await;
            let _ = send_frame(writer, json!({
              "type": "error",
              "message": "unexpected frame after the recording's end"
            })).await;
            drain_socket(reader).await;
            return;
          }
        }
      }
    }
  }
}

async fn log_abandoned(queue: &AudioQueue, session_id: &str, backend_id: &str) {
  let recording_s = queue.recording_seconds().await;
  let waited_s = queue.seconds_since_end().await;
  tracing::info!(
    target: "charliebot_voice",
    session_id,
    backend = %backend_id,
    recording_s,
    waited_s,
    "voice_preview_abandoned"
  );
}

async fn await_streamer(streamer: Option<JoinHandle<()>>) {
  if let Some(streamer) = streamer
    && let Err(error) = streamer.await
    && !error.is_cancelled()
  {
    tracing::error!(target: "charliebot_voice", error = %error, "voice_preview_streamer_failed");
  }
}

async fn await_backend_worker(
  worker: JoinHandle<Result<(), transcription::TranscriptionError>>,
  session_id: &str,
  backend_id: &str,
) {
  match worker.await {
    Ok(Ok(())) => {}
    Ok(Err(error)) => {
      tracing::debug!(target: "charliebot_voice", session_id, backend = %backend_id, error = %error, "voice_preview_cancelled_backend_failed")
    }
    Err(error) if error.is_cancelled() => {}
    Err(error) => {
      tracing::error!(target: "charliebot_voice", session_id, backend = %backend_id, error = %error, "voice_preview_backend_task_failed")
    }
  }
}

async fn send_frame(writer: &SocketWriter, payload: Value) -> Result<(), String> {
  let text =
    String::from_utf8(charliebot_compat::json::starlette_bytes(&payload)).map_err(|error| error.to_string())?;
  let mut writer = writer.lock().await;
  writer.send(Message::Text(text.into())).await.map_err(|error| error.to_string())
}

async fn refuse_preview(writer: &SocketWriter, message: &str) {
  if let Err(error) = send_frame(writer, json!({ "type": "error", "message": message })).await {
    tracing::debug!(target: "charliebot_voice", error = %error, "voice_preview_refusal_send_failed");
  }
  close_socket(writer).await;
}

async fn close_socket(writer: &SocketWriter) {
  let close = Message::Close(Some(CloseFrame { code: 1000, reason: String::new().into() }));
  if let Err(error) = writer.lock().await.send(close).await {
    tracing::debug!(target: "charliebot_voice", error = %error, "voice_preview_close_failed");
  }
}

async fn drain_socket(reader: &mut SplitStream<WebSocket>) {
  while let Some(message) = reader.next().await {
    match message {
      Ok(Message::Close(_)) | Err(_) => return,
      Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Text(_)) => {}
    }
  }
}

fn preview_end_devices(text: &str) -> Result<Option<Value>, String> {
  let control: Value =
    serde_json::from_str(text).map_err(|_| format!("malformed preview control frame {}", text_repr::str_repr(text)))?;
  let Some(control) = control.as_object() else {
    return Err(format!("unsupported preview control frame {}", text_repr::str_repr(text)));
  };
  if control.get("type").and_then(Value::as_str) != Some("end") {
    return Err(format!("unsupported preview control frame {}", text_repr::str_repr(text)));
  }
  let Some(devices) = control.get("devices") else {
    return Ok(None);
  };
  let Some(devices) = devices.as_object() else {
    return Err(format!(
      "end frame devices must carry exactly {}: {}",
      value_repr(&json!(DEVICE_FIELDS)),
      value_repr(devices)
    ));
  };
  let fields_match = devices.len() == DEVICE_FIELDS.len() && DEVICE_FIELDS.iter().all(|key| devices.contains_key(*key));
  if !fields_match {
    return Err(format!(
      "end frame devices must carry exactly {}: {}",
      value_repr(&json!(DEVICE_FIELDS)),
      value_repr(&Value::Object(devices.clone()))
    ));
  }
  Ok(Some(Value::Object(devices.clone())))
}

fn query_value(query: &str, target: &str) -> Option<String> {
  query
    .split('&')
    .filter_map(|pair| {
      let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
      let key = key.replace('+', " ");
      let key = percent_encoding::percent_decode_str(&key).decode_utf8_lossy();
      (key == target).then(|| {
        let value = value.replace('+', " ");
        percent_encoding::percent_decode_str(&value).decode_utf8_lossy().into_owned()
      })
    })
    .next_back()
}

fn log_socket_lost(session_id: &str, backend_id: &str, reason: &str) {
  tracing::warn!(target: "charliebot_voice", session_id, backend = %backend_id, reason, "voice_preview_socket_lost");
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  use axum::extract::State;
  use axum::routing::get;
  use axum::{Router, extract::ws::WebSocketUpgrade};
  use futures_util::{SinkExt, StreamExt};
  use tokio::net::TcpListener;
  use tokio::sync::oneshot;
  use tokio_tungstenite::tungstenite::Message as ClientMessage;
  use tokio_tungstenite::tungstenite::Message as ServerMessage;
  use tokio_tungstenite::{WebSocketStream, accept_async, connect_async};

  use crate::audio::MAX_RECORDING_SAMPLES;

  struct RelayTestState {
    config: Arc<CharlieBotConfig>,
    endpoint_url: String,
    max_recording_samples: usize,
  }

  struct RelayTestServer {
    address: std::net::SocketAddr,
    config: Arc<CharlieBotConfig>,
    _home: tempfile::TempDir,
    server: JoinHandle<()>,
    fake_backend: JoinHandle<()>,
    provider_connected: oneshot::Receiver<()>,
  }

  #[derive(Clone, Copy)]
  enum FakeMode {
    FinalAfterEnd,
    FinalBeforeEnd,
    NeverFinal,
  }

  async fn relay_test_handler(State(state): State<Arc<RelayTestState>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.max_message_size(16_777_216).max_frame_size(16_777_216).on_upgrade(move |socket| async move {
      let (writer, reader) = socket.split();
      relay_preview_with_queue(
        Arc::new(Mutex::new(writer)),
        reader,
        RelaySettings {
          config: Arc::clone(&state.config),
          vocabulary: Vec::new(),
          languages: Vec::new(),
          session_id: "synthetic-session".to_string(),
        },
        Backend::gemini_for_test("synthetic-key", &state.endpoint_url),
        AudioQueue::new_with_recording_budget_for_test(state.max_recording_samples * 2),
      )
      .await;
    })
  }

  async fn start_relay_test(mode: FakeMode, max_recording_samples: usize) -> RelayTestServer {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a fake backend listener");
    let endpoint_url = format!("ws://{}", backend_listener.local_addr().expect("read fake backend address"));
    let (provider_connected_tx, provider_connected) = oneshot::channel();
    let fake_backend = tokio::spawn(async move {
      let Ok(Ok((stream, _))) = tokio::time::timeout(Duration::from_secs(3), backend_listener.accept()).await else {
        return;
      };
      let mut socket = accept_async(stream).await.expect("accept the fake backend connection");
      provider_connected_tx.send(()).expect("relay test waits for the provider connection");
      let Some(Ok(ServerMessage::Text(_setup))) = socket.next().await else {
        panic!("Gemini sends its setup frame");
      };
      socket.send(ServerMessage::Text(r#"{"setupComplete":{}}"#.into())).await.expect("send the fake setup reply");
      let mut early_final_sent = false;
      while let Some(message) = socket.next().await {
        match message.expect("read the Gemini client frame") {
          ServerMessage::Text(text) => {
            let text = text.to_string();
            let value: Value = serde_json::from_str(&text).expect("Gemini sends JSON");
            let is_audio = value.pointer("/realtimeInput/audio").is_some();
            let is_end = value.pointer("/realtimeInput/activityEnd").is_some();
            if matches!(mode, FakeMode::FinalBeforeEnd) && is_audio && !early_final_sent {
              socket
                .send(ServerMessage::Text(
                  r#"{"serverContent":{"inputTranscription":{"text":"synthetic early final"}}}"#.into(),
                ))
                .await
                .expect("send the early final");
              early_final_sent = true;
            }
            if matches!(mode, FakeMode::FinalAfterEnd) && is_end {
              socket
                .send(ServerMessage::Text(
                  r#"{"serverContent":{"interimInputTranscription":{"text":"synthetic partial"}}}"#.into(),
                ))
                .await
                .expect("send the partial");
              socket
                .send(ServerMessage::Text(
                  r#"{"serverContent":{"inputTranscription":{"text":"synthetic final"}}}"#.into(),
                ))
                .await
                .expect("send the final");
            }
          }
          ServerMessage::Close(_) => {
            socket.flush().await.expect("flush the backend close response");
            return;
          }
          message => panic!("unexpected Gemini client frame: {message:?}"),
        }
      }
    });

    let home = tempfile::tempdir().expect("make an isolated relay home");
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("src.features.voice", crate::register).expect("register the voice config section");
    let config = Arc::new(registry.load_config(home.path()).expect("load synthetic voice config"));
    let state = Arc::new(RelayTestState { config: Arc::clone(&config), endpoint_url, max_recording_samples });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the relay test server");
    let address = listener.local_addr().expect("read relay server address");
    let app = Router::new().route("/ws", get(relay_test_handler)).with_state(state);
    let server = tokio::spawn(async move {
      axum::serve(listener, app).await.expect("serve the relay test app");
    });
    RelayTestServer { address, config, _home: home, server, fake_backend, provider_connected }
  }

  async fn receive_json_frame<S>(socket: &mut WebSocketStream<S>) -> Value
  where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
  {
    match tokio::time::timeout(Duration::from_secs(3), socket.next())
      .await
      .expect("relay sends the expected frame")
      .expect("relay socket remains open")
      .expect("read relay frame")
    {
      ClientMessage::Text(text) => serde_json::from_str(&text).expect("relay frame is JSON"),
      message => panic!("expected a JSON relay frame, got {message:?}"),
    }
  }

  async fn browser_socket(
    address: std::net::SocketAddr,
  ) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (socket, _) = connect_async(format!("ws://{address}/ws")).await.expect("connect browser to loopback relay");
    socket
  }

  #[tokio::test]
  async fn provider_connection_opens_before_browser_audio_arrives() {
    let mut server = start_relay_test(FakeMode::NeverFinal, MAX_RECORDING_SAMPLES).await;
    let mut browser = browser_socket(server.address).await;
    tokio::time::timeout(Duration::from_secs(3), &mut server.provider_connected)
      .await
      .expect("provider connects before the relay receives audio")
      .expect("fake provider signals its accepted connection");
    browser.close(None).await.expect("close browser without sending audio");
    stop_relay_test(server).await;
  }

  async fn stop_relay_test(server: RelayTestServer) {
    server.server.abort();
    let _ = server.fake_backend.await;
  }

  fn voice_dir(config: &CharlieBotConfig) -> std::path::PathBuf {
    config.sessions_dir().join("synthetic-session").join("voice")
  }

  #[test]
  fn query_uses_the_last_backend_value_and_defaults_to_empty() {
    assert_eq!(query_value("backend=gemini&backend=muse", "backend").as_deref(), Some("muse"));
    assert_eq!(query_value("other=voice", "backend"), None);
    assert_eq!(query_value("backend=foo+bar", "backend").as_deref(), Some("foo bar"));
  }

  #[test]
  fn control_frames_keep_error_text_and_device_order() {
    assert_eq!(preview_end_devices("{bad"), Err("malformed preview control frame '{bad'".to_string()));
    assert_eq!(
      preview_end_devices("{\"type\": \"other\"}"),
      Err("unsupported preview control frame '{\"type\": \"other\"}'".to_string())
    );
    assert_eq!(preview_end_devices("{\"type\": \"end\"}"), Ok(None));
    assert!(
      preview_end_devices("{\"type\": \"end\", \"devices\": {\"input_device\": null}}")
        .unwrap_err()
        .starts_with("end frame devices must carry exactly")
    );
  }

  #[tokio::test]
  async fn relay_archives_audio_and_transcript_before_the_final_frame() {
    let server = start_relay_test(FakeMode::FinalAfterEnd, MAX_RECORDING_SAMPLES).await;
    let mut browser = browser_socket(server.address).await;
    browser.send(ClientMessage::Binary(vec![1, 2, 3, 4].into())).await.expect("send one audio frame");
    browser
      .send(ClientMessage::Text(
        r#"{"type":"end","devices":{"input_device":"Synthetic mic","capture_settings":{},"output_device":"Synthetic speakers","communications_output_device":null,"device_error":null}}"#
          .into(),
      ))
      .await
      .expect("seal the recording with device facts");
    let partial = receive_json_frame(&mut browser).await;
    let final_frame = receive_json_frame(&mut browser).await;
    assert_eq!(partial, json!({ "type": "partial", "text": "synthetic partial" }));
    assert_eq!(final_frame, json!({ "type": "final", "text": "synthetic final" }));
    let files = std::fs::read_dir(voice_dir(&server.config))
      .expect("read the archived voice directory")
      .map(|entry| entry.expect("read archive entry").path())
      .collect::<Vec<_>>();
    let wav = files.iter().find(|path| path.extension().is_some_and(|extension| extension == "wav"));
    let transcript = files.iter().find(|path| path.extension().is_some_and(|extension| extension == "txt"));
    assert_eq!(files.len(), 2, "the archive pair is complete before final delivery");
    let wav_bytes = std::fs::read(wav.expect("archive contains WAV")).expect("read archived WAV");
    assert_eq!(&wav_bytes[..4], b"RIFF");
    assert_eq!(&wav_bytes[4..8], &40_u32.to_le_bytes());
    assert_eq!(&wav_bytes[8..16], b"WAVEfmt ");
    assert_eq!(&wav_bytes[16..20], &16_u32.to_le_bytes());
    assert_eq!(&wav_bytes[20..24], &[1, 0, 1, 0]);
    assert_eq!(&wav_bytes[24..32], &[0x80, 0x3e, 0, 0, 0, 0x7d, 0, 0]);
    assert_eq!(&wav_bytes[32..36], &[2, 0, 16, 0]);
    assert_eq!(&wav_bytes[36..40], b"data");
    assert_eq!(&wav_bytes[40..44], &4_u32.to_le_bytes());
    assert_eq!(&wav_bytes[44..], &[1, 2, 3, 4]);
    assert_eq!(std::fs::read_to_string(transcript.expect("archive contains transcript")).unwrap(), "synthetic final");
    browser.close(None).await.expect("close browser WebSocket");
    stop_relay_test(server).await;
  }

  #[tokio::test]
  async fn overflow_sends_one_error_then_drains_until_browser_close() {
    let server = start_relay_test(FakeMode::NeverFinal, MAX_RECORDING_SAMPLES).await;
    let mut browser = browser_socket(server.address).await;
    let oversized = vec![0_u8; PREVIEW_QUEUE_SECONDS * crate::audio::SAMPLE_RATE * 2 + 1];
    browser.send(ClientMessage::Binary(oversized.into())).await.expect("send an overflowing frame");
    assert_eq!(
      receive_json_frame(&mut browser).await,
      json!({
        "type": "error",
        "message": "voice preview buffer overflowed (over 30s of audio unconsumed)"
      })
    );
    browser.close(None).await.expect("close browser after the overflow frame");
    assert!(!voice_dir(&server.config).exists(), "overflow never archives the fallback recording");
    stop_relay_test(server).await;
  }

  #[tokio::test]
  async fn frame_after_end_cancels_the_backend_and_returns_the_protocol_error() {
    let server = start_relay_test(FakeMode::NeverFinal, MAX_RECORDING_SAMPLES).await;
    let mut browser = browser_socket(server.address).await;
    browser.send(ClientMessage::Binary(vec![1, 2].into())).await.expect("send audio");
    browser.send(ClientMessage::Text(r#"{"type":"end"}"#.into())).await.expect("seal the recording");
    browser.send(ClientMessage::Binary(vec![3, 4].into())).await.expect("send an illegal post-end frame");
    assert_eq!(
      receive_json_frame(&mut browser).await,
      json!({ "type": "error", "message": "unexpected frame after the recording's end" })
    );
    browser.close(None).await.expect("close browser after the protocol error");
    assert!(!voice_dir(&server.config).exists(), "a protocol violation leaves the recording unarchived");
    stop_relay_test(server).await;
  }

  #[tokio::test]
  async fn backend_final_before_end_is_reported_and_never_archived() {
    let server = start_relay_test(FakeMode::FinalBeforeEnd, MAX_RECORDING_SAMPLES).await;
    let mut browser = browser_socket(server.address).await;
    browser.send(ClientMessage::Binary(vec![1, 2].into())).await.expect("send audio");
    assert_eq!(
      receive_json_frame(&mut browser).await,
      json!({ "type": "error", "message": "backend produced its final before the recording's end frame" })
    );
    browser
      .send(ClientMessage::Text(r#"{"type":"end"}"#.into()))
      .await
      .expect("finish the browser upload after the early final");
    let _ = tokio::time::timeout(Duration::from_secs(3), browser.next()).await;
    assert!(!voice_dir(&server.config).exists(), "an early final never archives incomplete audio");
    stop_relay_test(server).await;
  }

  #[tokio::test]
  async fn disconnect_after_end_abandons_without_archiving() {
    let server = start_relay_test(FakeMode::NeverFinal, MAX_RECORDING_SAMPLES).await;
    let mut browser = browser_socket(server.address).await;
    browser.send(ClientMessage::Binary(vec![1, 2].into())).await.expect("send audio");
    browser.send(ClientMessage::Text(r#"{"type":"end"}"#.into())).await.expect("seal the recording");
    browser.close(None).await.expect("disconnect before backend final");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!voice_dir(&server.config).exists(), "an abandoned recording has no archive");
    stop_relay_test(server).await;
  }

  #[tokio::test]
  async fn test_frames_past_the_cap_are_cut_sample_exactly() {
    let server = start_relay_test(FakeMode::FinalAfterEnd, 3072).await;
    let mut browser = browser_socket(server.address).await;
    let frame = vec![0x2a_u8; 2048 * 2];
    for _ in 0..3 {
      browser.send(ClientMessage::Binary(frame.clone().into())).await.expect("send one live audio frame");
    }
    browser.send(ClientMessage::Text(r#"{"type":"end"}"#.into())).await.expect("seal the recording");
    assert_eq!(receive_json_frame(&mut browser).await["type"], "partial");
    assert_eq!(receive_json_frame(&mut browser).await, json!({ "type": "final", "text": "synthetic final" }));

    let wav = std::fs::read_dir(voice_dir(&server.config))
      .expect("read the archived voice directory")
      .map(|entry| entry.expect("read archive entry").path())
      .find(|path| path.extension().is_some_and(|extension| extension == "wav"))
      .expect("the fake live backend archived one WAV");
    let wav_bytes = std::fs::read(wav).expect("read the archived WAV");
    assert_eq!(&wav_bytes[44..], [frame.as_slice(), &frame[..2048]].concat());
    browser.close(None).await.expect("close browser WebSocket");
    stop_relay_test(server).await;
  }
}

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

use crate::api::{DEVICE_FIELDS, Transcribed, log_voice_transcribed};
use crate::audio::{AudioQueue, PREVIEW_QUEUE_SECONDS};
use crate::config::VoiceSettings;
use crate::recordings;
use crate::recordings::query_value;
use crate::transcription::{self, Backend, TranscriptEvent, TranscriptKind};

type SocketWriter = Arc<Mutex<SplitSink<WebSocket, Message>>>;

struct RelaySettings {
  config: Arc<CharlieBotConfig>,
  vocabulary: Vec<String>,
  languages: Vec<String>,
  session_id: String,
  run_id: String,
}

pub(crate) fn handler(features: Arc<FeatureContext>, request: WebSocketRequest) -> BoxFuture<'static, Response> {
  Box::pin(async move {
    let session_id = request.segments.first().expect("the route captures its session id").clone();
    let backend_id = query_value(&request.query, "backend").unwrap_or_default();
    let run_id = query_value(&request.query, "run_id").unwrap_or_default();
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
    let refusal = refusal.or_else(|| run_id_refusal(&run_id));
    let config = features.config.get();
    request.upgrade.max_message_size(16_777_216).max_frame_size(16_777_216).on_upgrade(move |socket| async move {
      let (writer, reader) = socket.split();
      let writer = Arc::new(Mutex::new(writer));
      if let Some(message) = refusal {
        send_error_frame(&writer, &message, &session_id, &backend_id).await;
        close_socket(&writer).await;
        return;
      }
      let backend = backend.expect("a backend without a refusal is built");
      let config = match config {
        Ok(config) => config,
        Err(error) => {
          let message = error.to_string();
          send_error_frame(&writer, &message, &session_id, &backend_id).await;
          close_socket(&writer).await;
          return;
        }
      };
      let settings = config.section::<VoiceSettings>("voice");
      let vocabulary = settings.vocabulary.clone();
      let languages = settings.languages.clone();
      relay_preview(writer, reader, RelaySettings { config, vocabulary, languages, session_id, run_id }, backend).await;
    })
  })
}

/// Why the relay refuses a socket before any recording work: the run id is not the eight lowercase hex
/// characters the browser makes at the click. A run id outside that form never becomes a path.
fn run_id_refusal(run_id: &str) -> Option<String> {
  (!recordings::valid_run_id(run_id))
    .then(|| format!("voice relay requires an eight lowercase hex run_id, got {}", text_repr::str_repr(run_id)))
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
  let RelaySettings { config, vocabulary, languages, session_id, run_id } = settings;
  let (cancel, _) = watch::channel(false);
  let (events, event_receiver) = mpsc::unbounded_channel::<TranscriptEvent>();
  let backend_id = backend.id().to_string();
  // Whether this relay already persisted the run's outcome — an archived final or a recorded failure. The
  // disconnect paths check it, so a recording that completed never gains a "connection lost" failure.
  let recorded = Arc::new(AtomicBool::new(false));
  let mut streamer = Some(tokio::spawn(stream_preview(StreamContext {
    writer: Arc::clone(&writer),
    queue: Arc::clone(&queue),
    config: Arc::clone(&config),
    session_id: session_id.clone(),
    backend_id: backend_id.clone(),
    run_id: run_id.clone(),
    backend,
    cancel: cancel.clone(),
    events,
    event_receiver,
    vocabulary,
    languages,
    recorded: Arc::clone(&recorded),
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
      let run = RelayRun {
        config: &config,
        session_id: &session_id,
        run_id: &run_id,
        backend_id: &backend_id,
        queue: &queue,
        recorded: &recorded,
      };
      record_relay_failure(&run, "connection lost").await;
    }
    ReceiveOutcome::Error(message) => {
      cancel.send_replace(true);
      await_streamer(streamer.take()).await;
      send_error_frame(&writer, &message, &session_id, &backend_id).await;
      drain_socket(&mut reader).await;
      close_socket(&writer).await;
    }
    ReceiveOutcome::Ended => {
      let run = RelayRun {
        config: &config,
        session_id: &session_id,
        run_id: &run_id,
        backend_id: &backend_id,
        queue: &queue,
        recorded: &recorded,
      };
      settle_sealed_socket(&writer, &mut reader, &cancel, &mut streamer, &run).await;
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
  run_id: String,
  backend: Backend,
  cancel: watch::Sender<bool>,
  events: mpsc::UnboundedSender<TranscriptEvent>,
  event_receiver: mpsc::UnboundedReceiver<TranscriptEvent>,
  vocabulary: Vec<String>,
  languages: Vec<String>,
  recorded: Arc<AtomicBool>,
}

/// The run facts the relay's archive and failure recording share.
struct RelayRun<'a> {
  config: &'a Arc<CharlieBotConfig>,
  session_id: &'a str,
  run_id: &'a str,
  backend_id: &'a str,
  queue: &'a AudioQueue,
  recorded: &'a AtomicBool,
}

/// Records one relay failure under the run's stem: the reason and the audio received so far, once per relay.
/// The first writer of the run's outcome wins, so a run whose final already archived records no failure. A write
/// that fails is an error log, never a skip; the frames the browser receives are unchanged.
async fn record_relay_failure(run: &RelayRun<'_>, reason: &str) {
  if run.recorded.swap(true, Ordering::SeqCst) {
    return;
  }
  let pcm_bytes = run.queue.recording_pcm().await;
  let devices = run.queue.devices().await;
  let config = Arc::clone(run.config);
  let session_id = run.session_id.to_string();
  let run_id = run.run_id.to_string();
  let backend = run.backend_id.to_string();
  let reason = reason.to_string();
  let written = tokio::task::spawn_blocking(move || {
    recordings::store_relay_failure(&config, &session_id, &run_id, &pcm_bytes, &backend, devices.as_ref(), &reason)
  })
  .await;
  match written {
    Ok(Ok(())) => {}
    Ok(Err(error)) => {
      tracing::error!(target: "charliebot_voice", error = %error, "voice_preview_failure_record_failed")
    }
    Err(error) => tracing::error!(target: "charliebot_voice", error = %error, "voice_preview_failure_record_failed"),
  }
}

async fn stream_preview(context: StreamContext) {
  let StreamContext {
    writer,
    queue,
    config,
    session_id,
    backend_id,
    run_id,
    backend,
    cancel,
    events,
    mut event_receiver,
    vocabulary,
    languages,
    recorded,
  } = context;
  let run = RelayRun {
    config: &config,
    session_id: &session_id,
    run_id: &run_id,
    backend_id: &backend_id,
    queue: &queue,
    recorded: &recorded,
  };
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
              record_relay_failure(&run, reason).await;
              send_error_frame(&writer, reason, &session_id, &backend_id).await;
              let _ = event.acknowledged.send(());
              cancel.send_replace(true);
              await_backend_worker(worker.take().expect("backend worker is present"), &session_id, &backend_id).await;
              return;
            }
            archive_final(&writer, &run, &event).await
          }
        };
          let _ = event.acknowledged.send(());
        if let Err(error) = sent {
          log_socket_lost(&session_id, &backend_id, &error);
          record_relay_failure(&run, &error).await;
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
        record_relay_failure(&run, &reason).await;
        send_error_frame(&writer, &reason, &session_id, &backend_id).await;
      }
    }
    Err(error) if error.is_cancelled() && *cancellation.borrow() => {}
    Err(error) => {
      let reason = error.to_string();
      tracing::warn!(target: "charliebot_voice", session_id, backend = %backend_id, reason = %reason, "voice_preview_failed");
      record_relay_failure(&run, &reason).await;
      send_error_frame(&writer, &reason, &session_id, &backend_id).await;
    }
  }
}

async fn archive_final(writer: &SocketWriter, run: &RelayRun<'_>, event: &TranscriptEvent) -> Result<(), String> {
  match archive_final_with(run, event, |_, text| async move {
    send_frame(writer, json!({ "type": "final", "text": text })).await
  })
  .await
  {
    Ok(()) => Ok(()),
    Err(ArchiveFinalError::Archive(error)) => {
      tracing::error!(target: "charliebot_voice", session_id = run.session_id, backend = %run.backend_id, "voice_preview_archive_failed");
      record_relay_failure(run, &error).await;
      send_error_frame(writer, "recording archive failed", run.session_id, run.backend_id).await;
      Ok(())
    }
    Err(ArchiveFinalError::Delivery(error)) => Err(error),
  }
}

#[derive(Debug)]
enum ArchiveFinalError {
  Archive(String),
  Delivery(String),
}

async fn archive_final_with<F, Fut>(
  run: &RelayRun<'_>,
  event: &TranscriptEvent,
  deliver_final: F,
) -> Result<(), ArchiveFinalError>
where
  F: FnOnce(std::path::PathBuf, String) -> Fut,
  Fut: Future<Output = Result<(), String>>,
{
  let pcm_bytes = run.queue.recording_pcm().await;
  let audio_bytes_size = pcm_bytes.len();
  let text = event.text.clone();
  let devices = run.queue.devices().await;
  let cfg = Arc::clone(run.config);
  let archive_session_id = run.session_id.to_string();
  let archive_run_id = run.run_id.to_string();
  let archive_backend_id = run.backend_id.to_string();
  let archive_devices = devices.clone();
  let archive = tokio::task::spawn_blocking(move || {
    recordings::archive_relay_final(
      &cfg,
      &archive_session_id,
      &archive_run_id,
      &pcm_bytes,
      &text,
      &archive_backend_id,
      archive_devices.as_ref(),
    )
  })
  .await;
  let audio_path = match archive {
    Ok(Ok(audio_path)) => {
      run.recorded.store(true, Ordering::SeqCst);
      audio_path
    }
    Ok(Err(error)) => return Err(ArchiveFinalError::Archive(error.to_string())),
    Err(error) => return Err(ArchiveFinalError::Archive(error.to_string())),
  };
  let end_to_final_s = run.queue.seconds_since_end().await;
  log_voice_transcribed(&Transcribed {
    session_id: run.session_id,
    audio_path: &audio_path,
    audio_bytes_size,
    text: &event.text,
    produced_by: run.backend_id,
    selected_backend: Some(run.backend_id),
    devices: devices.as_ref(),
    end_to_final_s: Some(end_to_final_s),
  });
  deliver_final(audio_path, event.text.clone()).await.map_err(ArchiveFinalError::Delivery)
}

async fn settle_sealed_socket(
  writer: &SocketWriter,
  reader: &mut SplitStream<WebSocket>,
  cancel: &watch::Sender<bool>,
  streamer: &mut Option<JoinHandle<()>>,
  run: &RelayRun<'_>,
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
            log_abandoned(run.queue, run.session_id, run.backend_id).await;
            record_relay_failure(run, "connection lost").await;
            return;
          }
          Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
          Some(Ok(Message::Binary(_) | Message::Text(_))) => {
            cancel.send_replace(true);
            await_streamer(streamer.take()).await;
            send_error_frame(writer, "unexpected frame after the recording's end", run.session_id, run.backend_id).await;
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

/// The relay's one error-frame path: a socket that no longer takes the frame is reported, not dropped.
async fn send_error_frame(writer: &SocketWriter, message: &str, session_id: &str, backend_id: &str) {
  if let Err(error) = send_frame(writer, json!({ "type": "error", "message": message })).await {
    log_socket_lost(session_id, backend_id, &error);
  }
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
      text_repr::value_repr(&json!(DEVICE_FIELDS)),
      text_repr::value_repr(devices)
    ));
  };
  let fields_match = devices.len() == DEVICE_FIELDS.len() && DEVICE_FIELDS.iter().all(|key| devices.contains_key(*key));
  if !fields_match {
    return Err(format!(
      "end frame devices must carry exactly {}: {}",
      text_repr::value_repr(&json!(DEVICE_FIELDS)),
      text_repr::value_repr(&Value::Object(devices.clone()))
    ));
  }
  Ok(Some(Value::Object(devices.clone())))
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
  use charliebot_core::config::{
    BackendsConfig, GithubConfig, PathsConfig, RemoteRunConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
  };
  use futures_util::{SinkExt, StreamExt};
  use tokio::net::TcpListener;
  use tokio::sync::{oneshot, watch};
  use tokio_tungstenite::tungstenite::Message as ClientMessage;
  use tokio_tungstenite::tungstenite::Message as ServerMessage;
  use tokio_tungstenite::{WebSocketStream, accept_async, connect_async};

  use crate::audio::MAX_RECORDING_SAMPLES;

  struct RelayTestState {
    config: Arc<CharlieBotConfig>,
    endpoint_url: String,
    max_recording_samples: usize,
    max_preview_queue_bytes: usize,
    relay_finished: watch::Sender<bool>,
  }

  struct RelayTestServer {
    address: std::net::SocketAddr,
    config: Arc<CharlieBotConfig>,
    _home: tempfile::TempDir,
    server: JoinHandle<()>,
    fake_backend: JoinHandle<()>,
    provider_connected: oneshot::Receiver<()>,
    relay_finished: watch::Receiver<bool>,
  }

  #[derive(Clone, Copy)]
  enum FakeMode {
    FinalBeforeEnd,
    NeverFinal,
  }

  fn relay_test_config(home: &std::path::Path) -> Arc<CharlieBotConfig> {
    Arc::new(CharlieBotConfig {
      charliebot_home: home.to_path_buf(),
      headless_chrome_bin: "synthetic-chrome".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18498,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 0,
        session_swap_max_mb: 0,
        session_tasks_max: 0,
        enable_session_cpu_weight: false,
        enable_session_io_weight: false,
        enable_local_work_guard: false,
        min_free_disk_gib: 0,
        public_url: None,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      remote_run: RemoteRunConfig::default(),
      github: GithubConfig::default(),
      sections: Sections::default(),
    })
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
          run_id: "5e1f2a3b".to_string(),
        },
        Backend::gemini_for_test("synthetic-key", &state.endpoint_url),
        AudioQueue::new_with_limits_for_test(state.max_preview_queue_bytes, state.max_recording_samples * 2),
      )
      .await;
      state.relay_finished.send_replace(true);
    })
  }

  async fn start_relay_test(mode: FakeMode, max_recording_samples: usize) -> RelayTestServer {
    start_relay_test_with_limits(mode, PREVIEW_QUEUE_SECONDS * crate::audio::SAMPLE_RATE * 2, max_recording_samples)
      .await
  }

  async fn start_relay_test_with_limits(
    mode: FakeMode,
    max_preview_queue_bytes: usize,
    max_recording_samples: usize,
  ) -> RelayTestServer {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a fake backend listener");
    let endpoint_url = format!("ws://{}", backend_listener.local_addr().expect("read fake backend address"));
    let (provider_connected_tx, provider_connected) = oneshot::channel();
    let fake_backend = tokio::spawn(async move {
      let Ok(Ok((stream, _))) = tokio::time::timeout(Duration::from_secs(3), backend_listener.accept()).await else {
        return;
      };
      stream.set_nodelay(true).expect("disable Nagle on the fake backend socket");
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
            if matches!(mode, FakeMode::FinalBeforeEnd) && is_audio && !early_final_sent {
              socket
                .send(ServerMessage::Text(
                  r#"{"serverContent":{"inputTranscription":{"text":"synthetic early final"}}}"#.into(),
                ))
                .await
                .expect("send the early final");
              early_final_sent = true;
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
    let config = relay_test_config(home.path());
    let (relay_finished, relay_finished_receiver) = watch::channel(false);
    let state = Arc::new(RelayTestState {
      config: Arc::clone(&config),
      endpoint_url,
      max_recording_samples,
      max_preview_queue_bytes,
      relay_finished,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the relay test server");
    let address = listener.local_addr().expect("read relay server address");
    let app = Router::new().route("/ws", get(relay_test_handler)).with_state(state);
    let server = tokio::spawn(async move {
      axum::serve(listener, app).await.expect("serve the relay test app");
    });
    RelayTestServer {
      address,
      config,
      _home: home,
      server,
      fake_backend,
      provider_connected,
      relay_finished: relay_finished_receiver,
    }
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
    let (mut socket, _) = connect_async(format!("ws://{address}/ws")).await.expect("connect browser to loopback relay");
    let tokio_tungstenite::MaybeTlsStream::Plain(stream) = socket.get_mut() else {
      panic!("the loopback browser socket uses plain TCP");
    };
    stream.set_nodelay(true).expect("disable Nagle on the browser socket");
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
  fn a_socket_whose_run_id_is_not_eight_lowercase_hex_is_refused_before_any_path_is_built() {
    assert_eq!(run_id_refusal("5e1f2a3b"), None);
    assert!(run_id_refusal("").is_some(), "a missing run_id is a refusal");
    assert!(run_id_refusal("5E1F2A3B").is_some(), "uppercase is not the browser's form");
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
  async fn archive_final_writes_audio_and_transcript_before_delivery() {
    let home = tempfile::tempdir().expect("make an isolated relay home");
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("features.voice", crate::register).expect("register the voice config section");
    let config = Arc::new(registry.load_config(home.path()).expect("load synthetic voice config"));
    let queue = AudioQueue::new_with_limits_for_test(4, 4);
    assert!(queue.put(vec![1, 2, 3, 4]).await);
    queue
      .end(Some(json!({
        "input_device": "Synthetic mic",
        "capture_settings": {},
        "output_device": "Synthetic speakers",
        "communications_output_device": null,
        "device_error": null
      })))
      .await;
    let recorded = AtomicBool::new(false);
    let run = RelayRun {
      config: &config,
      session_id: "synthetic-session",
      run_id: "5e1f2a3b",
      backend_id: "gemini",
      queue: &queue,
      recorded: &recorded,
    };
    let (acknowledged, _acknowledgement) = oneshot::channel();
    let event = TranscriptEvent { kind: TranscriptKind::Final, text: "synthetic final".to_string(), acknowledged };

    archive_final_with(&run, &event, |audio_path, text| async move {
      assert_eq!(text, "synthetic final");
      let voice_dir = audio_path.parent().expect("the archive path has a directory");
      assert_eq!(std::fs::read_dir(voice_dir).expect("read the archive directory").count(), 3);
      let parts: Value =
        serde_json::from_str(&std::fs::read_to_string(audio_path.with_extension("json")).unwrap()).unwrap();
      assert_eq!(parts["run_id"], "5e1f2a3b");
      assert_eq!(parts["startup"], Value::Null);
      assert_eq!(parts["failure"], Value::Null);
      assert_eq!(parts["audio"]["path"], "relay");
      assert_eq!(parts["audio"]["backend"], "gemini");
      assert_eq!(parts["audio"]["devices"]["input_device"], "Synthetic mic");
      let wav_bytes = std::fs::read(&audio_path).expect("read archived WAV");
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
      assert_eq!(
        std::fs::read_to_string(audio_path.with_extension("txt")).expect("read archived transcript"),
        "synthetic final"
      );
      Ok(())
    })
    .await
    .unwrap();
    assert!(recorded.load(Ordering::SeqCst));
  }

  #[tokio::test]
  async fn overflow_sends_one_error_then_drains_until_browser_close() {
    let server = start_relay_test_with_limits(FakeMode::NeverFinal, 4, MAX_RECORDING_SAMPLES).await;
    let mut browser = browser_socket(server.address).await;
    let oversized = vec![0_u8; 5];
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
    let server = start_relay_test_with_limits(FakeMode::NeverFinal, 2, 1).await;
    let mut browser = browser_socket(server.address).await;
    browser.send(ClientMessage::Text(r#"{"type":"end"}"#.into())).await.expect("seal the recording");
    browser.send(ClientMessage::Binary(vec![3, 4].into())).await.expect("send an illegal post-end frame");
    assert_eq!(
      receive_json_frame(&mut browser).await,
      json!({ "type": "error", "message": "unexpected frame after the recording's end" })
    );
    drop(browser);
    assert!(!voice_dir(&server.config).exists(), "a protocol violation leaves the recording unarchived");
    stop_relay_test(server).await;
  }

  #[tokio::test]
  async fn backend_final_before_end_records_the_failure_and_the_partial_audio() {
    let server = start_relay_test_with_limits(FakeMode::FinalBeforeEnd, 2, 1).await;
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
    let files: Vec<std::path::PathBuf> = std::fs::read_dir(voice_dir(&server.config))
      .expect("read the failed run's voice directory")
      .map(|entry| entry.expect("read archive entry").path())
      .collect();
    assert_eq!(files.len(), 2, "the failure archives the partial wav and the run json: {files:?}");
    let parts = files.iter().find(|path| path.extension().is_some_and(|extension| extension == "json"));
    let parts: Value =
      serde_json::from_str(&std::fs::read_to_string(parts.expect("the run json exists")).expect("read parts"))
        .expect("the run json parses");
    assert_eq!(
      parts["failure"],
      json!({"stage": "relay", "error": "backend produced its final before the recording's end frame"})
    );
    assert_eq!(parts["audio"], json!({"path": "relay", "backend": "gemini", "devices": null}));
    stop_relay_test(server).await;
  }

  #[tokio::test]
  async fn disconnect_after_end_records_the_failure_and_the_partial_audio() {
    let mut server = start_relay_test_with_limits(FakeMode::NeverFinal, 4, 2).await;
    let mut browser = browser_socket(server.address).await;
    browser.send(ClientMessage::Binary(vec![1, 2].into())).await.expect("send audio");
    browser.send(ClientMessage::Text(r#"{"type":"end"}"#.into())).await.expect("seal the recording");
    browser.close(None).await.expect("disconnect before backend final");
    tokio::time::timeout(Duration::from_secs(1), server.relay_finished.changed())
      .await
      .expect("relay observes the browser disconnect")
      .expect("relay completion signal remains available");
    assert!(*server.relay_finished.borrow(), "the relay completes after disconnect");
    let files: Vec<std::path::PathBuf> = std::fs::read_dir(voice_dir(&server.config))
      .expect("read the failed run's voice directory")
      .map(|entry| entry.expect("read archive entry").path())
      .collect();
    assert_eq!(files.len(), 2, "the disconnect archives the partial wav and the run json: {files:?}");
    let parts = files.iter().find(|path| path.extension().is_some_and(|extension| extension == "json"));
    let parts: Value =
      serde_json::from_str(&std::fs::read_to_string(parts.expect("the run json exists")).expect("read parts"))
        .expect("the run json parses");
    assert_eq!(parts["failure"], json!({"stage": "relay", "error": "connection lost"}));
    assert_eq!(parts["audio"], json!({"path": "relay", "backend": "gemini", "devices": null}));
    stop_relay_test(server).await;
  }
}

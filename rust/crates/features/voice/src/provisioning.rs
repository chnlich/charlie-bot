//! Background provisioning of the sherpa speech assets in `<home>/models`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, bail};
use bzip2::read::BzDecoder;
use charliebot_compat::http_client::{self, Settings};
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use sha2::{Digest, Sha256};
use tar::Archive;
use tokio::runtime::Builder;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::{VoiceEngine, VoiceSettings};

const QWEN3_ASR_DIR_NAME: &str = "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25";
const QWEN3_ASR_ARCHIVE_NAME: &str = "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2";
const QWEN3_ASR_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2";
const QWEN3_ASR_SHA256: &str = "393f8a14e2f5fb96746aaab342997a40641001fbd5bf9592a080a8329178ee96";
const SILERO_VAD_NAME: &str = "silero_vad.onnx";
const SILERO_VAD_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";
const SILERO_VAD_SHA256: &str = "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6";
const HTTP_MODEL_DOWNLOAD_TIMEOUT_SECS: u64 = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VoiceModelPaths {
  pub(crate) cache_dir: PathBuf,
  pub(crate) qwen3_archive: PathBuf,
  pub(crate) qwen3_dir: PathBuf,
  pub(crate) qwen3_conv_frontend: PathBuf,
  pub(crate) qwen3_encoder: PathBuf,
  pub(crate) qwen3_decoder: PathBuf,
  pub(crate) qwen3_tokenizer: PathBuf,
  pub(crate) silero_vad: PathBuf,
}

#[derive(Default)]
struct ProvisioningState {
  started: bool,
  ready_paths: Option<VoiceModelPaths>,
  error: Option<String>,
}

static PROVISIONING_STATE: OnceLock<Mutex<ProvisioningState>> = OnceLock::new();

fn provisioning_state() -> &'static Mutex<ProvisioningState> {
  PROVISIONING_STATE.get_or_init(|| Mutex::new(ProvisioningState::default()))
}

/// A request reached before model provisioning succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpeechModelsNotReadyError(String);

impl std::fmt::Display for SpeechModelsNotReadyError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl std::error::Error for SpeechModelsNotReadyError {}

fn voice_model_paths(cfg: &CharlieBotConfig) -> VoiceModelPaths {
  let cache_dir = cfg.charliebot_home.join("models");
  let qwen3_dir = cache_dir.join(QWEN3_ASR_DIR_NAME);
  VoiceModelPaths {
    qwen3_archive: cache_dir.join(QWEN3_ASR_ARCHIVE_NAME),
    qwen3_conv_frontend: qwen3_dir.join("conv_frontend.onnx"),
    qwen3_decoder: qwen3_dir.join("decoder.int8.onnx"),
    qwen3_dir,
    qwen3_encoder: cache_dir.join(QWEN3_ASR_DIR_NAME).join("encoder.int8.onnx"),
    qwen3_tokenizer: cache_dir.join(QWEN3_ASR_DIR_NAME).join("tokenizer"),
    silero_vad: cache_dir.join(SILERO_VAD_NAME),
    cache_dir,
  }
}

#[allow(dead_code)]
pub(crate) fn get_ready_model_paths() -> Result<VoiceModelPaths, SpeechModelsNotReadyError> {
  get_ready_model_paths_from(provisioning_state())
}

fn get_ready_model_paths_from(state: &Mutex<ProvisioningState>) -> Result<VoiceModelPaths, SpeechModelsNotReadyError> {
  let state = state.lock().expect("speech provisioning state lock is never poisoned");
  if let Some(paths) = &state.ready_paths {
    return Ok(paths.clone());
  }
  match &state.error {
    Some(error) if !error.is_empty() => Err(SpeechModelsNotReadyError(format!("speech models are not ready: {error}"))),
    _ => Err(SpeechModelsNotReadyError("speech models are still downloading".to_string())),
  }
}

fn provision_models(cfg: &CharlieBotConfig) {
  provision_models_with(provisioning_state(), || ensure_models_cached(cfg));
}

fn provision_models_with(state: &Mutex<ProvisioningState>, ensure: impl FnOnce() -> anyhow::Result<VoiceModelPaths>) {
  {
    let mut state = state.lock().expect("speech provisioning state lock is never poisoned");
    if state.started {
      return;
    }
    state.started = true;
    state.error = None;
  }

  match ensure() {
    Ok(paths) => {
      let cache_dir = paths.cache_dir.display().to_string();
      state.lock().expect("speech provisioning state lock is never poisoned").ready_paths = Some(paths);
      tracing::info!(cache_dir, "speech_models_ready");
    }
    Err(error) => {
      let error = format!("{error:#}");
      {
        let mut state = state.lock().expect("speech provisioning state lock is never poisoned");
        state.error = Some(error.clone());
        state.started = false;
      }
      tracing::error!(exception = %error, "speech_model_provisioning_failed");
    }
  }
}

fn ensure_models_cached(cfg: &CharlieBotConfig) -> anyhow::Result<VoiceModelPaths> {
  let paths = voice_model_paths(cfg);
  ensure_engine_cached(&paths, cfg.section::<VoiceSettings>("voice").engine, &Settings::from_env())
}

fn ensure_engine_cached(
  paths: &VoiceModelPaths,
  engine: VoiceEngine,
  settings: &Settings,
) -> anyhow::Result<VoiceModelPaths> {
  match engine {
    VoiceEngine::Sherpa => ensure_sherpa_paths_cached(paths, settings),
    VoiceEngine::Qwen3Hf => {
      fs::create_dir_all(&paths.cache_dir).with_context(|| format!("create {}", paths.cache_dir.display()))?;
      bail!("qwen3_hf provisioning is not ported")
    }
  }
}

fn ensure_sherpa_paths_cached(paths: &VoiceModelPaths, settings: &Settings) -> anyhow::Result<VoiceModelPaths> {
  ensure_sherpa_paths_cached_with(paths, settings, QWEN3_ASR_URL, QWEN3_ASR_SHA256, SILERO_VAD_URL, SILERO_VAD_SHA256)
}

fn ensure_sherpa_paths_cached_with(
  paths: &VoiceModelPaths,
  settings: &Settings,
  archive_url: &str,
  archive_sha256: &str,
  vad_url: &str,
  vad_sha256: &str,
) -> anyhow::Result<VoiceModelPaths> {
  fs::create_dir_all(&paths.cache_dir).with_context(|| format!("create {}", paths.cache_dir.display()))?;
  ensure_artifact(&paths.qwen3_archive, archive_url, archive_sha256, settings)?;
  ensure_artifact(&paths.silero_vad, vad_url, vad_sha256, settings)?;
  ensure_qwen3_extracted(paths)?;
  verify_model_files(paths)?;
  Ok(paths.clone())
}

fn ensure_artifact(path: &Path, url: &str, expected_sha256: &str, settings: &Settings) -> anyhow::Result<()> {
  if path.exists() {
    let actual = sha256_file(path)?;
    if actual != expected_sha256 {
      bail!("cached speech model hash mismatch for {}: {actual}", path.display());
    }
    return Ok(());
  }

  atomic_write_stream(path, |stream| {
    let runtime = Builder::new_current_thread().enable_all().build().context("build model download runtime")?;
    let client = http_client::client(settings).context("build model download HTTP client")?;
    runtime.block_on(async {
      // Python's src/features/voice/transcriber.py::_ensure_artifact passes
      // src/infra/timeouts.py::HTTP_MODEL_DOWNLOAD_TIMEOUT to urlopen here, which bounds each socket wait.
      // reqwest's request timeout would bound the whole body instead, so each wait is bounded here.
      let timeout = Duration::from_secs(HTTP_MODEL_DOWNLOAD_TIMEOUT_SECS);
      let mut response = tokio::time::timeout(timeout, client.get(url).send())
        .await
        .with_context(|| format!("model download timed out for {url}"))?
        .with_context(|| format!("model download failed for {url}"))?;
      let status = response.status().as_u16();
      if status != 200 {
        bail!("model download failed for {url}: HTTP {status}");
      }

      let mut digest = Sha256::new();
      while let Some(chunk) = tokio::time::timeout(timeout, response.chunk())
        .await
        .with_context(|| format!("model download timed out for {url}"))?
        .context("read model download response")?
      {
        digest.update(&chunk);
        stream.write_all(&chunk).context("write model download chunk")?;
      }
      let actual = format!("{:x}", digest.finalize());
      if actual != expected_sha256 {
        bail!("downloaded speech model hash mismatch for {url}: {actual}");
      }
      Ok(())
    })
  })
}

fn atomic_write_stream(path: &Path, write: impl FnOnce(&mut File) -> anyhow::Result<()>) -> anyhow::Result<()> {
  let parent = path.parent().context("model artifact path has no parent directory")?;
  let name = path.file_name().context("model artifact path has no filename")?.to_string_lossy();
  let temporary = parent.join(format!(".{name}.{}.tmp", Uuid::new_v4().simple()));
  let result = (|| {
    let mut stream = OpenOptions::new()
      .write(true)
      .create_new(true)
      .open(&temporary)
      .with_context(|| format!("create temporary model file {}", temporary.display()))?;
    write(&mut stream)?;
    drop(stream);
    fs::rename(&temporary, path)
      .with_context(|| format!("publish model file {} as {}", temporary.display(), path.display()))?;
    Ok(())
  })();

  if result.is_err() && temporary.exists() {
    fs::remove_file(&temporary).with_context(|| format!("remove temporary model file {}", temporary.display()))?;
  }
  result
}

fn qwen3_model_files(paths: &VoiceModelPaths) -> [PathBuf; 6] {
  [
    paths.qwen3_conv_frontend.clone(),
    paths.qwen3_encoder.clone(),
    paths.qwen3_decoder.clone(),
    paths.qwen3_tokenizer.join("merges.txt"),
    paths.qwen3_tokenizer.join("tokenizer_config.json"),
    paths.qwen3_tokenizer.join("vocab.json"),
  ]
}

fn ensure_qwen3_extracted(paths: &VoiceModelPaths) -> anyhow::Result<()> {
  if qwen3_model_files(paths).iter().all(|path| path.is_file()) {
    return Ok(());
  }

  let temporary_root = paths.cache_dir.join(format!(".{QWEN3_ASR_DIR_NAME}.{}.extracting", Uuid::new_v4().simple()));
  let extraction = (|| {
    fs::create_dir(&temporary_root).with_context(|| format!("create {}", temporary_root.display()))?;
    let archive_file =
      File::open(&paths.qwen3_archive).with_context(|| format!("open {}", paths.qwen3_archive.display()))?;
    Archive::new(BzDecoder::new(archive_file))
      .unpack(&temporary_root)
      .with_context(|| format!("extract {}", paths.qwen3_archive.display()))?;
    let extracted = temporary_root.join(QWEN3_ASR_DIR_NAME);
    if !extracted.is_dir() {
      bail!("Qwen3-ASR archive did not contain {QWEN3_ASR_DIR_NAME}");
    }
    fs::rename(&extracted, &paths.qwen3_dir)
      .with_context(|| format!("publish extracted model directory {}", paths.qwen3_dir.display()))?;
    Ok(())
  })();
  if temporary_root.exists() {
    fs::remove_dir_all(&temporary_root).with_context(|| format!("remove {}", temporary_root.display()))?;
  }
  extraction
}

fn verify_model_files(paths: &VoiceModelPaths) -> anyhow::Result<()> {
  let missing: Vec<String> = qwen3_model_files(paths)
    .into_iter()
    .chain(std::iter::once(paths.silero_vad.clone()))
    .filter(|path| !path.is_file())
    .map(|path| path.display().to_string())
    .collect();
  if !missing.is_empty() {
    bail!("speech model files are missing: {}", missing.join(", "));
  }
  Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
  let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
  let mut digest = Sha256::new();
  let mut buffer = [0_u8; 1024 * 1024];
  loop {
    let count = file.read(&mut buffer).with_context(|| format!("read {}", path.display()))?;
    if count == 0 {
      break;
    }
    digest.update(&buffer[..count]);
  }
  Ok(format!("{:x}", digest.finalize()))
}

#[derive(Default)]
pub(crate) struct SpeechService {
  task: Mutex<Option<JoinHandle<()>>>,
}

impl Service for SpeechService {
  fn name(&self) -> &'static str {
    "speech"
  }

  fn phase(&self) -> Phase {
    Phase::Early
  }

  fn start<'a>(&'a self, ctx: ServiceContext) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let cfg = ctx.features.config.get().map_err(|error| anyhow::anyhow!(error.to_string()))?;
      let cfg = cfg.as_ref().clone();
      let worker = tokio::task::spawn_blocking(move || provision_models(&cfg));
      let monitor = tokio::spawn(async move {
        if let Err(error) = worker.await {
          tracing::error!(exception = %error, "speech_model_provisioning_failed");
        }
      });
      *self.task.lock().expect("speech service task lock is never poisoned") = Some(monitor);
      Ok(())
    })
  }

  fn stop<'a>(&'a self) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let task = self.task.lock().expect("speech service task lock is never poisoned").take();
      if let Some(task) = task {
        task.abort();
        match task.await {
          Ok(()) => {}
          Err(error) if error.is_cancelled() => {}
          Err(error) => return Err(error.into()),
        }
      }
      Ok(())
    })
  }
}

#[cfg(test)]
mod tests {
  use std::io::Cursor;
  use std::net::TcpListener;
  use std::thread;

  use bzip2::write::BzEncoder;
  use sha2::{Digest, Sha256};
  use tar::{Builder, Header};

  use super::*;

  fn test_paths(root: &Path) -> VoiceModelPaths {
    let cache_dir = root.join("models");
    let qwen3_dir = cache_dir.join(QWEN3_ASR_DIR_NAME);
    VoiceModelPaths {
      qwen3_archive: cache_dir.join(QWEN3_ASR_ARCHIVE_NAME),
      qwen3_conv_frontend: qwen3_dir.join("conv_frontend.onnx"),
      qwen3_decoder: qwen3_dir.join("decoder.int8.onnx"),
      qwen3_dir,
      qwen3_encoder: cache_dir.join(QWEN3_ASR_DIR_NAME).join("encoder.int8.onnx"),
      qwen3_tokenizer: cache_dir.join(QWEN3_ASR_DIR_NAME).join("tokenizer"),
      silero_vad: cache_dir.join(SILERO_VAD_NAME),
      cache_dir,
    }
  }

  fn model_archive() -> Vec<u8> {
    let entries: [(&str, &[u8]); 6] = [
      ("conv_frontend.onnx", b"synthetic frontend"),
      ("encoder.int8.onnx", b"synthetic encoder"),
      ("decoder.int8.onnx", b"synthetic decoder"),
      ("tokenizer/merges.txt", b"synthetic merges"),
      ("tokenizer/tokenizer_config.json", b"{}"),
      ("tokenizer/vocab.json", b"{}"),
    ];
    let encoder = BzEncoder::new(Vec::new(), bzip2::Compression::best());
    let mut archive = Builder::new(encoder);
    for (name, contents) in entries {
      let mut header = Header::new_gnu();
      header.set_size(contents.len() as u64);
      header.set_mode(0o644);
      header.set_cksum();
      archive
        .append_data(&mut header, format!("{QWEN3_ASR_DIR_NAME}/{name}"), Cursor::new(contents))
        .expect("append a synthetic model file");
    }
    archive.into_inner().expect("finish the tar archive").finish().expect("finish bzip2 compression")
  }

  fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
  }

  fn loopback_response(status: u16, body: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local HTTP server");
    let address = listener.local_addr().expect("read local HTTP address");
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept the model request");
      let mut request = [0_u8; 1024];
      let _ = stream.read(&mut request).expect("read the model request");
      let status_text = if status == 200 { "OK" } else { "Unavailable" };
      write!(stream, "HTTP/1.1 {status} {status_text}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
        .expect("write the response headers");
      stream.write_all(&body).expect("write the model response");
    });
    (format!("http://{address}/model",), handle)
  }

  fn assert_no_temporary_files(directory: &Path) {
    let entries: Vec<PathBuf> = fs::read_dir(directory)
      .expect("read the synthetic model directory")
      .map(|entry| entry.expect("read a directory entry").path())
      .collect();
    assert!(
      entries.iter().all(|path| {
        let name = path.file_name().expect("directory entry has a name").to_string_lossy();
        !name.ends_with(".tmp") && !name.ends_with(".extracting")
      }),
      "temporary files remain: {entries:?}"
    );
  }

  #[test]
  fn cached_model_artifacts_with_valid_hashes_are_extracted_and_ready() {
    let root = tempfile::tempdir().expect("create a synthetic model home");
    let paths = test_paths(root.path());
    fs::create_dir_all(&paths.cache_dir).expect("create the synthetic cache");
    let archive = model_archive();
    let vad = b"synthetic vad model";
    fs::write(&paths.qwen3_archive, &archive).expect("cache the synthetic archive");
    fs::write(&paths.silero_vad, vad).expect("cache the synthetic VAD");

    let ready = ensure_sherpa_paths_cached_with(
      &paths,
      &Settings::default(),
      "http://127.0.0.1/archive",
      &sha256(&archive),
      "http://127.0.0.1/vad",
      &sha256(vad),
    )
    .expect("verify and prepare cached assets");

    assert_eq!(ready, paths);
    verify_model_files(&ready).expect("the six model files and VAD are present");
  }

  #[test]
  fn cached_hash_mismatch_uses_python_error_text() {
    let root = tempfile::tempdir().expect("create a synthetic cache");
    let path = root.path().join("cached-model.bin");
    fs::write(&path, b"wrong bytes").expect("write a mismatched cached model");
    let actual = sha256(b"wrong bytes");

    let error = ensure_artifact(&path, "http://127.0.0.1/model", &sha256(b"expected bytes"), &Settings::default())
      .expect_err("a mismatched cached asset must fail");

    assert_eq!(error.to_string(), format!("cached speech model hash mismatch for {}: {actual}", path.display()));
  }

  #[test]
  fn qwen3_hf_provisioning_creates_the_cache_then_fails_with_the_recorded_text() {
    let root = tempfile::tempdir().expect("create a synthetic model home");
    let paths = test_paths(root.path());

    let error = ensure_engine_cached(&paths, VoiceEngine::Qwen3Hf, &Settings::default())
      .expect_err("the not-yet-ported engine must fail visibly");

    assert_eq!(error.to_string(), "qwen3_hf provisioning is not ported");
    assert!(paths.cache_dir.is_dir());
  }

  #[test]
  fn download_is_published_after_verification_without_a_temporary_sibling() {
    let root = tempfile::tempdir().expect("create a synthetic cache");
    let path = root.path().join("model.bin");
    let body = b"synthetic downloaded model".to_vec();
    let (url, server) = loopback_response(200, body.clone());

    ensure_artifact(&path, &url, &sha256(&body), &Settings::default()).expect("download and publish the model");
    server.join().expect("finish the loopback HTTP server");

    assert_eq!(fs::read(&path).expect("read the published model"), body);
    let entries: Vec<PathBuf> = fs::read_dir(root.path())
      .expect("read the synthetic cache")
      .map(|entry| entry.expect("read a cache entry").path())
      .collect();
    assert_eq!(entries, [path]);
  }

  #[test]
  fn failed_download_removes_its_temporary_sibling() {
    let root = tempfile::tempdir().expect("create a synthetic cache");
    let path = root.path().join("model.bin");
    let body = b"wrong downloaded bytes".to_vec();
    let (url, server) = loopback_response(200, body.clone());

    let error = ensure_artifact(&path, &url, &sha256(b"expected bytes"), &Settings::default())
      .expect_err("a downloaded hash mismatch must fail");
    server.join().expect("finish the loopback HTTP server");

    assert_eq!(error.to_string(), format!("downloaded speech model hash mismatch for {url}: {}", sha256(&body)));
    assert!(!path.exists());
    assert_no_temporary_files(root.path());
  }

  #[test]
  fn provisioning_second_call_in_one_process_does_not_run_twice() {
    let state = Mutex::new(ProvisioningState::default());
    let root = tempfile::tempdir().expect("create a synthetic model home");
    let paths = test_paths(root.path());
    let calls = std::sync::atomic::AtomicUsize::new(0);
    for _ in 0..2 {
      provision_models_with(&state, || {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(paths.clone())
      });
    }

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(get_ready_model_paths_from(&state), Ok(paths));
  }

  #[test]
  fn extraction_publishes_a_small_tar_bz2_and_cleans_staging_directory() {
    let root = tempfile::tempdir().expect("create a synthetic model home");
    let paths = test_paths(root.path());
    fs::create_dir_all(&paths.cache_dir).expect("create the synthetic cache");
    fs::write(&paths.qwen3_archive, model_archive()).expect("write a synthetic tar.bz2 archive");

    ensure_qwen3_extracted(&paths).expect("extract the model archive");

    assert!(paths.qwen3_encoder.is_file());
    assert!(paths.qwen3_tokenizer.join("vocab.json").is_file());
    assert_no_temporary_files(&paths.cache_dir);
  }

  #[test]
  fn not_ready_errors_match_python_messages_and_a_failed_attempt_can_retry() {
    let state = Mutex::new(ProvisioningState::default());
    assert_eq!(get_ready_model_paths_from(&state).unwrap_err().to_string(), "speech models are still downloading");

    provision_models_with(&state, || anyhow::bail!("synthetic provisioning error"));
    assert_eq!(
      get_ready_model_paths_from(&state).unwrap_err().to_string(),
      "speech models are not ready: synthetic provisioning error"
    );
    assert!(!state.lock().expect("state lock").started);
  }
}

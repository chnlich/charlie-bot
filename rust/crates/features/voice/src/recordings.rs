//! Recording runs: one stem per run, the run's `.json` parts, and the recent recordings list.
//!
//! The browser makes a run id (eight lowercase hex characters) at the click, and the startup report, the relay
//! socket and the upload all carry it. Every file of the run lands under one stem, `<UTC stamp>_<run id>`,
//! whichever request arrives first. The first write creates the stem, and the later ones resolve it by the run id
//! suffix. The `.json` beside the WAV holds the run's three parts. Each part is updated only by its writer
//! through [`with_run_stem`], which serializes the writes per stem and replaces the file atomically.
//!
//! [`list_recordings`] answers the newest rows first, with the duration and the levels read from each row's WAV at
//! request time. Each request reads the sessions directory and the `voice/` directory of every session the session
//! index names. The first request of a home builds that index with one probe per session, and [`with_run_stem`]
//! names its session in the index, so a recording this server stores appears on the next request.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use charliebot_core::config::CharlieBotConfig;
use charliebot_store::{layout, metadata};
use nix::errno::Errno;
use nix::fcntl::AtFlags;
use nix::sys::stat::fstatat;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::{Map, Value, json};

use crate::archive;
use crate::wav;

/// The run id the browser makes at the click: eight lowercase hex characters.
pub(crate) fn valid_run_id(run_id: &str) -> bool {
  run_id.len() == 8 && run_id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The lock that serializes one run's writes, keyed by the voice directory and the run id. It is held across the
/// stem resolution and the writes, so two near-simultaneous writers resolve and fill the same stem.
fn run_lock(voice_dir: &Path, run_id: &str) -> Arc<Mutex<()>> {
  static LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
  let mut locks = LOCKS.lock().expect("the run lock registry is not poisoned");
  locks.entry(format!("{}\0{run_id}", voice_dir.display())).or_default().clone()
}

/// The session ids that hold a `voice/` directory, per sessions directory.
///
/// The first list request of a home builds the set with one probe per session; later requests read only the
/// sessions it names. [`with_run_stem`] names its session under the same lock, so a recording this server stores
/// appears on the next request. No other process writes a voice directory, so the set names every session the list
/// can answer.
static VOICE_SESSIONS: LazyLock<Mutex<HashMap<PathBuf, BTreeSet<String>>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

/// The session ids that hold a `voice/` directory under *sessions_dir*, built by one probe per session when the
/// index for that directory is not there yet.
fn voice_sessions(sessions_dir: &Path, sessions_root: &fs::File) -> io::Result<BTreeSet<String>> {
  let mut index = VOICE_SESSIONS.lock().expect("the voice session index is not poisoned");
  if let Some(sessions) = index.get(sessions_dir) {
    return Ok(sessions.clone());
  }
  let mut sessions = BTreeSet::new();
  for session in fs::read_dir(sessions_dir)? {
    let session = session?;
    // The store reads its sessions as the directory entries of the sessions directory (`list_all_run_records_sync`
    // in run_records.rs), so the files at its root, such as `.counter`, are store metadata. Like that scan, this
    // reads `DirEntry::file_type`, which does not follow a symlink.
    if !session.file_type()?.is_dir() {
      continue;
    }
    let name = session.file_name();
    let session_id = name.to_str().ok_or_else(|| {
      io::Error::other(format!("sessions directory holds a non-UTF-8 name {}", name.to_string_lossy()))
    })?;
    if voice_entry_exists(sessions_root, session_id)? {
      sessions.insert(session_id.to_string());
    }
  }
  index.insert(sessions_dir.to_path_buf(), sessions.clone());
  Ok(sessions)
}

/// Records that *session_id* holds a `voice/` directory under *sessions_dir*. An index that is not built yet stays
/// unbuilt, because the scan that builds it finds the directory too.
fn note_voice_session(sessions_dir: &Path, session_id: &str) {
  let mut index = VOICE_SESSIONS.lock().expect("the voice session index is not poisoned");
  if let Some(sessions) = index.get_mut(sessions_dir) {
    sessions.insert(session_id.to_string());
  }
}

/// The stem of one run's files: the existing stem for `run_id`, or a fresh one this call creates.
///
/// The caller holds the run's lock, so two resolvers never race. The resolution scans the directory for the run
/// id's suffix on every file kind the run writes, including the `.wav.partial` and `.json.tmp` forms a crash can
/// leave behind. Two different stems for one run id is a state this module never creates, so it is an error.
fn resolve_stem(voice_dir: &Path, run_id: &str) -> io::Result<PathBuf> {
  const SUFFIXES: [&str; 5] = [".wav", ".txt", ".json", ".wav.partial", ".json.tmp"];
  let run_suffix = format!("_{run_id}");
  let mut stamps: Vec<String> = Vec::new();
  let entries = match fs::read_dir(voice_dir) {
    Ok(entries) => entries,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(voice_dir.join(run_stem_now(run_id))),
    Err(error) => return Err(error),
  };
  for entry in entries {
    let name = entry?.file_name().into_string().map_err(|name| {
      io::Error::other(format!("voice directory holds a non-UTF-8 file name {}", name.to_string_lossy()))
    })?;
    for suffix in SUFFIXES {
      if let Some(stamp) = name.strip_suffix(suffix).and_then(|stem| stem.strip_suffix(run_suffix.as_str())) {
        stamps.push(stamp.to_string());
      }
    }
  }
  stamps.sort_unstable();
  stamps.dedup();
  match stamps.as_slice() {
    [] => Ok(voice_dir.join(run_stem_now(run_id))),
    [stamp] => Ok(voice_dir.join(format!("{stamp}_{run_id}"))),
    several => Err(io::Error::other(format!(
      "run id {run_id} already spans {} stems in {}: {}",
      several.len(),
      voice_dir.display(),
      several.join(", ")
    ))),
  }
}

/// The stem of a run whose first file this call writes: the current UTC stamp and the run id, the existing stem
/// format whose suffix is the run id.
fn run_stem_now(run_id: &str) -> String {
  archive::stamp_prefix() + &format!("_{run_id}")
}

/// The write path for one run's stem, which runs `write` after resolving the stem. The run id is validated before
/// any path is built, and the stem is resolved under the run's lock. The writes are then serialized against every
/// other writer of the same run.
pub(crate) fn with_run_stem<T>(
  cfg: &CharlieBotConfig,
  session_id: &str,
  run_id: &str,
  write: impl FnOnce(&Path) -> io::Result<T>,
) -> io::Result<T> {
  if !valid_run_id(run_id) {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("run id {} is not eight lowercase hex characters", text_lossy(run_id)),
    ));
  }
  let sessions_dir = cfg.sessions_dir();
  let voice_dir = sessions_dir.join(session_id).join("voice");
  let lock = run_lock(&voice_dir, run_id);
  let _held = lock.lock().expect("the run lock is not poisoned");
  let stem = resolve_stem(&voice_dir, run_id)?;
  fs::create_dir_all(&voice_dir)?;
  note_voice_session(&sessions_dir, session_id);
  write(&stem)
}

/// The stem path with one file kind's extension appended. The stem name carries dots, so the extension is
/// appended, never replaced: `with_extension` would cut the run id off the name.
fn with_kind(stem: &Path, kind: &str) -> PathBuf {
  let mut name = stem.as_os_str().to_owned();
  name.push(".");
  name.push(kind);
  PathBuf::from(name)
}

/// The run's `.json`: the parts read from the stem, or a fresh file's shape when none exists.
fn read_parts(stem: &Path) -> io::Result<Map<String, Value>> {
  let path = with_kind(stem, "json");
  let text = match fs::read_to_string(&path) {
    Ok(text) => text,
    Err(error) if error.kind() == io::ErrorKind::NotFound => {
      let run_id = stem_run_id(stem);
      let mut parts = Map::new();
      parts.insert("run_id".to_string(), json!(run_id));
      parts.insert("audio".to_string(), Value::Null);
      parts.insert("startup".to_string(), Value::Null);
      parts.insert("failure".to_string(), Value::Null);
      return Ok(parts);
    }
    Err(error) => return Err(error),
  };
  serde_json::from_str(&text)
    .map_err(|error| io::Error::other(format!("{} does not hold one JSON object: {error}", path.display())))
}

/// The run id of a stem: the stem's suffix after the last `_`.
fn stem_run_id(stem: &Path) -> String {
  stem
    .file_name()
    .and_then(|name| name.to_str())
    .and_then(|name| name.rsplit_once('_'))
    .map(|(_, run_id)| run_id.to_string())
    .unwrap_or_default()
}

/// Replaces the run's `.json` atomically: the new text lands on a temporary file beside it, then renames over it.
fn write_parts(stem: &Path, parts: &Map<String, Value>) -> io::Result<()> {
  let path = with_kind(stem, "json");
  let text = Value::Object(parts.clone()).to_string();
  let temporary = with_kind(stem, "json.tmp");
  fs::write(&temporary, text.as_bytes())?;
  fs::rename(&temporary, &path)
}

/// The run's `.json` update through `update`: read and replaced under the caller's run lock.
fn update_parts(stem: &Path, update: impl FnOnce(&mut Map<String, Value>)) -> io::Result<()> {
  let mut parts = read_parts(stem)?;
  update(&mut parts);
  write_parts(stem, &parts)
}

/// The run's `audio` part: which path stored the WAV, the transcription backend, and the five device fields.
fn audio_part(kind: &str, backend: Option<&str>, devices: Option<&Value>) -> Value {
  json!({ "path": kind, "backend": backend, "devices": devices })
}

/// The atomic WAV write of one run's stem: the bytes land on the `.wav.partial`, then rename over the WAV.
fn write_run_wav(stem: &Path, pcm_bytes: &[u8]) -> io::Result<()> {
  let wav = with_kind(stem, "wav");
  let partial = with_kind(stem, "wav.partial");
  archive::write_wav(&partial, pcm_bytes)?;
  fs::rename(&partial, &wav)
}

/// The startup report of one run, persisted as its `startup` part.
pub(crate) fn write_startup_part(
  cfg: &CharlieBotConfig,
  session_id: &str,
  run_id: &str,
  startup: Value,
) -> io::Result<()> {
  with_run_stem(cfg, session_id, run_id, |stem| {
    update_parts(stem, |parts| {
      parts.insert("startup".to_string(), startup.clone());
    })?;
    Ok(())
  })
}

/// One upload's WAV and its `audio` part, persisted under the run's stem and answering the WAV path.
pub(crate) fn store_upload_audio(
  cfg: &CharlieBotConfig,
  session_id: &str,
  run_id: &str,
  pcm_bytes: &[u8],
  backend: Option<&str>,
  devices: Option<&Value>,
) -> io::Result<PathBuf> {
  with_run_stem(cfg, session_id, run_id, |stem| {
    write_run_wav(stem, pcm_bytes)?;
    update_parts(stem, |parts| {
      parts.insert("audio".to_string(), audio_part("upload", backend, devices));
    })?;
    Ok(with_kind(stem, "wav"))
  })
}

/// One upload transcription failure, recorded as the run's `failure` part. The WAV the upload persisted stays.
pub(crate) fn write_upload_failure(
  cfg: &CharlieBotConfig,
  session_id: &str,
  run_id: &str,
  error: &str,
) -> io::Result<()> {
  with_run_stem(cfg, session_id, run_id, |stem| {
    update_parts(stem, |parts| {
      parts.insert("failure".to_string(), json!({ "stage": "upload", "error": error }));
    })?;
    Ok(())
  })
}

/// The archive of one successful upload: the transcript beside the WAV, and the failure part cleared — a
/// successful retry of a failed upload owns the row again.
pub(crate) fn write_upload_transcript(
  cfg: &CharlieBotConfig,
  session_id: &str,
  run_id: &str,
  text: &str,
) -> io::Result<()> {
  with_run_stem(cfg, session_id, run_id, |stem| {
    fs::write(with_kind(stem, "txt"), text.as_bytes())?;
    update_parts(stem, |parts| {
      parts.insert("failure".to_string(), Value::Null);
    })?;
    Ok(())
  })
}

/// The archive of one successful relay final: the WAV, the transcript and the run's `audio` part, under one stem.
/// Answers the WAV path for the `voice_transcribed` line.
pub(crate) fn archive_relay_final(
  cfg: &CharlieBotConfig,
  session_id: &str,
  run_id: &str,
  pcm_bytes: &[u8],
  text: &str,
  backend: &str,
  devices: Option<&Value>,
) -> io::Result<PathBuf> {
  with_run_stem(cfg, session_id, run_id, |stem| {
    write_run_wav(stem, pcm_bytes)?;
    fs::write(with_kind(stem, "txt"), text.as_bytes())?;
    update_parts(stem, |parts| {
      parts.insert("audio".to_string(), audio_part("relay", Some(backend), devices));
    })?;
    Ok(with_kind(stem, "wav"))
  })
}

/// The failure record of one relay: the run's `failure` part. The audio received so far becomes the run's WAV
/// when there is any, and its `audio` part names the relay, the backend and the devices.
pub(crate) fn store_relay_failure(
  cfg: &CharlieBotConfig,
  session_id: &str,
  run_id: &str,
  pcm_bytes: &[u8],
  backend: &str,
  devices: Option<&Value>,
  error: &str,
) -> io::Result<()> {
  with_run_stem(cfg, session_id, run_id, |stem| {
    if !pcm_bytes.is_empty() {
      write_run_wav(stem, pcm_bytes)?;
    }
    update_parts(stem, |parts| {
      if !pcm_bytes.is_empty() {
        parts.insert("audio".to_string(), audio_part("relay", Some(backend), devices));
      }
      parts.insert("failure".to_string(), json!({ "stage": "relay", "error": error }));
    })?;
    Ok(())
  })
}

// --- Recent recordings list --------------------------------------------------

/// The default `limit` of `GET /api/voice/recordings`.
pub(crate) const DEFAULT_LIST_LIMIT: usize = 50;
/// The largest `limit` the list route accepts.
pub(crate) const MAX_LIST_LIMIT: usize = 200;

/// The path characters the `audio_url` encodes: controls and the characters a URL's path treats as structure.
fn path_encoding() -> &'static AsciiSet {
  static ENCODING: LazyLock<AsciiSet> = LazyLock::new(|| {
    CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'?').add(b'`').add(b'<').add(b'>').add(b'[').add(b']')
  });
  &ENCODING
}

/// The `limit` query parameter of the list route: absent means [`DEFAULT_LIST_LIMIT`], anything else a positive
/// integer within [`MAX_LIST_LIMIT`].
pub(crate) fn parse_list_limit(query: &str) -> Result<usize, String> {
  let Some(raw) = query_value(query, "limit") else {
    return Ok(DEFAULT_LIST_LIMIT);
  };
  match raw.parse::<usize>() {
    Ok(limit) if (1..=MAX_LIST_LIMIT).contains(&limit) => Ok(limit),
    _ => Err(format!("limit must be an integer from 1 to {MAX_LIST_LIMIT}, got {}", text_lossy(&raw))),
  }
}

/// The first value of one query parameter, `+` as space, percent-decoded.
pub(crate) fn query_value(query: &str, target: &str) -> Option<String> {
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

/// A stem file's name minus its row-creating extension. Only the `.wav` and the `.json` of a run list it, so a
/// lone `.txt`, a leftover `.wav.partial` or a `.json.tmp` names no row.
fn stem_of_row_name(name: &str) -> Option<&str> {
  for suffix in [".wav", ".json"] {
    if let Some(stem) = name.strip_suffix(suffix) {
      return Some(stem);
    }
  }
  None
}

/// A stem's `"<22-char stamp>_<8 hex>"` shape, as `(stamp, run_id)`. A `.wav` or `.json` whose name does not
/// match is a state the server never writes, so the caller fails the request that saw it.
fn stem_shape(stem: &str) -> Option<(&str, &str)> {
  if stem.len() != 31 || !stem.is_ascii() {
    return None;
  }
  let (stamp, run_id) = stem.split_once('_')?;
  let bytes = stamp.as_bytes();
  let stamp_shaped = bytes.len() == 22
    && [(4usize, b'-'), (7, b'-'), (10, b'T'), (17, b'.'), (21, b'Z')].iter().all(|(at, byte)| bytes[*at] == *byte)
    && [(0usize, 4), (5, 7), (8, 10), (11, 13), (13, 15), (15, 17), (18, 21)]
      .iter()
      .all(|(start, end)| bytes[*start..*end].iter().all(u8::is_ascii_digit));
  (stamp_shaped && valid_run_id(run_id)).then_some((stamp, run_id))
}

/// Whether one session holds a `voice` entry, looked up relative to the open sessions directory.
///
/// The probe answers for the path, not for its kind: the scan's `read_dir` is what rejects an entry that is not a
/// directory. The lookup walks the session's own name under the descriptor instead of the whole sessions path, which
/// a scan would otherwise walk again for every session of the home.
fn voice_entry_exists(sessions: &fs::File, session_id: &str) -> io::Result<bool> {
  match fstatat(sessions, &Path::new(session_id).join("voice"), AtFlags::empty()) {
    Ok(_) => Ok(true),
    Err(Errno::ENOENT) => Ok(false),
    Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
  }
}

/// The most recent recordings across every session directory, newest first, one row per run.
///
/// Each request reads the sessions directory, then the `voice/` directory of every session the session index
/// names; a session without one is skipped, and any other read failure fails the request. The duration and the
/// levels of each row come from its WAV at request time.
pub(crate) fn list_recordings(cfg: &CharlieBotConfig, limit: usize) -> io::Result<Vec<Value>> {
  let sessions_dir = cfg.sessions_dir();
  let mut stems: Vec<(String, String, String)> = Vec::new();
  // The store owns the "no sessions directory is an empty store" convention, and so does this list.
  let sessions = match fs::read_dir(&sessions_dir) {
    Ok(sessions) => sessions,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(error),
  };
  let sessions_root = fs::File::open(&sessions_dir)?;
  let with_voice = voice_sessions(&sessions_dir, &sessions_root)?;
  for session in sessions {
    let session = session?;
    if !session.file_type()?.is_dir() {
      continue;
    }
    let name = session.file_name();
    let session_id = name.to_str().ok_or_else(|| {
      io::Error::other(format!("sessions directory holds a non-UTF-8 name {}", name.to_string_lossy()))
    })?;
    if !with_voice.contains(session_id) {
      continue;
    }
    let voice_dir = session.path().join("voice");
    let entries = match fs::read_dir(&voice_dir) {
      Ok(entries) => entries,
      Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
      Err(error) => return Err(error),
    };
    for entry in entries {
      let name = entry?.file_name().into_string().map_err(|name| {
        io::Error::other(format!("voice directory holds a non-UTF-8 file name {}", name.to_string_lossy()))
      })?;
      let Some(stem) = stem_of_row_name(&name) else { continue };
      let Some((stamp, _)) = stem_shape(stem) else {
        return Err(io::Error::other(format!(
          "{} does not carry a recording stem name",
          voice_dir.join(&name).display()
        )));
      };
      stems.push((stamp.to_string(), session_id.to_string(), stem.to_string()));
    }
  }
  stems.sort_unstable_by(|a, b| {
    (b.0.as_str(), b.1.as_str(), b.2.as_str()).cmp(&(a.0.as_str(), a.1.as_str(), a.2.as_str()))
  });
  // A stem the run's `.wav` and `.json` both carry is one row.
  stems.dedup();
  stems.truncate(limit);
  stems.iter().map(|(_, session_id, stem)| recording_row(&sessions_dir, session_id, stem)).collect()
}

/// One row of the recent recordings list, the fields of the plan's 4.1 in order.
fn recording_row(sessions_dir: &Path, session_id: &str, stem: &str) -> io::Result<Value> {
  let stem_path = sessions_dir.join(session_id).join("voice").join(stem);
  let parts = match fs::metadata(with_kind(&stem_path, "json")) {
    Ok(_) => Some(read_parts(&stem_path)?),
    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
    Err(error) => return Err(error),
  };
  let audio = parts.as_ref().and_then(|parts| parts.get("audio")).filter(|audio| !audio.is_null());
  let startup = parts.as_ref().and_then(|parts| parts.get("startup")).filter(|startup| !startup.is_null());
  let failure = parts.as_ref().and_then(|parts| parts.get("failure")).filter(|failure| !failure.is_null());
  let devices = audio.and_then(|audio| audio.get("devices")).filter(|devices| !devices.is_null());
  let (audio_url, duration_s, digital_silence, peak_dbfs, rms_dbfs) = match fs::metadata(with_kind(&stem_path, "wav")) {
    Ok(_) => {
      let bytes = fs::read(with_kind(&stem_path, "wav"))?;
      let (duration_s, digital_silence, peak_dbfs, rms_dbfs) = wav_levels(&bytes)?;
      // The encoded path keeps its leading `/`, which the file route's mount supplies.
      let url = utf8_percent_encode(&with_kind(&stem_path, "wav").to_string_lossy(), path_encoding()).to_string();
      (Some(format!("/absolute_filepath{url}")), Some(duration_s), Some(digital_silence), peak_dbfs, rms_dbfs)
    }
    Err(error) if error.kind() == io::ErrorKind::NotFound => (None, None, None, None, None),
    Err(error) => return Err(error),
  };
  let transcript = match fs::read_to_string(with_kind(&stem_path, "txt")) {
    Ok(text) => Some(text),
    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
    Err(error) => return Err(error),
  };
  let (stamp, _) = stem_shape(stem).expect("the scan validated the stem shape");
  Ok(json!({
    "session_id": session_id,
    (metadata::SESSION_NAME_FIELD): session_name(&sessions_dir.join(session_id))?,
    "stem": stem,
    "recorded_at": recorded_at(stamp),
    "audio_url": audio_url,
    "duration_s": duration_s,
    "digital_silence": digital_silence,
    "peak_dbfs": peak_dbfs,
    "rms_dbfs": rms_dbfs,
    "transcript": transcript,
    "failure": failure,
    "audio_path_kind": audio.and_then(|audio| audio.get("path")),
    "backend": audio.and_then(|audio| audio.get("backend")),
    "input_device": devices.and_then(|devices| devices.get("input_device")),
    "capture_settings": devices.and_then(|devices| devices.get("capture_settings")),
    "reached": startup.and_then(|startup| startup.get("reached")),
    "end": startup.and_then(|startup| startup.get("end")),
    "mic_permission": startup.and_then(|startup| startup.get("mic_permission")),
    "startup_error": startup.and_then(|startup| startup.get("error")),
  }))
}

/// A session's name from the metadata the store owns; a session without its metadata file names no row name.
fn session_name(session_dir: &Path) -> io::Result<Option<String>> {
  match metadata::read_metadata_object(&layout::metadata_path(session_dir)) {
    Ok(Some(object)) => Ok(object.get("name").and_then(Value::as_str).map(str::to_string)),
    Ok(None) => Ok(None),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(io::Error::other(error.to_string())),
  }
}

/// The stem's UTC stamp as `2026-11-07T08:58:43.611Z`. The scan validated the stamp's shape.
fn recorded_at(stamp: &str) -> String {
  format!(
    "{}-{}-{}T{}:{}:{}{}Z",
    &stamp[0..4],
    &stamp[5..7],
    &stamp[8..10],
    &stamp[11..13],
    &stamp[13..15],
    &stamp[15..17],
    &stamp[17..21]
  )
}

/// One WAV's duration, digital-silence flag and levels: the peak and the RMS of every sample, in dBFS. A WAV that
/// holds no non-zero sample is digital silence with no levels, and so is an empty one.
fn wav_levels(bytes: &[u8]) -> io::Result<(f64, bool, Option<f64>, Option<f64>)> {
  let wave = wav::read(bytes).map_err(|error| io::Error::other(format!("recording WAV does not parse: {error}")))?;
  if wave.sample_width != 2 {
    return Err(io::Error::other(format!("recording WAV holds {}-bit samples", wave.sample_width * 8)));
  }
  let frame_bytes = usize::from(wave.channels) * 2;
  let frames = wave.frames.len() / frame_bytes;
  // The peak is an i32: `i16::MIN`'s magnitude is one past i16's range.
  let mut peak: i32 = 0;
  let mut sum_squares = 0_u128;
  let mut samples = 0_u128;
  for chunk in wave.frames.as_chunks::<2>().0 {
    let sample = i16::from_le_bytes(*chunk);
    peak = peak.max(i32::from(sample).abs());
    sum_squares += u128::from(sample.unsigned_abs()) * u128::from(sample.unsigned_abs());
    samples += 1;
  }
  let duration = round(frames as f64 / f64::from(wave.frame_rate), 2);
  if samples == 0 || peak == 0 {
    return Ok((duration, true, None, None));
  }
  let full_scale = 32768.0_f64;
  let peak_dbfs = round(20.0 * (f64::from(peak) / full_scale).log10(), 1);
  let rms = (sum_squares as f64 / samples as f64).sqrt();
  let rms_dbfs = round(20.0 * (rms / full_scale).log10(), 1);
  Ok((duration, false, Some(peak_dbfs), Some(rms_dbfs)))
}

fn round(value: f64, decimals: u32) -> f64 {
  let factor = 10_f64.powi(decimals as i32);
  (value * factor).round() / factor + 0.0
}

fn text_lossy(text: &str) -> String {
  String::from_utf8_lossy(text.as_bytes()).into_owned()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn synthetic_config(home: &Path) -> CharlieBotConfig {
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("features.voice", crate::register).expect("register the voice config section");
    registry.load_config(home).expect("load synthetic config")
  }

  fn voice_dir(cfg: &CharlieBotConfig, session_id: &str) -> PathBuf {
    cfg.sessions_dir().join(session_id).join("voice")
  }

  fn run_id(seed: u8) -> String {
    format!("{seed:08x}")
  }

  #[test]
  fn run_ids_beyond_eight_lowercase_hex_are_invalid() {
    assert!(valid_run_id("0a1b2c3d"));
    assert!(valid_run_id("ffffffff"));
    assert!(!valid_run_id("0A1B2C3D"), "uppercase is not the browser's form");
    assert!(!valid_run_id("0a1b2c3"), "seven characters");
    assert!(!valid_run_id("0a1b2c3de"), "nine characters");
    assert!(!valid_run_id("0a1b2c3g"), "g is not hex");
    assert!(!valid_run_id(""), "empty");
  }

  #[test]
  fn an_invalid_run_id_is_refused_before_any_path_is_built() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let error = with_run_stem(&config, "synthetic-session", "../escape", |_stem| Ok(())).expect_err("refused");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(!home.path().join("sessions").exists(), "no path was built from the invalid id");
  }

  #[test]
  fn the_first_write_creates_the_stem_and_the_rest_land_under_it() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let id = run_id(1);
    write_startup_part(&config, "synthetic-session", &id, json!({"reached": "ready"}))
      .expect("the startup part creates the stem");
    store_upload_audio(&config, "synthetic-session", &id, &[1, 2, 3], Some("gemini"), None)
      .expect("the upload lands under the same stem");
    write_upload_failure(&config, "synthetic-session", &id, "synthetic failure")
      .expect("the failure part lands under the same stem");
    let mut files: Vec<String> = fs::read_dir(voice_dir(&config, "synthetic-session"))
      .expect("read the voice directory")
      .map(|entry| entry.expect("read a voice entry").file_name().to_string_lossy().into_owned())
      .collect();
    files.sort();
    assert_eq!(files.len(), 2, "one stem holds the wav and the json: {files:?}");
    let json = files.iter().find(|name| name.ends_with(".json")).expect("the json exists");
    let (stamp, suffix) = json.trim_end_matches(".json").split_once('_').expect("the stem joins stamp and run id");
    assert_eq!(suffix, id);
    assert_eq!(stamp.len(), 22, "the stamp keeps the stem format: {stamp}");
    let parts: Value = serde_json::from_str(
      &fs::read_to_string(voice_dir(&config, "synthetic-session").join(json)).expect("read the parts"),
    )
    .expect("the parts parse");
    assert_eq!(parts["run_id"], id.as_str());
    assert_eq!(parts["startup"], json!({"reached": "ready"}));
    assert_eq!(parts["audio"]["path"], "upload");
    assert_eq!(parts["failure"]["error"], "synthetic failure");
  }

  #[tokio::test]
  async fn near_simultaneous_writers_of_one_run_lose_nothing() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = Arc::new(synthetic_config(home.path()));
    let id = run_id(2);
    let startup = {
      let config = Arc::clone(&config);
      let id = id.clone();
      tokio::task::spawn_blocking(move || write_startup_part(&config, "synthetic-session", &id, json!({"end": "stop"})))
    };
    let audio = {
      let config = Arc::clone(&config);
      let id = id.clone();
      tokio::task::spawn_blocking(move || store_upload_audio(&config, "synthetic-session", &id, &[1, 2], None, None))
    };
    let (startup, audio) = tokio::join!(startup, audio);
    startup.expect("the startup write joins").expect("the startup write succeeds");
    audio.expect("the audio write joins").expect("the audio write succeeds");
    let json_name = fs::read_dir(voice_dir(&config, "synthetic-session"))
      .expect("read the voice directory")
      .map(|entry| entry.expect("read a voice entry").file_name().to_string_lossy().into_owned())
      .find(|name| name.ends_with(".json"))
      .expect("the json exists");
    let parts: Value = serde_json::from_str(
      &fs::read_to_string(voice_dir(&config, "synthetic-session").join(&json_name)).expect("read the parts"),
    )
    .expect("the parts parse");
    assert_eq!(parts["startup"], json!({"end": "stop"}));
    assert_eq!(parts["audio"]["path"], "upload");
    assert_eq!(
      fs::read_dir(voice_dir(&config, "synthetic-session")).expect("read the voice directory").count(),
      2,
      "one stem holds the wav and the json"
    );
  }

  #[tokio::test]
  async fn every_arrival_order_of_startup_relay_and_upload_lands_under_one_stem() {
    for orders in [[0usize, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
      let home = tempfile::tempdir().expect("make an isolated home");
      let config = synthetic_config(home.path());
      let id = run_id(3);
      for step in orders {
        match step {
          0 => {
            write_startup_part(&config, "synthetic-session", &id, json!({"reached": "ready"}))
              .expect("the startup part writes");
          }
          1 => {
            archive_relay_final(&config, "synthetic-session", &id, &[1, 2, 3], "relay words", "gemini", None)
              .expect("the relay final archives");
          }
          _ => {
            store_upload_audio(&config, "synthetic-session", &id, &[4, 5, 6], Some("gemini"), None)
              .expect("the upload stores");
          }
        }
      }
      let files: Vec<String> = fs::read_dir(voice_dir(&config, "synthetic-session"))
        .expect("read the voice directory")
        .map(|entry| entry.expect("read a voice entry").file_name().to_string_lossy().into_owned())
        .collect();
      assert_eq!(files.len(), 3, "order {orders:?} lands under one stem: {files:?}");
      let parts: Value = serde_json::from_str(
        &fs::read_to_string(
          files
            .iter()
            .find(|name| name.ends_with(".json"))
            .map(|name| voice_dir(&config, "synthetic-session").join(name))
            .expect("the json exists"),
        )
        .expect("read the parts"),
      )
      .expect("the parts parse");
      assert_eq!(parts["startup"], json!({"reached": "ready"}), "order {orders:?}");
      // The audio part names whichever audio writer came last: the upload, or the relay final.
      let last_audio = orders.iter().rev().find(|step| **step != 0).expect("every order holds an audio writer");
      let expected = if *last_audio == 2 { "upload" } else { "relay" };
      assert_eq!(parts["audio"]["path"], expected, "the last audio writer owns the part: order {orders:?}");
      assert_eq!(
        fs::read_to_string(
          files
            .iter()
            .find(|name| name.ends_with(".txt"))
            .map(|name| voice_dir(&config, "synthetic-session").join(name))
            .expect("the txt exists")
        )
        .expect("read the transcript"),
        "relay words",
        "order {orders:?}"
      );
    }
  }

  #[test]
  fn a_relay_failure_records_its_reason_and_the_audio_received_so_far() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let id = run_id(4);
    let devices = json!({
      "input_device": "Synthetic mic",
      "capture_settings": {"echoCancellation": true},
      "output_device": null,
      "communications_output_device": null,
      "device_error": null
    });
    store_relay_failure(&config, "synthetic-session", &id, &[1, 2, 3], "gemini", Some(&devices), "connection lost")
      .expect("the failure archives");
    let files: Vec<String> = fs::read_dir(voice_dir(&config, "synthetic-session"))
      .expect("read the voice directory")
      .map(|entry| entry.expect("read a voice entry").file_name().to_string_lossy().into_owned())
      .collect();
    assert_eq!(files.len(), 2, "the failure archives the partial wav and the json: {files:?}");
    let wav = files.iter().find(|name| name.ends_with(".wav")).expect("the partial wav exists");
    let wav_bytes = fs::read(voice_dir(&config, "synthetic-session").join(wav)).expect("read the wav");
    let wave = wav::read(&wav_bytes).expect("parses");
    // Three bytes are one whole 16-bit frame and a trailing half frame the WAV reader cuts.
    assert_eq!(wave.frames, &[1, 2]);
    let json = files.iter().find(|name| name.ends_with(".json")).expect("the json exists");
    let parts: Value =
      serde_json::from_str(&fs::read_to_string(voice_dir(&config, "synthetic-session").join(json)).expect("read"))
        .expect("parse");
    assert_eq!(parts["failure"], json!({"stage": "relay", "error": "connection lost"}));
    assert_eq!(parts["audio"], json!({"path": "relay", "backend": "gemini", "devices": devices}));
  }

  #[test]
  fn a_relay_failure_without_audio_records_only_the_json() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let id = run_id(5);
    store_relay_failure(&config, "synthetic-session", &id, &[], "gemini", None, "connection lost")
      .expect("the failure records");
    let files: Vec<String> = fs::read_dir(voice_dir(&config, "synthetic-session"))
      .expect("read the voice directory")
      .map(|entry| entry.expect("read a voice entry").file_name().to_string_lossy().into_owned())
      .collect();
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with(".json"), "no WAV is stored when no audio arrived: {files:?}");
    let parts: Value =
      serde_json::from_str(&fs::read_to_string(voice_dir(&config, "synthetic-session").join(&files[0])).expect("read"))
        .expect("parse");
    assert_eq!(parts["failure"], json!({"stage": "relay", "error": "connection lost"}));
    assert_eq!(parts["audio"], Value::Null);
  }

  #[test]
  fn a_retry_upload_overwrites_its_wav_and_its_audio_part() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let id = run_id(6);
    store_upload_audio(&config, "synthetic-session", &id, &[1, 1, 1], Some("gemini"), None)
      .expect("the first upload stores");
    write_upload_failure(&config, "synthetic-session", &id, "first failure").expect("the failure records");
    store_upload_audio(&config, "synthetic-session", &id, &[2, 2, 2, 2], Some("muse"), None)
      .expect("the retry overwrites");
    write_upload_transcript(&config, "synthetic-session", &id, "retry words").expect("the transcript writes");
    let files: Vec<String> = fs::read_dir(voice_dir(&config, "synthetic-session"))
      .expect("read the voice directory")
      .map(|entry| entry.expect("read a voice entry").file_name().to_string_lossy().into_owned())
      .collect();
    assert_eq!(files.len(), 3, "one run, one stem: {files:?}");
    let wav = files.iter().find(|name| name.ends_with(".wav")).expect("the wav exists");
    let wav_path = voice_dir(&config, "synthetic-session").join(wav);
    let wav_bytes = fs::read(&wav_path).expect("read the wav");
    let wave = wav::read(&wav_bytes).expect("the wav parses");
    assert_eq!(wave.frames, &[2, 2, 2, 2], "the retry's audio replaced the first attempt's");
    let json = files.iter().find(|name| name.ends_with(".json")).expect("the json exists");
    let parts: Value =
      serde_json::from_str(&fs::read_to_string(voice_dir(&config, "synthetic-session").join(json)).expect("read"))
        .expect("parse");
    assert_eq!(parts["audio"], json!({"path": "upload", "backend": "muse", "devices": null}));
    assert_eq!(parts["failure"], Value::Null, "a successful retry owns the row again");
    assert_eq!(
      fs::read_to_string(voice_dir(&config, "synthetic-session").join(json).with_extension("txt"))
        .expect("read the transcript"),
      "retry words"
    );
  }

  #[test]
  fn the_list_answers_newest_first_across_sessions_within_the_limit() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let old_stem = "2026-11-01T100000.000Z_000000aa";
    let new_stem = "2026-11-07T090000.000Z_000000bb";
    let older_stem = "2026-10-31T100000.000Z_000000cc";
    for (session, stem) in [("session-a", old_stem), ("session-b", new_stem), ("session-a", older_stem)] {
      let voice = voice_dir(&config, session);
      fs::create_dir_all(&voice).expect("create the voice directory");
      archive::write_wav(&voice.join(format!("{stem}.wav")), &[1, 2]).expect("write a synthetic wav");
    }
    let rows = list_recordings(&config, 2).expect("the list answers");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["stem"], new_stem);
    assert_eq!(rows[0]["session_id"], "session-b");
    assert_eq!(rows[1]["stem"], old_stem);
    let rows = list_recordings(&config, DEFAULT_LIST_LIMIT).expect("the list answers");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2]["stem"], older_stem);
  }

  #[test]
  fn a_recording_stored_after_a_list_appears_on_the_next_list() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    fs::create_dir_all(config.sessions_dir()).expect("create the sessions directory");
    assert!(list_recordings(&config, DEFAULT_LIST_LIMIT).expect("an empty store lists nothing").is_empty());
    let id = run_id(7);
    store_upload_audio(&config, "synthetic-session", &id, &[1, 2], Some("gemini"), None).expect("the upload stores");

    let rows = list_recordings(&config, DEFAULT_LIST_LIMIT).expect("the list answers");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["session_id"], "synthetic-session");
  }

  #[test]
  fn the_list_answers_empty_when_no_sessions_directory_exists() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());

    let rows = list_recordings(&config, DEFAULT_LIST_LIMIT).expect("a store without sessions lists no recordings");

    assert!(rows.is_empty());
  }

  #[test]
  fn the_list_skips_the_store_root_files_beside_the_sessions() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let sessions = config.sessions_dir();
    fs::create_dir_all(&sessions).expect("create the sessions directory");
    fs::write(sessions.join(".counter"), "2").expect("write the store's session counter");
    fs::write(sessions.join("session_aliases.json"), "{}").expect("write the store's alias map");
    let voice = voice_dir(&config, "synthetic-session");
    fs::create_dir_all(&voice).expect("create the voice directory");
    archive::write_wav(&voice.join("2026-11-01T100000.000Z_000000aa.wav"), &[1, 2]).expect("write a synthetic wav");

    let rows = list_recordings(&config, DEFAULT_LIST_LIMIT).expect("the store's root files are not sessions");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["session_id"], "synthetic-session");
    assert_eq!(rows[0]["stem"], "2026-11-01T100000.000Z_000000aa");
  }

  #[test]
  fn a_voice_entry_that_is_not_a_directory_fails_the_list() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let session = config.sessions_dir().join("synthetic-session");
    fs::create_dir_all(&session).expect("create the session directory");
    fs::write(session.join("voice"), b"synthetic").expect("write a voice file");

    let error = list_recordings(&config, DEFAULT_LIST_LIMIT).expect_err("a voice file is a read failure");

    assert_eq!(error.kind(), io::ErrorKind::NotADirectory, "{error}");
  }

  #[test]
  fn a_pre_change_recording_lists_with_null_json_fields_and_its_levels() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let voice = voice_dir(&config, "synthetic-session");
    fs::create_dir_all(&voice).expect("create the voice directory");
    // 16000 samples of a full-scale square wave: 1.00 s at 0.0 dBFS peak.
    let mut pcm = Vec::new();
    for index in 0..16_000_i16 {
      let sample = if index % 2 == 0 { i16::MAX } else { i16::MIN };
      pcm.extend_from_slice(&sample.to_le_bytes());
    }
    archive::write_wav(&voice.join("2026-11-01T100000.000Z_000000dd.wav"), &pcm).expect("write a synthetic wav");
    fs::write(voice.join("2026-11-01T100000.000Z_000000dd.txt"), "old words").expect("write a synthetic transcript");
    fs::write(voice.join("2026-11-01T100000.000Z_000000ee.txt"), "a lone transcript names no row")
      .expect("write an orphan transcript");

    let rows = list_recordings(&config, DEFAULT_LIST_LIMIT).expect("the list answers");

    assert_eq!(rows.len(), 1, "the orphan transcript is not a row");
    let row = &rows[0];
    assert_eq!(row["stem"], "2026-11-01T100000.000Z_000000dd");
    assert_eq!(row["recorded_at"], "2026-11-01T10:00:00.000Z");
    assert!(row["audio_url"].as_str().expect("the wav answers a url").starts_with("/absolute_filepath/"));
    assert_eq!(row["duration_s"], 1.0);
    assert_eq!(row["digital_silence"], false);
    assert_eq!(row["peak_dbfs"], 0.0);
    assert_eq!(row["transcript"], "old words");
    for field in
      ["failure", "audio_path_kind", "backend", "input_device", "reached", "end", "mic_permission", "startup_error"]
    {
      assert_eq!(row[field], Value::Null, "{field} of a pre-change recording");
    }
  }

  #[test]
  fn an_all_zero_wav_lists_as_digital_silence_without_levels() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let voice = voice_dir(&config, "synthetic-session");
    fs::create_dir_all(&voice).expect("create the voice directory");
    archive::write_wav(&voice.join("2026-11-01T100000.000Z_000000ff.wav"), &[0_u8; 3200]).expect("write silence");

    let rows = list_recordings(&config, DEFAULT_LIST_LIMIT).expect("the list answers");

    assert_eq!(rows[0]["duration_s"], 0.1);
    assert_eq!(rows[0]["digital_silence"], true);
    for field in ["peak_dbfs", "rms_dbfs"] {
      assert_eq!(rows[0][field], Value::Null, "{field} of digital silence");
    }
    assert!(rows[0]["audio_url"].is_string(), "digital silence still plays");
  }

  #[test]
  fn a_run_without_audio_lists_its_startup_parts_without_a_player() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let id = run_id(0x11);
    write_startup_part(
      &config,
      "synthetic-session",
      &id,
      json!({"reached": "mic_opening", "end": "startup_error", "error": "mic: NotAllowedError", "mic_permission": "denied"}),
    )
    .expect("the startup part writes");

    let rows = list_recordings(&config, DEFAULT_LIST_LIMIT).expect("the list answers");

    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    for field in ["audio_url", "duration_s", "digital_silence", "peak_dbfs", "rms_dbfs", "transcript"] {
      assert_eq!(row[field], Value::Null, "{field} of a run without audio");
    }
    assert_eq!(row["reached"], "mic_opening");
    assert_eq!(row["end"], "startup_error");
    assert_eq!(row["startup_error"], "mic: NotAllowedError");
    assert_eq!(row["mic_permission"], "denied");
  }

  #[test]
  fn the_row_carries_the_session_name_and_the_audio_part_fields() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let session = config.sessions_dir().join("synthetic-session");
    fs::create_dir_all(session.join("voice")).expect("create the session");
    fs::write(
      session.join("metadata.json"),
      r#"{"id": "synthetic-session", "name": "Synthetic Session Name", "profile": "manager", "schema_version": 2}"#,
    )
    .expect("write the session metadata");
    let id = run_id(0x12);
    store_upload_audio(
      &config,
      "synthetic-session",
      &id,
      &[1, 2],
      Some("gemini"),
      Some(&json!({
        "input_device": "Synthetic mic",
        "capture_settings": {"echoCancellation": true},
        "output_device": null,
        "communications_output_device": null,
        "device_error": null
      })),
    )
    .expect("the upload stores");

    let rows = list_recordings(&config, DEFAULT_LIST_LIMIT).expect("the list answers");

    assert_eq!(rows[0]["session_name"], "Synthetic Session Name");
    assert_eq!(rows[0]["audio_path_kind"], "upload");
    assert_eq!(rows[0]["backend"], "gemini");
    assert_eq!(rows[0]["input_device"], "Synthetic mic");
    assert_eq!(rows[0]["capture_settings"], json!({"echoCancellation": true}));
  }

  #[test]
  fn a_wav_or_json_whose_name_is_not_a_stem_fails_the_list() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let voice = voice_dir(&config, "synthetic-session");
    fs::create_dir_all(&voice).expect("create the voice directory");
    fs::write(voice.join("not-a-stem.wav"), b"synthetic").expect("write a stray wav");
    let error = list_recordings(&config, DEFAULT_LIST_LIMIT).expect_err("the stray name fails the list");
    assert!(error.to_string().contains("not-a-stem.wav"), "{error}");
  }

  #[test]
  fn the_list_limit_parses_the_query_parameter() {
    assert_eq!(parse_list_limit("").expect("no query defaults"), DEFAULT_LIST_LIMIT);
    assert_eq!(parse_list_limit("limit=7").expect("a valid limit"), 7);
    assert!(parse_list_limit("limit=0").is_err());
    assert!(parse_list_limit("limit=201").is_err());
    assert!(parse_list_limit("limit=abc").is_err());
    assert!(parse_list_limit("limit=-1").is_err());
  }

  #[test]
  fn the_query_reader_takes_the_last_value_and_decodes() {
    assert_eq!(query_value("limit=7&limit=8", "limit").as_deref(), Some("8"));
    assert_eq!(query_value("other=x", "limit"), None);
    assert_eq!(query_value("limit=a+b", "limit").as_deref(), Some("a b"));
  }

  #[test]
  fn wav_levels_measure_the_peak_and_the_rms_in_dbfs() {
    // One full-scale sample followed by 15999 silence samples: peak 0.0 dBFS, RMS -42.0 dBFS.
    let mut pcm = i16::MAX.to_le_bytes().to_vec();
    pcm.extend(std::iter::repeat_n(0_i16.to_le_bytes(), 15_999).flatten());
    let directory = tempfile::tempdir().expect("make a scratch directory");
    let path = directory.path().join("synthetic.wav");
    archive::write_wav(&path, &pcm).expect("write a synthetic wav");
    let (duration, silence, peak, rms) =
      wav_levels(&fs::read(path).expect("read the synthetic wav")).expect("measures");
    assert_eq!(duration, 1.0);
    assert!(!silence);
    assert_eq!(peak, Some(0.0));
    assert_eq!(rms, Some(-42.0));
  }

  #[test]
  fn a_non_pcm16_wav_fails_the_measurement() {
    let error = wav_levels(b"synthetic invalid wav body").expect_err("the body fails");
    assert!(error.to_string().contains("does not parse"), "{error}");
  }
}

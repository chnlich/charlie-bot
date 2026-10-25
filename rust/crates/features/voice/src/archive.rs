use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use charliebot_core::config::CharlieBotConfig;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audio::SAMPLE_RATE;

pub(crate) fn archive_voice_pair(
  cfg: &CharlieBotConfig,
  session_id: &str,
  pcm_bytes: &[u8],
  text: &str,
) -> io::Result<PathBuf> {
  archive_voice_pair_with_writer(cfg, session_id, pcm_bytes, text, write_wav)
}

fn archive_voice_pair_with_writer<F>(
  cfg: &CharlieBotConfig,
  session_id: &str,
  pcm_bytes: &[u8],
  text: &str,
  write_audio: F,
) -> io::Result<PathBuf>
where
  F: FnOnce(&Path, &[u8]) -> io::Result<()>,
{
  let voice_dir = cfg.sessions_dir().join(session_id).join("voice");
  fs::create_dir_all(&voice_dir)?;
  let audio_path = voice_dir.join(format!("{}.wav", voice_stem()));
  let transcript_path = audio_path.with_extension("txt");
  let partial_path =
    voice_dir.join(format!("{}.wav.partial", audio_path.file_stem().expect("voice stem exists").to_string_lossy()));
  let result = (|| {
    fs::write(&transcript_path, text.as_bytes())?;
    write_audio(&partial_path, pcm_bytes)?;
    fs::rename(&partial_path, &audio_path)?;
    Ok(())
  })();
  if let Err(error) = result {
    let partial_cleanup = remove_if_exists(&partial_path);
    let transcript_cleanup = remove_if_exists(&transcript_path);
    partial_cleanup?;
    transcript_cleanup?;
    return Err(error);
  }
  Ok(audio_path)
}

pub(crate) fn write_wav(path: &Path, pcm_bytes: &[u8]) -> io::Result<()> {
  let data_length = u32::try_from(pcm_bytes.len()).map_err(|_| io::Error::other("voice recording is too large"))?;
  let riff_length = 36_u32.checked_add(data_length).ok_or_else(|| io::Error::other("voice recording is too large"))?;
  let mut wav = fs::File::create(path)?;
  wav.write_all(b"RIFF")?;
  wav.write_all(&riff_length.to_le_bytes())?;
  wav.write_all(b"WAVEfmt ")?;
  wav.write_all(&16_u32.to_le_bytes())?;
  wav.write_all(&1_u16.to_le_bytes())?;
  wav.write_all(&1_u16.to_le_bytes())?;
  wav.write_all(&(SAMPLE_RATE as u32).to_le_bytes())?;
  wav.write_all(&((SAMPLE_RATE * 2) as u32).to_le_bytes())?;
  wav.write_all(&2_u16.to_le_bytes())?;
  wav.write_all(&16_u16.to_le_bytes())?;
  wav.write_all(b"data")?;
  wav.write_all(&data_length.to_le_bytes())?;
  wav.write_all(pcm_bytes)
}

/// Persist an upload's recording as `sessions/{id}/voice/<stem>.wav` before it is decoded, and return its path.
pub(crate) fn persist_recording(cfg: &CharlieBotConfig, session_id: &str, pcm_bytes: &[u8]) -> io::Result<PathBuf> {
  let voice_dir = cfg.sessions_dir().join(session_id).join("voice");
  fs::create_dir_all(&voice_dir)?;
  let audio_path = voice_dir.join(format!("{}.wav", voice_stem()));
  write_wav(&audio_path, pcm_bytes)?;
  Ok(audio_path)
}

/// The transcript beside a recording: the `.txt` with the recording's stem, UTF-8.
pub(crate) fn write_transcript(audio_path: &Path, text: &str) -> io::Result<()> {
  fs::write(audio_path.with_extension("txt"), text.as_bytes())
}

/// `datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H%M%S.%f")[:-3] + "Z"`: UTC to the millisecond, then `Z`.
fn voice_stem_format() -> Vec<time::format_description::FormatItem<'static>> {
  time::macros::format_description!("[year]-[month]-[day]T[hour][minute][second].[subsecond digits:3]Z").to_vec()
}

fn voice_stem() -> String {
  let now = OffsetDateTime::now_utc();
  let random = Uuid::new_v4().simple().to_string();
  format!("{}_{}", now.format(&voice_stem_format()).expect("the stem format renders"), &random[..8])
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
  match fs::remove_file(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error),
  }
}

#[cfg(test)]
fn wav_bytes(pcm_bytes: &[u8]) -> Vec<u8> {
  let path = std::env::temp_dir().join(format!("voice-wav-{}.wav", Uuid::new_v4()));
  write_wav(&path, pcm_bytes).expect("write a synthetic wav");
  let bytes = fs::read(&path).expect("read a synthetic wav");
  fs::remove_file(path).expect("remove a synthetic wav");
  bytes
}

#[cfg(test)]
mod tests {
  use super::*;

  fn synthetic_config(home: &Path) -> CharlieBotConfig {
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("src.features.voice", crate::register).expect("register the voice config section");
    registry.load_config(home).expect("load synthetic config")
  }

  #[test]
  fn voice_stem_matches_the_python_stem_shape() {
    let stem = voice_stem();
    let (stamp, random) = stem.split_once('_').expect("the stem joins the stamp and the random suffix");
    assert_eq!(random.len(), 8);
    assert!(random.chars().all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()), "stem {stem}");
    // `strftime("%Y-%m-%dT%H%M%S.%f")[:-3] + "Z"`: 4-digit year, dashes, `T`, six digits, millisecond fraction, `Z`.
    assert_eq!(stamp.len(), 22, "the stamp is YYYY-MM-DDTHHMMSS.mmmZ: {stamp}");
    let bytes = stamp.as_bytes();
    for (index, expected) in [(4, b'-'), (7, b'-'), (10, b'T'), (17, b'.'), (21, b'Z')] {
      assert_eq!(bytes[index], expected, "stamp {stamp}");
    }
    for (start, end) in [(0usize, 4), (5, 7), (8, 10), (11, 13), (13, 15), (15, 17), (18, 21)] {
      assert!(bytes[start..end].iter().all(u8::is_ascii_digit), "stamp {stamp}");
    }
  }

  #[test]
  fn wav_bytes_match_python_wave_for_even_and_odd_pcm_lengths() {
    assert_eq!(wav_bytes(&[]), include_bytes!("../tests/fixtures/wav-empty.wav"));
    assert_eq!(wav_bytes(&[1]), include_bytes!("../tests/fixtures/wav-one-byte.wav"));
    assert_eq!(wav_bytes(&[1, 2]), include_bytes!("../tests/fixtures/wav-two-bytes.wav"));
    assert_eq!(wav_bytes(&[1, 2, 3]), include_bytes!("../tests/fixtures/wav-three-bytes.wav"));
    assert_eq!(wav_bytes(&[1, 2, 3, 4, 5]), include_bytes!("../tests/fixtures/wav-five-bytes.wav"));
  }

  #[test]
  fn voice_pair_publishes_the_transcript_and_odd_length_wav() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let audio_path = archive_voice_pair(&config, "synthetic-session", &[1, 2, 3], "synthetic transcript")
      .expect("publish the voice pair");
    assert_eq!(
      std::fs::read(&audio_path).expect("read the published WAV"),
      include_bytes!("../tests/fixtures/wav-three-bytes.wav")
    );
    assert_eq!(
      std::fs::read_to_string(audio_path.with_extension("txt")).expect("read the transcript"),
      "synthetic transcript"
    );
    assert_eq!(
      std::fs::read_dir(audio_path.parent().expect("WAV has a parent")).expect("read archive directory").count(),
      2
    );
    let stem = audio_path.file_stem().expect("WAV has a stem").to_string_lossy();
    let (timestamp, suffix) = stem.rsplit_once('_').expect("stem has a UUID suffix");
    assert_eq!(suffix.len(), 8);
    assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(timestamp.len(), 22);
    assert!(timestamp.ends_with('Z'));
  }

  #[test]
  fn archive_failure_removes_the_transcript_and_partial_wav() {
    let home = tempfile::tempdir().expect("make an isolated home");
    let config = synthetic_config(home.path());
    let voice_dir = config.sessions_dir().join("synthetic-session").join("voice");
    let error =
      archive_voice_pair_with_writer(&config, "synthetic-session", &[1, 2], "synthetic transcript", |_path, _bytes| {
        Err(io::Error::other("synthetic write failure"))
      })
      .expect_err("failed WAV write propagates");
    assert_eq!(error.to_string(), "synthetic write failure");
    assert_eq!(std::fs::read_dir(voice_dir).expect("read cleanup directory").count(), 0);
  }
}

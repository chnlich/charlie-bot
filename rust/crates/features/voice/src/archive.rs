use std::fs;
use std::io::{self, Write};
use std::path::Path;

use time::OffsetDateTime;

use crate::audio::SAMPLE_RATE;

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

/// The timestamp prefix of a recording stem. It is UTC to the millisecond, then `Z`, in the format
/// `datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H%M%S.%f")[:-3] + "Z"`.
/// A recording stem appends `_` and the run id the browser made at the click.
fn voice_stem_format() -> Vec<time::format_description::FormatItem<'static>> {
  time::macros::format_description!("[year]-[month]-[day]T[hour][minute][second].[subsecond digits:3]Z").to_vec()
}

/// The current UTC stamp, the prefix a fresh recording stem carries.
pub(crate) fn stamp_prefix() -> String {
  OffsetDateTime::now_utc().format(&voice_stem_format()).expect("the stem format renders")
}

#[cfg(test)]
fn wav_bytes(pcm_bytes: &[u8]) -> Vec<u8> {
  let directory = tempfile::tempdir().expect("make a scratch directory");
  let path = directory.path().join("synthetic.wav");
  write_wav(&path, pcm_bytes).expect("write a synthetic wav");
  fs::read(&path).expect("read a synthetic wav")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_stamp_prefix_matches_the_stem_shape() {
    let stamp = stamp_prefix();
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
  fn wav_bytes_match_wave_for_even_and_odd_pcm_lengths() {
    assert_eq!(wav_bytes(&[]), include_bytes!("../tests/fixtures/wav-empty.wav"));
    assert_eq!(wav_bytes(&[1]), include_bytes!("../tests/fixtures/wav-one-byte.wav"));
    assert_eq!(wav_bytes(&[1, 2]), include_bytes!("../tests/fixtures/wav-two-bytes.wav"));
    assert_eq!(wav_bytes(&[1, 2, 3]), include_bytes!("../tests/fixtures/wav-three-bytes.wav"));
    assert_eq!(wav_bytes(&[1, 2, 3, 4, 5]), include_bytes!("../tests/fixtures/wav-five-bytes.wav"));
  }
}

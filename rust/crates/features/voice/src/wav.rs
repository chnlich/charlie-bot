//! The WAV format the voice routes read and write.
//!
//! A body is accepted when it is a RIFF/WAVE container whose `fmt ` chunk precedes its `data`
//! chunk, with PCM or extensible PCM samples. Frames are the data chunk's bytes up to its declared size, cut to whole
//! frames. The writer lays the same container out from the PCM16 mono frames the upload route stores.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::audio::SAMPLE_RATE;

/// The RIFF container and its two chunks. A reader and a writer that disagree on one of them refuse or misread the
/// file silently, so each identifier is spelled here once.
const RIFF_ID: &[u8; 4] = b"RIFF";
const WAVE_ID: &[u8; 4] = b"WAVE";
const FMT_ID: &[u8; 4] = b"fmt ";
const DATA_ID: &[u8; 4] = b"data";

/// The bytes a RIFF/WAVE header holds before the samples: `WAVE`, the `fmt ` chunk's identifier, size and body, and
/// the `data` chunk's identifier and size.
const HEADER_BYTES: u32 = 36;

/// The `fmt ` chunk's declared body size: the fixed PCM format fields.
const FMT_CHUNK_BYTES: u32 = 16;

/// The upload route stores 16-bit mono samples at [`SAMPLE_RATE`].
const SAMPLE_WIDTH: u16 = 2;
const CHANNELS: u16 = 1;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
// KSDATAFORMAT_SUBTYPE_PCM, the extensible format's sub-format GUID in its little-endian byte order.
const KSDATAFORMAT_SUBTYPE_PCM: [u8; 16] =
  [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];

/// A parsed WAV body: its format and the frames of its data chunk.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WaveData<'a> {
  pub(crate) channels: u16,
  pub(crate) sample_width: u16,
  pub(crate) frame_rate: u32,
  pub(crate) frames: &'a [u8],
}

/// The chunk header at `position`: its four-byte name, declared size and the offset of its body.
struct ChunkHeader {
  name: [u8; 4],
  size: usize,
  body: usize,
}

fn chunk_header(body: &[u8], position: usize) -> Option<ChunkHeader> {
  let name = body.get(position..position + 4)?.try_into().ok()?;
  let size = u32::from_le_bytes(body.get(position + 4..position + 8)?.try_into().ok()?) as usize;
  Some(ChunkHeader { name, size, body: position + 8 })
}

/// A WAV body parsed into its format and the data chunk's frames.
pub(crate) fn read(body: &[u8]) -> Result<WaveData<'_>, String> {
  let riff = chunk_header(body, 0).ok_or("truncated WAV header")?;
  if &riff.name != RIFF_ID {
    return Err("file does not start with RIFF id".to_string());
  }
  if body.get(8..12) != Some(&WAVE_ID[..]) {
    return Err("not a WAVE file".to_string());
  }
  let mut format = None;
  let mut position = 12;
  while let Some(chunk) = chunk_header(body, position) {
    let end = chunk.body.saturating_add(chunk.size).min(body.len());
    let chunk_body = &body[chunk.body..end];
    if &chunk.name == FMT_ID {
      format = Some(read_format(chunk_body)?);
    } else if &chunk.name == DATA_ID {
      let (channels, sample_width, frame_rate) = format.ok_or_else(|| "data chunk before fmt chunk".to_string())?;
      let frame_size = usize::from(channels) * usize::from(sample_width);
      let whole_frames = chunk.size / frame_size * frame_size;
      let frames = &body[chunk.body..body.len().min(chunk.body + whole_frames)];
      return Ok(WaveData { channels, sample_width, frame_rate, frames });
    }
    // A chunk whose declared size is odd is followed by one pad byte, and so is every chunk the walker skips.
    position = chunk.body.saturating_add(chunk.size).saturating_add(chunk.size & 1);
  }
  Err("fmt chunk and/or data chunk missing".to_string())
}

/// Write `pcm_bytes` as the 16-bit mono WAV the upload route stores: a `fmt ` chunk and a `data` chunk.
pub(crate) fn write(path: &Path, pcm_bytes: &[u8]) -> io::Result<()> {
  let data_length = u32::try_from(pcm_bytes.len()).map_err(|_| io::Error::other("voice recording is too large"))?;
  let riff_length =
    HEADER_BYTES.checked_add(data_length).ok_or_else(|| io::Error::other("voice recording is too large"))?;
  let block_align = CHANNELS * SAMPLE_WIDTH;
  let byte_rate = SAMPLE_RATE as u32 * u32::from(block_align);
  let mut wav = fs::File::create(path)?;
  wav.write_all(RIFF_ID)?;
  wav.write_all(&riff_length.to_le_bytes())?;
  wav.write_all(WAVE_ID)?;
  wav.write_all(FMT_ID)?;
  wav.write_all(&FMT_CHUNK_BYTES.to_le_bytes())?;
  wav.write_all(&WAVE_FORMAT_PCM.to_le_bytes())?;
  wav.write_all(&CHANNELS.to_le_bytes())?;
  wav.write_all(&(SAMPLE_RATE as u32).to_le_bytes())?;
  wav.write_all(&byte_rate.to_le_bytes())?;
  wav.write_all(&block_align.to_le_bytes())?;
  wav.write_all(&(SAMPLE_WIDTH * 8).to_le_bytes())?;
  wav.write_all(DATA_ID)?;
  wav.write_all(&data_length.to_le_bytes())?;
  wav.write_all(pcm_bytes)
}

/// The `fmt ` chunk's channel count, sample width in bytes and frame rate.
fn read_format(chunk: &[u8]) -> Result<(u16, u16, u32), String> {
  let tag = le_u16(chunk, 0)?;
  let channels = le_u16(chunk, 2)?;
  let frame_rate = u32::from_le_bytes(le_bytes(chunk, 4)?);
  let bits = le_u16(chunk, 14)?;
  if tag != WAVE_FORMAT_PCM && tag != WAVE_FORMAT_EXTENSIBLE {
    return Err(format!("unknown format: {tag}"));
  }
  if tag == WAVE_FORMAT_EXTENSIBLE && chunk.get(24..40) != Some(&KSDATAFORMAT_SUBTYPE_PCM[..]) {
    return Err("unknown extended format".to_string());
  }
  let sample_width = bits.div_ceil(8);
  if sample_width == 0 {
    return Err("bad sample width".to_string());
  }
  if channels == 0 {
    return Err("bad # of channels".to_string());
  }
  Ok((channels, sample_width, frame_rate))
}

fn le_bytes<const N: usize>(chunk: &[u8], at: usize) -> Result<[u8; N], String> {
  chunk.get(at..at + N).and_then(|bytes| bytes.try_into().ok()).ok_or_else(|| "truncated fmt chunk".to_string())
}

fn le_u16(chunk: &[u8], at: usize) -> Result<u16, String> {
  Ok(u16::from_le_bytes(le_bytes(chunk, at)?))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The bytes `write` lays down for `pcm_bytes`, read back from a scratch file.
  fn wav_bytes(pcm_bytes: &[u8]) -> Vec<u8> {
    let directory = tempfile::tempdir().expect("make a scratch directory");
    let path = directory.path().join("synthetic.wav");
    write(&path, pcm_bytes).expect("write a synthetic wav");
    fs::read(&path).expect("read a synthetic wav")
  }

  #[test]
  fn written_bytes_match_the_fixture_for_even_and_odd_pcm_lengths() {
    assert_eq!(wav_bytes(&[]), include_bytes!("../tests/fixtures/wav-empty.wav"));
    assert_eq!(wav_bytes(&[1]), include_bytes!("../tests/fixtures/wav-one-byte.wav"));
    assert_eq!(wav_bytes(&[1, 2]), include_bytes!("../tests/fixtures/wav-two-bytes.wav"));
    assert_eq!(wav_bytes(&[1, 2, 3]), include_bytes!("../tests/fixtures/wav-three-bytes.wav"));
    assert_eq!(wav_bytes(&[1, 2, 3, 4, 5]), include_bytes!("../tests/fixtures/wav-five-bytes.wav"));
  }

  #[test]
  fn fixture_bodies_read_back_as_wave_reads_them() {
    // The three-byte fixture holds one whole 16-bit mono frame; the odd trailing byte is not a frame.
    let wave = read(include_bytes!("../tests/fixtures/wav-three-bytes.wav")).expect("a fixture WAV parses");
    assert_eq!(wave, WaveData { channels: 1, sample_width: 2, frame_rate: 16_000, frames: &[1, 2] });
  }

  #[test]
  fn a_body_that_is_not_a_wave_file_is_refused() {
    assert_eq!(read(b"synthetic invalid WAV body\n"), Err("file does not start with RIFF id".to_string()));
  }
}

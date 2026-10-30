//! The WAV reader of Python's `wave` module, as the upload route uses it.
//!
//! A body is accepted where `wave.open` accepts it: a RIFF/WAVE container whose `fmt ` chunk precedes its `data`
//! chunk, with PCM or extensible PCM samples. Frames are the data chunk's bytes up to its declared size, cut to whole
//! frames, as `readframes(getnframes())` returns them.

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

/// Parse a WAV body the way `wave.open` reads it, returning the format and the data chunk's frames.
pub(crate) fn read(body: &[u8]) -> Result<WaveData<'_>, String> {
  let riff = chunk_header(body, 0).ok_or("truncated WAV header")?;
  if &riff.name != b"RIFF" {
    return Err("file does not start with RIFF id".to_string());
  }
  if body.get(8..12) != Some(&b"WAVE"[..]) {
    return Err("not a WAVE file".to_string());
  }
  let mut format = None;
  let mut position = 12;
  while let Some(chunk) = chunk_header(body, position) {
    let end = chunk.body.saturating_add(chunk.size).min(body.len());
    let chunk_body = &body[chunk.body..end];
    match &chunk.name {
      b"fmt " => format = Some(read_format(chunk_body)?),
      b"data" => {
        let (channels, sample_width, frame_rate) = format.ok_or_else(|| "data chunk before fmt chunk".to_string())?;
        let frame_size = usize::from(channels) * usize::from(sample_width);
        let whole_frames = chunk.size / frame_size * frame_size;
        let frames = &body[chunk.body..body.len().min(chunk.body + whole_frames)];
        return Ok(WaveData { channels, sample_width, frame_rate, frames });
      }
      _ => {}
    }
    // A chunk whose declared size is odd is followed by one pad byte, and so is every chunk `wave` skips.
    position = chunk.body.saturating_add(chunk.size).saturating_add(chunk.size & 1);
  }
  Err("fmt chunk and/or data chunk missing".to_string())
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

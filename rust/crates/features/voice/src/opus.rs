//! Opus packets of a WebM or Ogg file, decoded by libopus 1.3.1 translated to Rust (`unsafe-libopus`).
//!
//! The crate builds with cargo alone, so the file path needs no system libopus. libopus decodes at 48 kHz whatever the
//! input rate was, and the caller resamples the output. The trims follow RFC 7845: the OpusHead pre-skip comes off the
//! start, and the demuxer's end trim comes off the last packet.

use symphonia::core::formats::FormatReader;
use unsafe_libopus::{OpusDecoder, opus_decode_float, opus_decoder_create, opus_decoder_destroy};

/// The rate libopus decodes at.
pub(crate) const OPUS_RATE: u32 = 48_000;
/// The most frames one packet holds per channel: 120 ms, the longest Opus packet.
const MAX_FRAMES_PER_PACKET: usize = 5_760;
/// The OpusHead field that holds the pre-skip: a little-endian u16 at byte 10 (RFC 7845, section 5.1).
const PRE_SKIP_AT: usize = 10;

/// The pre-skip frames of an OpusHead, the identification header a track keeps as its extra data.
pub(crate) fn pre_skip(opus_head: Option<&[u8]>) -> Result<usize, String> {
  let head = opus_head.ok_or("Opus track has no OpusHead")?;
  let bytes = head.get(PRE_SKIP_AT..PRE_SKIP_AT + 2).ok_or("OpusHead is truncated")?;
  Ok(usize::from(u16::from_le_bytes([bytes[0], bytes[1]])))
}

/// Decode every packet of `track_id` to mono samples at 48 kHz, without the pre-skip and the end trim.
pub(crate) fn decode_track(
  format: &mut dyn FormatReader,
  track_id: u32,
  channels: usize,
  pre_skip: usize,
) -> Result<Vec<f32>, String> {
  let mut decoder = PacketDecoder::new(channels)?;
  let mut pcm = vec![0.0; MAX_FRAMES_PER_PACKET * channels];
  let mut samples = Vec::new();
  let mut to_skip = pre_skip;
  while let Some(packet) = format.next_packet().map_err(|error| error.to_string())? {
    // An empty packet carries no frames; decoding it would conceal a lost packet instead.
    if packet.track_id != track_id || packet.data.is_empty() {
      continue;
    }
    let frames = decoder.decode(&packet.data, &mut pcm)?;
    let end_trim = usize::try_from(packet.trim_end.get()).map_err(|_| "Opus end trim overflows")?;
    for frame in pcm[..frames.saturating_sub(end_trim) * channels].chunks_exact(channels) {
      if to_skip > 0 {
        to_skip -= 1;
        continue;
      }
      samples.push(frame.iter().sum::<f32>() / channels as f32);
    }
  }
  Ok(samples)
}

/// One libopus decoder, freed when dropped.
struct PacketDecoder {
  raw: *mut OpusDecoder,
}

impl PacketDecoder {
  fn new(channels: usize) -> Result<Self, String> {
    let mut error = 0;
    // SAFETY: `error` is a valid out-parameter for the call, and the decoder is only used through `decode` and freed
    // by `Drop`.
    let raw = unsafe { opus_decoder_create(OPUS_RATE as i32, channels as i32, &mut error) };
    if error != 0 || raw.is_null() {
      return Err(format!("opus_decoder_create failed with error {error}"));
    }
    Ok(Self { raw })
  }

  /// Decode one packet into `pcm` as interleaved frames, returning the frames per channel.
  fn decode(&mut self, packet: &[u8], pcm: &mut [f32]) -> Result<usize, String> {
    let len = i32::try_from(packet.len()).map_err(|_| "Opus packet is too long")?;
    // SAFETY: `packet` is readable for `len` bytes, and `pcm` holds `MAX_FRAMES_PER_PACKET` frames of `channels`
    // samples, which is the frame size passed to libopus.
    let frames =
      unsafe { opus_decode_float(self.raw, packet.as_ptr(), len, pcm.as_mut_ptr(), MAX_FRAMES_PER_PACKET as i32, 0) };
    usize::try_from(frames).map_err(|_| format!("opus_decode_float failed with error {frames}"))
  }
}

impl Drop for PacketDecoder {
  fn drop(&mut self) {
    // SAFETY: `raw` came from `opus_decoder_create` and is destroyed once, here.
    unsafe { opus_decoder_destroy(self.raw) }
  }
}

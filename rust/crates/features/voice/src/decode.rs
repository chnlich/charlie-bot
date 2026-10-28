//! Decode one audio file to the 16 kHz mono PCM16 that the recording transcription takes.
//!
//! Symphonia reads the container and the codec. Opus packets go to `opus`, and the container's own priming and
//! padding go to `edit_list` (MP4) and `matroska` (WebM). A `.pcm` or `.s16` file is already in the target format and
//! passes through unchanged.

use std::fs;
use std::io::Cursor;
use std::path::Path;

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::default::{get_codecs, get_probe};

use crate::edit_list::{self, AudioEdit};
use crate::{matroska, opus};

/// The rate the transcription backend takes.
pub(crate) const TARGET_RATE: u32 = 16_000;
/// Input frames per resampler chunk.
const RESAMPLE_CHUNK_FRAMES: usize = 1024;

/// Decoded mono audio at `rate`, with the container's priming and padding already removed.
struct Mono {
  samples: Vec<f32>,
  rate: u32,
}

/// The file as 16 kHz mono PCM16, little-endian: the bytes `transcribe_recording` sends to the backend.
pub(crate) fn load_pcm(path: &Path) -> Result<Vec<u8>, String> {
  if is_raw_pcm(path) {
    return fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()));
  }
  let audio = decode(path)?;
  let samples = resample(&audio.samples, audio.rate)?;
  Ok(samples.iter().flat_map(|sample| to_s16(*sample).to_le_bytes()).collect())
}

fn is_raw_pcm(path: &Path) -> bool {
  let extension = path.extension().and_then(|extension| extension.to_str()).unwrap_or_default();
  matches!(extension.to_ascii_lowercase().as_str(), "pcm" | "s16")
}

fn decode(path: &Path) -> Result<Mono, String> {
  let bytes = fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
  let edit = edit_list::audio_edit(&bytes)?;
  let discard_ns = matroska::last_discard_padding_ns(&bytes)?;
  let mut hint = Hint::new();
  if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
    hint.with_extension(extension);
  }
  let stream = MediaSourceStream::new(Box::new(Cursor::new(bytes)), MediaSourceStreamOptions::default());
  let mut format = get_probe()
    .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
    .map_err(|error| format!("{}: {error}", path.display()))?;
  let track = format.default_track(TrackType::Audio).ok_or_else(|| format!("{}: no audio track", path.display()))?;
  let track_id = track.id;
  let Some(CodecParameters::Audio(params)) = track.codec_params.clone() else {
    return Err(format!("{}: the audio track has no codec parameters", path.display()));
  };
  let rate = params.sample_rate.ok_or_else(|| format!("{}: the audio track has no sample rate", path.display()))?;

  if params.codec == CODEC_ID_OPUS {
    let channels = params
      .channels
      .as_ref()
      .map(|channels| channels.count())
      .ok_or_else(|| format!("{}: the Opus track has no channel layout", path.display()))?;
    let pre_skip = opus::pre_skip(params.extra_data.as_deref())?;
    let mut samples = opus::decode_track(format.as_mut(), track_id, channels, pre_skip)?;
    if let Some(ns) = discard_ns {
      let frames = usize::try_from(ns * i64::from(opus::OPUS_RATE) / 1_000_000_000)
        .map_err(|_| format!("{}: the Opus discard padding is negative", path.display()))?;
      samples.truncate(samples.len().saturating_sub(frames));
    }
    return Ok(Mono { samples, rate: opus::OPUS_RATE });
  }

  let mut decoder = get_codecs()
    .make_audio_decoder(&params, &AudioDecoderOptions::default())
    .map_err(|error| format!("{}: {error}", path.display()))?;
  let mut planes: Vec<Vec<f32>> = Vec::new();
  let mut samples = Vec::new();
  while let Some(packet) = format.next_packet().map_err(|error| format!("{}: {error}", path.display()))? {
    if packet.track_id != track_id {
      continue;
    }
    let decoded = decoder.decode(&packet).map_err(|error| format!("{}: {error}", path.display()))?;
    decoded.copy_to_vecs_planar(&mut planes);
    push_mono(&planes, &mut samples);
  }
  trim_mp4_edit(&mut samples, rate, edit)?;
  Ok(Mono { samples, rate })
}

/// Keep the frames that an MP4 edit list plays: it starts at `media_time` and runs for its duration. A file without an
/// edit list has no priming to cut; symphonia already applies the MP3 and Ogg priming in its decoders.
fn trim_mp4_edit(samples: &mut Vec<f32>, rate: u32, edit: Option<AudioEdit>) -> Result<(), String> {
  let Some(edit) = edit else {
    return Ok(());
  };
  let scale = u128::from(edit.movie_timescale);
  let kept = (u128::from(edit.duration) * u128::from(rate) + scale / 2) / scale;
  let start = usize::try_from(edit.media_time).map_err(|_| "MP4 edit starts past the addressable frames")?;
  samples.truncate(start.saturating_add(kept as usize).min(samples.len()));
  samples.drain(..start.min(samples.len()));
  Ok(())
}

/// Append each frame's mean over the channels. A mono track is copied as it is.
fn push_mono(planes: &[Vec<f32>], out: &mut Vec<f32>) {
  let frames = planes.first().map_or(0, Vec::len);
  let scale = 1.0 / planes.len() as f32;
  for frame in 0..frames {
    out.push(planes.iter().map(|plane| plane[frame]).sum::<f32>() * scale);
  }
}

/// Resample with an FFT resampler. Its delay is a whole number of frames at both rates, so the output starts on the
/// input's first frame; a sinc resampler truncates a fractional delay and shifts the output by up to one frame.
fn resample(samples: &[f32], rate: u32) -> Result<Vec<f32>, String> {
  if rate == TARGET_RATE {
    return Ok(samples.to_vec());
  }
  let mut resampler = Fft::<f32>::new(rate as usize, TARGET_RATE as usize, RESAMPLE_CHUNK_FRAMES, 1, FixedSync::Both)
    .map_err(|error| error.to_string())?;
  let input = vec![samples.to_vec()];
  let input_adapter = SequentialSliceOfVecs::new(&input, 1, samples.len()).map_err(|error| error.to_string())?;
  let needed = resampler.process_all_needed_output_len(samples.len());
  let mut output = vec![vec![0.0_f32; needed]];
  let produced = {
    let mut output_adapter =
      SequentialSliceOfVecs::new_mut(&mut output, 1, needed).map_err(|error| error.to_string())?;
    let (_, produced) = resampler
      .process_all_into_buffer(&input_adapter, &mut output_adapter, samples.len(), None)
      .map_err(|error| error.to_string())?;
    produced
  };
  let mut mono = output.swap_remove(0);
  mono.truncate(produced);
  Ok(mono)
}

/// The sample as signed 16-bit PCM, rounded and clipped as libswresample converts it.
fn to_s16(sample: f32) -> i16 {
  (sample * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use sha2::{Digest, Sha256};

  use super::*;

  /// A fixture clip and the PyAV reference the decoder must match: the reference's sample count and SHA-256, as
  /// recorded when the reference was made (`tests/fixtures/audio/README.md`).
  struct Clip {
    file: &'static str,
    samples: usize,
    sha256: &'static str,
  }

  const WAV: Clip = Clip {
    file: "tone.wav",
    samples: 8000,
    sha256: "ec21be46363aa2c372dd757b6a59aad530051610caf8e760ac413e01ede7cbfc",
  };
  const MP4_AAC: Clip = Clip {
    file: "tone-aac.m4a",
    samples: 7984,
    sha256: "5a85953387149209f739cdaa0975907587e8c3fbb167a875a6895f81be28b0ac",
  };
  const MP3: Clip = Clip {
    file: "tone-mp3.mp3",
    samples: 7984,
    sha256: "3a67031f58141675c0d5e5f06cd069497da58bea6d63f141a9f7c06616cdd677",
  };
  const OGG_VORBIS: Clip = Clip {
    file: "tone-vorbis.ogg",
    samples: 7937,
    sha256: "b014585f32f590a56437cc044582037fa7ede7e2f0a3f032aa32ed43ebad575e",
  };
  const WEBM_OPUS: Clip = Clip {
    file: "tone-opus.webm",
    samples: 7984,
    sha256: "2921a54d24cd099c5edfa86acf739331af2bb20253780d846dba9dd06813a2d2",
  };
  const OGG_OPUS: Clip = Clip {
    file: "tone-opus.ogg",
    samples: 7984,
    sha256: "2921a54d24cd099c5edfa86acf739331af2bb20253780d846dba9dd06813a2d2",
  };

  fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/audio").join(name)
  }

  /// Decode the clip, and check its length and its correlation with the PyAV reference.
  fn assert_decodes_like_pyav(clip: &Clip) {
    let reference = fs::read(fixture(&format!("{}.pyav16k.s16", clip.file))).expect("the PyAV reference is committed");
    assert_eq!(format!("{:x}", Sha256::digest(&reference)), clip.sha256, "{} reference changed", clip.file);
    assert_eq!(reference.len(), clip.samples * 2, "{} reference length", clip.file);

    let decoded = to_samples(&load_pcm(&fixture(clip.file)).expect("the clip decodes"));
    let reference = to_samples(&reference);
    let length_error = (decoded.len() as f64 - clip.samples as f64).abs() / clip.samples as f64;
    assert!(length_error <= 0.01, "{}: {} samples against {}", clip.file, decoded.len(), clip.samples);
    let value = correlation(&decoded, &reference);
    assert!(value >= 0.99, "{}: correlation {value}", clip.file);
  }

  fn to_samples(pcm: &[u8]) -> Vec<i16> {
    pcm.as_chunks::<2>().0.iter().map(|pair| i16::from_le_bytes(*pair)).collect()
  }

  /// The Pearson correlation of the overlapping samples.
  fn correlation(decoded: &[i16], reference: &[i16]) -> f64 {
    let n = decoded.len().min(reference.len());
    let x: Vec<f64> = decoded[..n].iter().map(|&sample| f64::from(sample)).collect();
    let y: Vec<f64> = reference[..n].iter().map(|&sample| f64::from(sample)).collect();
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    let (mean_x, mean_y) = (mean(&x), mean(&y));
    let covariance: f64 = x.iter().zip(&y).map(|(a, b)| (a - mean_x) * (b - mean_y)).sum();
    let variance_x: f64 = x.iter().map(|a| (a - mean_x).powi(2)).sum();
    let variance_y: f64 = y.iter().map(|b| (b - mean_y).powi(2)).sum();
    covariance / (variance_x * variance_y).sqrt()
  }

  #[test]
  fn wav_decodes_like_pyav() {
    assert_decodes_like_pyav(&WAV);
  }

  #[test]
  fn mp4_aac_decodes_like_pyav() {
    assert_decodes_like_pyav(&MP4_AAC);
  }

  #[test]
  fn mp3_decodes_like_pyav() {
    assert_decodes_like_pyav(&MP3);
  }

  #[test]
  fn ogg_vorbis_decodes_like_pyav() {
    assert_decodes_like_pyav(&OGG_VORBIS);
  }

  #[test]
  fn webm_opus_decodes_like_pyav() {
    assert_decodes_like_pyav(&WEBM_OPUS);
  }

  #[test]
  fn ogg_opus_decodes_like_pyav() {
    assert_decodes_like_pyav(&OGG_OPUS);
  }
}

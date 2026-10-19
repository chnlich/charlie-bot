//! The offline decode the differential run compares with Python's `transcribe_pcm_offline`.
//!
//! `tests/local_decode_differential.py` drives the `local_decode` example, which loads the bundle once through
//! [`OfflineDecoder::load`] and decodes each PCM file through [`OfflineDecoder::decode`].

use std::path::Path;

use crate::provisioning::voice_model_paths;
use crate::transcriber::{self, SpeechModelBundle};

/// One decode: each window as `(start, end, left, right)` in samples, and the transcript.
pub struct Decoded {
  pub windows: Vec<(usize, usize, usize, usize)>,
  pub text: String,
}

/// The resident sherpa bundle, built from the speech models under `cache_dir`.
pub struct OfflineDecoder {
  bundle: SpeechModelBundle,
}

impl OfflineDecoder {
  pub fn load(cache_dir: &Path) -> anyhow::Result<Self> {
    let paths = voice_model_paths(cache_dir);
    Ok(Self { bundle: transcriber::create_sherpa_bundle(&paths, "")? })
  }

  pub fn decode(&self, pcm: &[u8]) -> anyhow::Result<Decoded> {
    let decoded = transcriber::decode_pcm_offline(&self.bundle, pcm)?;
    let windows = decoded.windows.iter().map(|window| (window.start, window.end, window.left, window.right)).collect();
    Ok(Decoded { windows, text: decoded.text })
  }
}

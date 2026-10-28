# Audio decoder fixtures

One clip per container the file decoder reads, all made from `tone.wav`: a 0.5 s synthetic mix of 220 Hz and 440 Hz
tones, a 600 Hz to 1200 Hz chirp, and a 1800 Hz partial under a raised-cosine fade.

| File | Container and codec | How it was made |
| --- | --- | --- |
| `tone.wav` | WAV, PCM s16le, 16 kHz mono | Written with Python's `wave` module from the synthetic mix (8000 frames). |
| `tone-aac.m4a` | MP4, AAC-LC | PyAV 19.0.2 (installed with `uv`): resample to 44100 Hz mono fltp, native `aac` encoder at 64 kbps, MP4 mux. |
| `tone-mp3.mp3` | MP3 | PyAV 19.0.2: resample to 44100 Hz mono fltp, `libmp3lame` at 64 kbps. |
| `tone-vorbis.ogg` | Ogg, Vorbis | Resampled to 44100 Hz mono float32 with PyAV, then written by libsndfile 1.2.2 (`soundfile` 0.14.0, `format="OGG", subtype="VORBIS"`). The PyAV build here refuses to open its native Vorbis encoder, so the encode step differs from the other clips. |
| `tone-opus.webm` | WebM, Opus | PyAV 19.0.2: resample to 48000 Hz mono s16, `libopus` at 32 kbps, WebM mux. |
| `tone-opus.ogg` | Ogg, Opus | As `tone-opus.webm`, muxed to Ogg. |

Each `<clip>.pyav16k.s16` is the reference: the PCM that the Python voice-notes decoder produced for the clip before it
was ported to `charliebot voice transcribe` (last version at commit 1f1c81015), as raw little-endian int16 at 16 kHz
mono. It is PyAV's decode, resampled by PyAV's `AudioResampler` to `format="s16", layout="mono", rate=16000`, and read
the way that decoder's `load_pcm` read it, with no flush at the end.
The sample count and the SHA-256 of each reference are recorded in the `decode.rs` tests.

# Audio decoder fixtures

One clip per container the file decoder reads, all made from `tone.wav`: a 0.5 s synthetic mix of 220 Hz and 440 Hz
tones, a 600 Hz to 1200 Hz chirp, and a 1800 Hz partial under a raised-cosine fade.

| File | Container and codec | Encoding |
| --- | --- | --- |
| `tone.wav` | WAV, PCM s16le, 16 kHz mono | The synthetic mix as PCM (8000 frames). |
| `tone-aac.m4a` | MP4, AAC-LC | Resampled to 44100 Hz mono fltp, `aac` at 64 kbps, MP4 mux. |
| `tone-mp3.mp3` | MP3 | Resampled to 44100 Hz mono fltp, `libmp3lame` at 64 kbps. |
| `tone-vorbis.ogg` | Ogg, Vorbis | Resampled to 44100 Hz mono float32, then written by libsndfile with `format="OGG", subtype="VORBIS"`. The encoder differs from the other clips' `libavcodec` encoders. |
| `tone-opus.webm` | WebM, Opus | Resampled to 48000 Hz mono s16, `libopus` at 32 kbps, WebM mux. |
| `tone-opus.ogg` | Ogg, Opus | As `tone-opus.webm`, muxed to Ogg. |

Each `<clip>.pyav16k.s16` is the reference: the PCM the recorded decoder produced for the clip, as raw little-endian
int16 at 16 kHz mono — the clip decoded and resampled to s16 mono at 16 kHz, read with no flush at the end.
The sample count and the SHA-256 of each reference are recorded in the `decode.rs` tests.

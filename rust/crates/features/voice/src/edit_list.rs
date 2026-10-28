//! The edit list of an MP4 file's first audio track.
//!
//! symphonia 0.6's isomp4 reader parses the `elst` box but does not apply it, so a decoded AAC track still holds the
//! encoder's priming frames. libavformat applies the box, and PyAV is built on libavformat, so this reader takes the
//! same two numbers: where playback starts in the track, and how long it runs.

const FTYP: &[u8; 4] = b"ftyp";
const MOOV: &[u8; 4] = b"moov";
const MVHD: &[u8; 4] = b"mvhd";
const TRAK: &[u8; 4] = b"trak";
const MDIA: &[u8; 4] = b"mdia";
const HDLR: &[u8; 4] = b"hdlr";
const EDTS: &[u8; 4] = b"edts";
const ELST: &[u8; 4] = b"elst";
const SOUN: &[u8; 4] = b"soun";

/// The single edit of an audio track.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AudioEdit {
  /// The track frame where playback starts, in frames at the track's sample rate.
  pub(crate) media_time: u64,
  /// The playback length, in ticks of `movie_timescale`.
  pub(crate) duration: u64,
  /// Ticks per second of the movie header, the unit of `duration`.
  pub(crate) movie_timescale: u32,
}

/// One box of an ISO BMFF file: its four-character type and its body.
struct Atom<'a> {
  kind: &'a [u8],
  body: &'a [u8],
}

/// The edit of the first audio track, or `None` when the file is not ISO BMFF or that track has no edit list.
///
/// A file with several edits, or one that opens on an empty edit, is refused: the reader does not model either.
pub(crate) fn audio_edit(file: &[u8]) -> Result<Option<AudioEdit>, String> {
  if file.get(4..8) != Some(FTYP.as_slice()) {
    return Ok(None);
  }
  let top = atoms(file)?;
  let Some(moov) = find(&top, MOOV) else {
    return Ok(None);
  };
  let movie = atoms(moov.body)?;
  let mvhd = find(&movie, MVHD).ok_or("MP4 file has no movie header")?;
  let movie_timescale = movie_timescale(mvhd.body)?;
  for trak in movie.iter().filter(|atom| atom.kind == TRAK) {
    let track = atoms(trak.body)?;
    let Some(mdia) = find(&track, MDIA) else {
      continue;
    };
    let media = atoms(mdia.body)?;
    if handler_type(find(&media, HDLR).ok_or("MP4 track has no handler")?.body)? != SOUN {
      continue;
    }
    let Some(edts) = find(&track, EDTS) else {
      return Ok(None);
    };
    let edit_boxes = atoms(edts.body)?;
    let Some(elst) = find(&edit_boxes, ELST) else {
      return Ok(None);
    };
    return edit_entry(elst.body, movie_timescale).map(Some);
  }
  Ok(None)
}

/// The boxes laid out in `data`. A size of 0 runs to the end of the data; a size of 1 is followed by a 64-bit size.
fn atoms(mut data: &[u8]) -> Result<Vec<Atom<'_>>, String> {
  let mut boxes = Vec::new();
  while !data.is_empty() {
    let (header, size) = match be_u32(data, 0)? {
      0 => (8, data.len()),
      1 => (16, usize::try_from(be_u64(data, 8)?).map_err(|_| "MP4 box size overflows")?),
      size => (8, size as usize),
    };
    if size < header || size > data.len() {
      return Err("MP4 box runs past its parent".to_string());
    }
    boxes.push(Atom { kind: &data[4..8], body: &data[header..size] });
    data = &data[size..];
  }
  Ok(boxes)
}

fn find<'a>(boxes: &'a [Atom<'a>], kind: &[u8; 4]) -> Option<&'a Atom<'a>> {
  boxes.iter().find(|atom| atom.kind == kind)
}

/// The movie header's time scale: its 32-bit field after the version-dependent creation and modification times.
fn movie_timescale(body: &[u8]) -> Result<u32, String> {
  match body.first() {
    Some(0) => be_u32(body, 12),
    Some(1) => be_u32(body, 20),
    _ => Err("MP4 movie header has an unknown version".to_string()),
  }
}

/// The handler type of an `hdlr` box, four bytes after its version, flags and pre-defined field.
fn handler_type(body: &[u8]) -> Result<&[u8], String> {
  body.get(8..12).ok_or_else(|| "MP4 handler box is truncated".to_string())
}

/// The one `elst` entry of an audio track. The reader needs a single edit that starts at a media time of 0 or later.
fn edit_entry(body: &[u8], movie_timescale: u32) -> Result<AudioEdit, String> {
  let version = *body.first().ok_or("MP4 edit list is truncated")?;
  let count = be_u32(body, 4)?;
  if count != 1 {
    return Err(format!("MP4 edit list has {count} entries; only a single entry is supported"));
  }
  let (duration, media_time) = match version {
    0 => (u64::from(be_u32(body, 8)?), i64::from(be_u32(body, 12)? as i32)),
    1 => (be_u64(body, 8)?, be_u64(body, 16)? as i64),
    _ => return Err("MP4 edit list has an unknown version".to_string()),
  };
  let media_time = u64::try_from(media_time)
    .map_err(|_| "MP4 edit list starts with an empty edit; only media from time 0 is supported".to_string())?;
  Ok(AudioEdit { media_time, duration, movie_timescale })
}

fn be_u32(data: &[u8], at: usize) -> Result<u32, String> {
  let bytes = data.get(at..at + 4).ok_or("MP4 box is truncated")?;
  Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn be_u64(data: &[u8], at: usize) -> Result<u64, String> {
  let bytes = data.get(at..at + 8).ok_or("MP4 box is truncated")?;
  Ok(u64::from_be_bytes(bytes.try_into().expect("an eight-byte slice")))
}

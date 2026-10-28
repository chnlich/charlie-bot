//! The `DiscardPadding` of a WebM file's last block: the trailing samples that libav drops from the stream.
//!
//! symphonia 0.6's Matroska reader parses the element but does not apply it, so an Opus track keeps the encoder's
//! end padding. This module walks the EBML tree to find the element. It enters the masters that hold blocks, so the
//! walk does not need their sizes, which a live recording leaves unknown.

const EBML_MAGIC: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];
const SEGMENT: u64 = 0x1853_8067;
const CLUSTER: u64 = 0x1F43_B675;
const BLOCK_GROUP: u64 = 0xA0;
const DISCARD_PADDING: u64 = 0x75A2;

/// The `DiscardPadding` of the last block that carries one, in nanoseconds. `None` when the file is not Matroska or no
/// block carries one. A file with one audio track has its padding in its audio blocks.
pub(crate) fn last_discard_padding_ns(file: &[u8]) -> Result<Option<i64>, String> {
  if !file.starts_with(&EBML_MAGIC) {
    return Ok(None);
  }
  let mut last = None;
  let mut at = 0;
  while at < file.len() {
    let (id, id_len) = read_vint(file, at, true)?;
    let (size, size_len) = read_vint(file, at + id_len, false)?;
    let body = at + id_len + size_len;
    match id {
      SEGMENT | CLUSTER | BLOCK_GROUP => at = body,
      DISCARD_PADDING => {
        let size = known_size(size, size_len, id)?;
        let bytes = file.get(body..body + size).ok_or("Matroska DiscardPadding is truncated")?;
        last = Some(signed_be(bytes)?);
        at = body + size;
      }
      _ => at = body + known_size(size, size_len, id)?,
    }
  }
  Ok(last)
}

/// A variable-length integer at `at`: an element ID keeps its length marker, a size drops it. Returns the value and
/// its length in bytes.
fn read_vint(file: &[u8], at: usize, keep_marker: bool) -> Result<(u64, usize), String> {
  let first = *file.get(at).ok_or("Matroska element is truncated")?;
  let len = first.leading_zeros() as usize + 1;
  let bytes = file.get(at..at + len).filter(|_| len <= 8).ok_or("Matroska variable-length integer is invalid")?;
  let first_bits = if keep_marker { u64::from(first) } else { u64::from(first) & ((1 << (8 - len)) - 1) };
  Ok((bytes[1..].iter().fold(first_bits, |value, byte| (value << 8) | u64::from(*byte)), len))
}

/// The size of a non-master element. A size of all ones, the unknown size, cannot be skipped.
fn known_size(size: u64, size_len: usize, id: u64) -> Result<usize, String> {
  if size == (1 << (7 * size_len)) - 1 {
    return Err(format!("Matroska element 0x{id:X} has an unknown size"));
  }
  usize::try_from(size).map_err(|_| "Matroska element size overflows".to_string())
}

/// A big-endian signed integer of 1 to 8 bytes.
fn signed_be(bytes: &[u8]) -> Result<i64, String> {
  if bytes.is_empty() || bytes.len() > 8 {
    return Err("Matroska DiscardPadding is not 1 to 8 bytes".to_string());
  }
  let fill = if bytes[0] & 0x80 != 0 { 0xFF } else { 0x00 };
  let mut buffer = [fill; 8];
  buffer[8 - bytes.len()..].copy_from_slice(bytes);
  Ok(i64::from_be_bytes(buffer))
}

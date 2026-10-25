//! A PAX tar writer that mirrors what Python's `tarfile.add` records for one member, with the ustar header block
//! built through the `tar` crate.
//!
//! Python's `tarfile` writes the PAX format (`tarfile.DEFAULT_FORMAT`): every member carries a ustar
//! header, and a `././@PaxHeader` extended entry precedes it whenever a field does not fit ustar's
//! limits or is a float. `mtime` arrives from `os.stat` as a float, so every member Python writes
//! carries a PAX `mtime` record. The Rust side keeps the same split: the rounded integer mtime in the
//! ustar header and the full-precision float in the PAX record, so a reader sees the same mtime value
//! from either archive.
//!
//! Names and link targets are raw bytes, as `os.fsencode` gives them. A value that is not UTF-8 goes
//! to its PAX record as those bytes under `hdrcharset=BINARY`, and its ustar field holds `?` for each
//! character, as Python's `surrogateescape` string does.

use std::io::{Read, Write};

const BLOCK: usize = 512;
const RECORD: usize = 10240;

const NAME_LEN: usize = 100;
const LINKNAME_LEN: usize = 100;
const UNAME_LEN: usize = 32;
const GNAME_LEN: usize = 32;

/// One member's metadata, in the fields Python's `tarfile.TarInfo` records.
pub struct Member<'a> {
  pub name: &'a [u8],
  /// The full `st_mode`, type bits included, as Python's `gettarinfo` stores it.
  pub mode: u32,
  pub uid: u32,
  pub gid: u32,
  pub size: u64,
  /// Seconds and nanoseconds of the mtime; the float value rides the PAX record.
  pub mtime_sec: i64,
  pub mtime_nsec: u32,
  pub typeflag: u8,
  pub linkname: &'a [u8],
  pub uname: &'a [u8],
  pub gname: &'a [u8],
  pub devmajor: u32,
  pub devminor: u32,
}

/// The code points of a name as Python's `str` holds it: the bytes decoded as UTF-8 with `surrogateescape`,
/// so each byte outside a UTF-8 sequence stands for U+DC00 plus that byte. Python sorts names by these.
pub fn python_code_points(name: &[u8]) -> Vec<u32> {
  let mut points = Vec::with_capacity(name.len());
  for chunk in name.utf8_chunks() {
    points.extend(chunk.valid().chars().map(u32::from));
    points.extend(chunk.invalid().iter().map(|&byte| 0xDC00 + u32::from(byte)));
  }
  points
}

/// Whether a number fits an octal field of `digits` characters: `digits - 1` octal digits, then NUL.
fn fits_ustar(value: i128, digits: u32) -> bool {
  (0..8i128.pow(digits - 1)).contains(&value)
}

/// The number a ustar field holds: the value itself when it fits, else 0 with a PAX record carrying it.
fn ustar_number(value: i128, digits: u32) -> u64 {
  if fits_ustar(value, digits) { value as u64 } else { 0 }
}

/// The float mtime Python's `tarfile` holds: the seconds plus the nanoseconds, as `os.stat` reports them.
fn mtime_float(member: &Member) -> f64 {
  member.mtime_sec as f64 + f64::from(member.mtime_nsec) * 1e-9
}

/// The integer mtime of the ustar field: the float rounded to even, as Python's `create_pax_header` does.
fn mtime_ustar(member: &Member) -> i128 {
  mtime_float(member).round_ties_even() as i128
}

/// Stores a text field as Python's ustar text: each code point that is not ASCII becomes `?`
/// (`stn` with `ascii` and `replace`), and the PAX record keeps the real value.
fn store_text(field: &mut [u8], value: &[u8]) {
  let text: Vec<u8> = python_code_points(value).into_iter().map(|c| if c < 0x80 { c as u8 } else { b'?' }).collect();
  let len = text.len().min(field.len());
  field[..len].copy_from_slice(&text[..len]);
}

fn ustar_header(member: &Member) -> [u8; BLOCK] {
  let mut header = tar::Header::new_ustar();
  // Python's `get_info` keeps the permission and the setuid, setgid and sticky bits only; a number that does not fit
  // its octal field stays 0 in the ustar header and rides the PAX record.
  header.set_mode(member.mode & 0o7777);
  header.set_uid(ustar_number(i128::from(member.uid), 8));
  header.set_gid(ustar_number(i128::from(member.gid), 8));
  header.set_size(ustar_number(i128::from(member.size), 12));
  header.set_mtime(ustar_number(mtime_ustar(member), 12));
  let ustar = header.as_ustar_mut().expect("new_ustar is a ustar header");
  store_text(&mut ustar.name, member.name);
  ustar.typeflag[0] = member.typeflag;
  store_text(&mut ustar.linkname, member.linkname);
  store_text(&mut ustar.uname, member.uname);
  store_text(&mut ustar.gname, member.gname);
  // Python writes the device numbers of character and block devices only; the other members keep NULs.
  if matches!(member.typeflag, b'3' | b'4') {
    header.set_device_major(member.devmajor).expect("the device number fits its octal field");
    header.set_device_minor(member.devminor).expect("the device number fits its octal field");
  }
  header.set_cksum();
  *header.as_bytes()
}

/// One PAX record: `len key=value\n`, with the length found by the same fixed-point
/// iteration Python's `_create_pax_generic_header` uses.
fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
  let mut p = 0;
  loop {
    let n = key.len() + value.len() + 3 + p.to_string().len();
    if n == p {
      break;
    }
    p = n;
  }
  let mut record = format!("{p} {key}=").into_bytes();
  record.extend_from_slice(value);
  record.push(b'\n');
  record
}

/// Python's `str(float)`: the shortest round-trip digits, with `.0` kept for integral values.
fn float_repr(value: f64) -> String {
  let text = format!("{value}");
  if text.contains('.') || text.contains('e') { text } else { format!("{text}.0") }
}

/// Writes the `././@PaxHeader` entry Python emits before a member whose fields need it, with the
/// records in Python's insertion order: path, linkpath, uname, gname, then the number fields.
/// Returns the bytes written, header and padding included.
fn pax_entry<W: Write>(out: &mut W, records: &[u8]) -> std::io::Result<u64> {
  let pax = Member {
    name: b"././@PaxHeader",
    mode: 0,
    uid: 0,
    gid: 0,
    size: records.len() as u64,
    mtime_sec: 0,
    mtime_nsec: 0,
    typeflag: b'x',
    linkname: b"",
    uname: b"",
    gname: b"",
    devmajor: 0,
    devminor: 0,
  };
  out.write_all(&ustar_header(&pax))?;
  out.write_all(records)?;
  // The records payload pads to a block boundary, as Python's `_create_payload` pads it.
  let padding = (BLOCK - records.len() % BLOCK) % BLOCK;
  out.write_all(&vec![0u8; padding])?;
  Ok((BLOCK + records.len() + padding) as u64)
}

/// The PAX records this member needs, empty when ustar holds every field. A value that is not UTF-8 makes
/// the header `hdrcharset=BINARY`, first, as Python's `_create_pax_generic_header` writes it.
fn pax_records(member: &Member) -> Vec<u8> {
  let mut fields: Vec<(&str, &[u8])> = Vec::new();
  let mut numbers: Vec<(&str, Vec<u8>)> = Vec::new();
  let text_fields = [
    ("path", member.name, NAME_LEN),
    ("linkpath", member.linkname, LINKNAME_LEN),
    ("uname", member.uname, UNAME_LEN),
    ("gname", member.gname, GNAME_LEN),
  ];
  for (key, value, limit) in text_fields {
    if !value.is_ascii() || value.len() > limit {
      fields.push((key, value));
    }
  }
  // A number field that does not fit its ustar field goes to a PAX record, as `create_pax_header` does.
  let number_fields =
    [("uid", i128::from(member.uid), 8), ("gid", i128::from(member.gid), 8), ("size", i128::from(member.size), 12)];
  for (key, value, digits) in number_fields {
    if !fits_ustar(value, digits) {
      numbers.push((key, value.to_string().into_bytes()));
    }
  }
  // os.stat hands Python's tarfile a float mtime, so the PAX record always rides the member,
  // exactly as Python writes it; the ustar header keeps the rounded integer.
  numbers.push(("mtime", float_repr(mtime_float(member)).into_bytes()));
  let binary = fields.iter().any(|(_, value)| std::str::from_utf8(value).is_err());
  let mut records = Vec::new();
  if binary {
    records.extend(pax_record("hdrcharset", b"BINARY"));
  }
  for (key, value) in fields {
    records.extend(pax_record(key, value));
  }
  for (key, value) in numbers {
    records.extend(pax_record(key, &value));
  }
  records
}

/// Writes one member and, for a regular file, its content. `content` is read lazily by the caller
/// through the returned write of the data bytes. Returns the bytes written, padding included.
pub fn write_member<W: Write>(out: &mut W, member: &Member, content: Option<&mut dyn Read>) -> std::io::Result<u64> {
  let mut written = 0u64;
  let records = pax_records(member);
  if !records.is_empty() {
    written += pax_entry(out, &records)?;
  }
  out.write_all(&ustar_header(member))?;
  written += BLOCK as u64;
  if let Some(reader) = content {
    let copied = std::io::copy(&mut reader.take(member.size), out)?;
    if copied < member.size {
      return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "file shorter than its header size"));
    }
    let padding = (BLOCK - (member.size as usize % BLOCK)) % BLOCK;
    out.write_all(&vec![0u8; padding])?;
    written += member.size + padding as u64;
  }
  Ok(written)
}

/// Closes the archive: the two zero blocks and the record-size padding Python's `tarfile.close`
/// writes. `bytes_written` counts every archive byte written so far, padding included.
pub fn finish<W: Write>(out: &mut W, bytes_written: u64) -> std::io::Result<()> {
  out.write_all(&[0u8; 2 * BLOCK])?;
  let total = bytes_written + 2 * BLOCK as u64;
  let padding = (RECORD - (total as usize % RECORD)) % RECORD;
  out.write_all(&vec![0u8; padding])?;
  Ok(())
}

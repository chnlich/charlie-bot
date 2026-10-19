//! A minimal ustar/PAX tar writer that mirrors what Python's `tarfile.add` records for one member.
//!
//! Python's `tarfile` writes the PAX format (`tarfile.DEFAULT_FORMAT`): every member carries a ustar
//! header, and a `././@PaxHeader` extended entry precedes it whenever a field does not fit ustar's
//! limits or is a float. `mtime` arrives from `os.stat` as a float, so every member Python writes
//! carries a PAX `mtime` record. The Rust side keeps the same split: the rounded integer mtime in the
//! ustar header and the full-precision float in the PAX record, so a reader sees the same mtime value
//! from either archive.

use std::io::{Read, Write};

const BLOCK: usize = 512;
const RECORD: usize = 10240;

const NAME_LEN: usize = 100;
const LINKNAME_LEN: usize = 100;
const UNAME_LEN: usize = 32;
const GNAME_LEN: usize = 32;

/// One member's metadata, in the fields Python's `tarfile.TarInfo` records.
pub struct Member<'a> {
  pub name: &'a str,
  /// The full `st_mode`, type bits included, as Python's `gettarinfo` stores it.
  pub mode: u32,
  pub uid: u32,
  pub gid: u32,
  pub size: u64,
  /// Seconds and nanoseconds of the mtime; the float value rides the PAX record.
  pub mtime_sec: i64,
  pub mtime_nsec: u32,
  pub typeflag: u8,
  pub linkname: &'a str,
  pub uname: &'a str,
  pub gname: &'a str,
  pub devmajor: u32,
  pub devminor: u32,
}

/// One octal number in a ustar field: `digits - 1` zero-padded digits, then NUL, as Python's `tarfile.itn`.
fn itn(field: &mut [u8], n: u64, digits: usize) {
  let text = format!("{:0width$o}", n, width = digits - 1);
  let bytes = text.as_bytes();
  assert!(bytes.len() < field.len(), "octal field overflow: {n}");
  field[..bytes.len()].copy_from_slice(bytes);
  field[bytes.len()] = 0;
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

fn store_name(field: &mut [u8], value: &str) {
  // Python's ustar text fields are ASCII: each other character becomes `?`, and the PAX record keeps it.
  let ascii: String = value.chars().map(|c| if c.is_ascii() { c } else { '?' }).collect();
  let bytes = ascii.as_bytes();
  let len = bytes.len().min(field.len());
  field[..len].copy_from_slice(&bytes[..len]);
}

fn ustar_header(member: &Member) -> [u8; BLOCK] {
  let mut header = [0u8; BLOCK];
  store_name(&mut header[0..NAME_LEN], member.name);
  // Python's `get_info` keeps the permission and the setuid, setgid and sticky bits only.
  itn(&mut header[100..108], u64::from(member.mode & 0o7777), 8);
  itn(&mut header[108..116], ustar_number(i128::from(member.uid), 8), 8);
  itn(&mut header[116..124], ustar_number(i128::from(member.gid), 8), 8);
  itn(&mut header[124..136], ustar_number(i128::from(member.size), 12), 12);
  itn(&mut header[136..148], ustar_number(mtime_ustar(member), 12), 12);
  header[148..156].fill(b' ');
  header[156] = member.typeflag;
  store_name(&mut header[157..157 + LINKNAME_LEN], member.linkname);
  header[257..263].copy_from_slice(b"ustar\0");
  header[263..265].copy_from_slice(b"00");
  store_name(&mut header[265..265 + UNAME_LEN], member.uname);
  store_name(&mut header[297..297 + GNAME_LEN], member.gname);
  // Python writes the device numbers of character and block devices only; the other members keep NULs.
  if matches!(member.typeflag, b'3' | b'4') {
    itn(&mut header[329..337], u64::from(member.devmajor), 8);
    itn(&mut header[337..345], u64::from(member.devminor), 8);
  }
  let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
  itn(&mut header[148..155], u64::from(checksum), 7);
  header[155] = b' ';
  header
}

/// One PAX record: `len key=value\n`, with the length found by the same fixed-point
/// iteration Python's `_create_pax_generic_header` uses.
fn pax_record(key: &str, value: &str) -> Vec<u8> {
  let mut p = 0;
  loop {
    let n = key.len() + value.len() + 3 + p.to_string().len();
    if n == p {
      break;
    }
    p = n;
  }
  format!("{p} {key}={value}\n").into_bytes()
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
    name: "././@PaxHeader",
    mode: 0,
    uid: 0,
    gid: 0,
    size: records.len() as u64,
    mtime_sec: 0,
    mtime_nsec: 0,
    typeflag: b'x',
    linkname: "",
    uname: "",
    gname: "",
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

/// The PAX records this member needs, empty when ustar holds every field.
fn pax_records(member: &Member) -> Vec<u8> {
  let mut records = Vec::new();
  if !member.name.is_ascii() || member.name.len() > NAME_LEN {
    records.extend(pax_record("path", member.name));
  }
  if !member.linkname.is_ascii() || member.linkname.len() > LINKNAME_LEN {
    records.extend(pax_record("linkpath", member.linkname));
  }
  if !member.uname.is_ascii() || member.uname.len() > UNAME_LEN {
    records.extend(pax_record("uname", member.uname));
  }
  if !member.gname.is_ascii() || member.gname.len() > GNAME_LEN {
    records.extend(pax_record("gname", member.gname));
  }
  // A number field that does not fit its ustar field goes to a PAX record, as `create_pax_header` does.
  let numbers =
    [("uid", i128::from(member.uid), 8), ("gid", i128::from(member.gid), 8), ("size", i128::from(member.size), 12)];
  for (key, value, digits) in numbers {
    if !fits_ustar(value, digits) {
      records.extend(pax_record(key, &value.to_string()));
    }
  }
  // os.stat hands Python's tarfile a float mtime, so the PAX record always rides the member,
  // exactly as Python writes it; the ustar header keeps the rounded integer.
  records.extend(pax_record("mtime", &float_repr(mtime_float(member))));
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
    let mut remaining = member.size;
    let mut buffer = [0u8; 64 * 1024];
    loop {
      let read = reader.read(&mut buffer)?;
      if read == 0 {
        break;
      }
      let take = read.min(remaining as usize);
      out.write_all(&buffer[..take])?;
      remaining -= take as u64;
    }
    if remaining > 0 {
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

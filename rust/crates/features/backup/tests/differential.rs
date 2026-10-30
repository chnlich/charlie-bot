//! Compare this crate's archive with the recorded Python `tarfile` output.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use nix::unistd::{Gid, Group, Uid, User};
use serde_json::{Value, json};

/// Writes one fixture file, creating its parent directories.
fn write(home: &Path, relative: impl AsRef<Path>, content: &[u8]) {
  let path = home.join(relative);
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).unwrap();
  }
  std::fs::write(path, content).unwrap();
}

/// Builds the synthetic home the two sides archive: nested directories, an empty directory, a credentials
/// file, a `.git` directory, a `.claude` directory, a pycache file, an opencode store at the top level and one
/// deeper, a session thread directory, names that are not UTF-8 at the top level and nested, a non-ASCII
/// UTF-8 name, a name over 100 bytes, and symlinks whose targets are over 100 bytes or not UTF-8.
fn build_fixture_home(home: &Path) {
  write(home, "config.yaml", b"server: {}\n");
  write(home, "nested/a/b/deep.txt", b"deep content\n");
  write(home, OsStr::from_bytes(b"raw-\xff\xfe.txt"), b"a top-level name that is not UTF-8\n");
  write(home, OsStr::from_bytes(b"nested/a/bad-\xc3(.txt"), b"a nested name with a truncated sequence\n");
  write(home, OsStr::from_bytes(b"nested/raw-dir-\xff/inner-\x80.txt"), b"inside a directory that is not UTF-8\n");
  write(home, "nested/a/café-日本.txt", b"a non-ASCII UTF-8 name\n");
  // Python sorts names by code point: the raw `\xf0` byte is U+DCF0, below U+E000, so the raw name comes first.
  // Byte order would put the private-use name first.
  write(home, OsStr::from_bytes(b"\xf0-raw.txt"), b"sorts before the private-use name\n");
  write(home, "\u{e000}-private.txt", b"sorts after the raw name\n");
  write(home, format!("{}.txt", "n".repeat(110)), b"a name over 100 bytes\n");
  std::fs::create_dir_all(home.join("empty_dir")).unwrap();
  std::os::unix::fs::symlink("nested", home.join("link")).unwrap();
  std::os::unix::fs::symlink("target-".repeat(15), home.join("long-link")).unwrap();
  std::os::unix::fs::symlink(OsStr::from_bytes(b"raw-target-\xff"), home.join("raw-link")).unwrap();
  write(home, "credentials.yaml", b"credentials:\n  access_key: secret\n");
  write(home, ".git/HEAD", b"ref: refs/heads/main\n");
  write(home, ".claude/settings.json", b"{}\n");
  write(home, "__pycache__/x.pyc", b"bytecode\n");
  write(home, "cache/y.pyc", b"more bytecode\n");
  write(home, "opencode/data/opencode/opencode.db", b"db\n");
  write(home, "sessions/s1/opencode/note.json", b"{}\n");
  write(home, "sessions/s1/threads/t1/metadata.json", b"{}\n");
}

/// Builds the fixture under `root` and archives it on the Rust side. The home is `root/charliebot`, and the
/// Rust side reads it through the symlink `root/rust/charliebot`, so both sides see the same metadata and
/// write into different backup directories: `root/charliebot_backup` for Python, `root/rust/charliebot_backup`
/// for this crate.
fn archive_fixture(root: &Path) -> (PathBuf, PathBuf) {
  let home = root.join("charliebot");
  std::fs::create_dir(&home).unwrap();
  build_fixture_home(&home);
  std::fs::create_dir(root.join("rust")).unwrap();
  let rust_home = root.join("rust").join("charliebot");
  std::os::unix::fs::symlink(&home, &rust_home).unwrap();
  let rust_archive = charliebot_backup::backup::create_backup_in(&rust_home).unwrap();
  (home, rust_archive)
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(value: &str) -> Vec<u8> {
  assert_eq!(value.len() % 2, 0, "hex fixture has an even number of digits");
  value
    .as_bytes()
    .as_chunks::<2>()
    .0
    .iter()
    .map(|digits| u8::from_str_radix(std::str::from_utf8(digits).unwrap(), 16).unwrap())
    .collect()
}

fn source_mtime(metadata: &std::fs::Metadata) -> f64 {
  metadata.mtime() as f64 + metadata.mtime_nsec() as f64 * 1e-9
}

fn float_repr(value: f64) -> String {
  let text = format!("{value}");
  if text.contains('.') || text.contains('e') { text } else { format!("{text}.0") }
}

fn recorded_python_members(home: &Path) -> Vec<Value> {
  let mut decoder = flate2::read::GzDecoder::new(include_bytes!("fixtures/backup_manifest_golden.json.gz").as_slice());
  let mut fixture_text = String::new();
  decoder.read_to_string(&mut fixture_text).unwrap();
  let fixture: Value = serde_json::from_str(&fixture_text).unwrap();
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  assert_eq!(
    fixture["fields"],
    json!([
      "name",
      "type",
      "mode",
      "uid",
      "gid",
      "uname",
      "gname",
      "mtime",
      "size",
      "linkname",
      "pax_headers",
      "content"
    ])
  );

  fixture["members"]
    .as_array()
    .unwrap()
    .iter()
    .map(|recorded| {
      let name = decode_hex(recorded["name_hex"].as_str().unwrap());
      let relative = name.strip_suffix(b"/").unwrap_or(&name);
      let metadata = std::fs::symlink_metadata(home.join(std::ffi::OsStr::from_bytes(relative))).unwrap();
      let uid = metadata.uid();
      let gid = metadata.gid();
      let uname = User::from_uid(Uid::from_raw(uid)).unwrap().expect("the source uid has a passwd entry").name;
      let gname = Group::from_gid(Gid::from_raw(gid)).unwrap().expect("the source gid has a group entry").name;
      let mtime = source_mtime(&metadata);
      let mut member = recorded.clone();
      member["mode"] = json!(metadata.mode() & 0o7777);
      member["uid"] = json!(uid);
      member["gid"] = json!(gid);
      member["uname_hex"] = json!(hex(uname.as_bytes()));
      member["gname_hex"] = json!(hex(gname.as_bytes()));
      member.as_object_mut().unwrap().remove("uname");
      member.as_object_mut().unwrap().remove("gname");
      member["mtime"] = json!(mtime);

      let pax = member["pax_headers"].as_object_mut().unwrap();
      for value in pax.values_mut() {
        let token = value.as_str().unwrap();
        *value = match token {
          "$SOURCE_UID" => json!(hex(uid.to_string().as_bytes())),
          "$SOURCE_GID" => json!(hex(gid.to_string().as_bytes())),
          "$UNAME" => json!(hex(uname.as_bytes())),
          "$GNAME" => json!(hex(gname.as_bytes())),
          "$SOURCE_MTIME" => json!(hex(float_repr(mtime).as_bytes())),
          _ => json!(token),
        };
      }
      member
    })
    .collect()
}

fn archive_members(archive_path: &Path) -> Vec<Value> {
  let file = std::fs::File::open(archive_path).unwrap();
  let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
  archive
    .entries()
    .unwrap()
    .map(|entry| {
      let mut entry = entry.unwrap();
      let mut pax = std::collections::BTreeMap::new();
      if let Some(extensions) = entry.pax_extensions().unwrap() {
        for extension in extensions {
          let extension = extension.unwrap();
          let key = std::str::from_utf8(extension.key_bytes()).unwrap().to_string();
          pax.insert(key, hex(extension.value_bytes()));
        }
      }

      let header = entry.header();
      let typeflag = header.entry_type().as_byte();
      let mut name = entry.path_bytes().into_owned();
      if typeflag == b'5' {
        name.pop();
      }
      let linkname = entry.link_name_bytes().map(|name| name.into_owned()).unwrap_or_default();
      let mode = header.mode().unwrap();
      let uid = pax
        .get("uid")
        .map(|value| String::from_utf8(decode_hex(value)).unwrap().parse::<u64>().unwrap())
        .unwrap_or_else(|| header.uid().unwrap());
      let gid = pax
        .get("gid")
        .map(|value| String::from_utf8(decode_hex(value)).unwrap().parse::<u64>().unwrap())
        .unwrap_or_else(|| header.gid().unwrap());
      let uname = pax
        .get("uname")
        .map(|value| decode_hex(value))
        .unwrap_or_else(|| header.username_bytes().expect("archive header has username bytes").to_vec());
      let gname = pax
        .get("gname")
        .map(|value| decode_hex(value))
        .unwrap_or_else(|| header.groupname_bytes().expect("archive header has groupname bytes").to_vec());
      let mtime = pax
        .get("mtime")
        .map(|value| String::from_utf8(decode_hex(value)).unwrap().parse::<f64>().unwrap())
        .unwrap_or_else(|| header.mtime().unwrap() as f64);
      let size = entry.size();
      let mut content = Vec::new();
      let is_file = typeflag == b'0' || typeflag == 0;
      if is_file {
        entry.read_to_end(&mut content).unwrap();
      }
      json!({
        "name_hex": hex(&name),
        "type_hex": hex(&[typeflag]),
        "mode": mode,
        "uid": uid,
        "gid": gid,
        "uname_hex": hex(&uname),
        "gname_hex": hex(&gname),
        "mtime": mtime,
        "size": size,
        "linkname_hex": hex(&linkname),
        "pax_headers": pax,
        "content_hex": if is_file { Some(hex(&content)) } else { None },
      })
    })
    .collect()
}

fn member_difference_count(left: &[Value], right: &[Value]) -> usize {
  left.iter().zip(right).filter(|(left, right)| left != right).count() + left.len().abs_diff(right.len())
}

fn assert_members_equal(actual: &[Value], expected: &[Value]) {
  if actual != expected {
    let index = actual
      .iter()
      .zip(expected)
      .position(|(actual, expected)| actual != expected)
      .unwrap_or_else(|| actual.len().min(expected.len()));
    panic!(
      "archive members differ at {index}: actual={}, expected={}",
      actual.get(index).cloned().unwrap_or(Value::Null),
      expected.get(index).cloned().unwrap_or(Value::Null),
    );
  }
}

#[test]
fn differential_backup_matches_python() {
  let root = tempfile::tempdir().unwrap();
  let (home, rust_archive) = archive_fixture(root.path());
  let expected = recorded_python_members(&home);
  let actual = archive_members(&rust_archive);
  assert_members_equal(&actual, &expected);
}

#[test]
fn differential_control_run_reports_one_planted_difference() {
  let root = tempfile::tempdir().unwrap();
  let (home, rust_archive) = archive_fixture(root.path());
  let expected = recorded_python_members(&home);
  let mut actual = archive_members(&rust_archive);
  let first_regular = actual.iter_mut().find(|member| !member["content_hex"].is_null()).unwrap();
  let mut content = decode_hex(first_regular["content_hex"].as_str().unwrap());
  content.extend_from_slice(b"planted difference\n");
  first_regular["content_hex"] = json!(hex(&content));
  assert_eq!(member_difference_count(&actual, &expected), 1);
}

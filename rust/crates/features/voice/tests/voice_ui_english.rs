//! Guard: the voice input UI serves English text only.
//!
//! Every source file the voice feature hands to the browser — its static assets and its templates — must be
//! free of CJK characters. A phrase copied from a Chinese plan page into a UI string would otherwise ship
//! unseen; the test fails on the file and line that holds it.

use std::fs;
use std::path::{Path, PathBuf};

/// The CJK blocks the voice UI must not contain: CJK punctuation and kana, CJK extension A, the unified
/// ideographs, Hangul syllables, CJK compatibility ideographs, and the fullwidth forms.
const CJK_RANGES: [(char, char); 6] = [
  ('\u{3000}', '\u{30ff}'),
  ('\u{3400}', '\u{4dbf}'),
  ('\u{4e00}', '\u{9fff}'),
  ('\u{ac00}', '\u{d7af}'),
  ('\u{f900}', '\u{faff}'),
  ('\u{ff00}', '\u{ffef}'),
];

/// The directories of voice UI source files, relative to the repository root.
const VOICE_UI_DIRS: [&str; 2] = ["src/features/voice/static", "src/features/voice/templates"];

fn is_cjk(character: char) -> bool {
  CJK_RANGES.iter().any(|&(low, high)| (low..=high).contains(&character))
}

/// Every file under `dir`, in no order yet.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
  let mut entries: Vec<PathBuf> = fs::read_dir(dir)
    .unwrap_or_else(|error| panic!("read_dir {}: {error}", dir.display()))
    .map(|entry| entry.expect("dir entry").path())
    .collect();
  entries.sort();
  for path in entries {
    if path.is_dir() {
      collect_files(&path, out);
    } else {
      out.push(path);
    }
  }
}

/// One `path:line: text` per line of a voice UI source file that holds a CJK character.
fn cjk_hits(root: &Path) -> Vec<String> {
  let mut paths = Vec::new();
  for directory in VOICE_UI_DIRS {
    collect_files(&root.join(directory), &mut paths);
  }
  paths.sort();
  let mut hits = Vec::new();
  for path in paths {
    let rel = path.strip_prefix(root).expect("voice UI file under the repo root").to_string_lossy();
    let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    for (number, line) in text.lines().enumerate() {
      if line.chars().any(is_cjk) {
        hits.push(format!("{rel}:{}: {line}", number + 1));
      }
    }
  }
  hits
}

#[test]
fn voice_ui_sources_hold_no_cjk() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("repo root");
  let hits = cjk_hits(root);
  assert!(hits.is_empty(), "{}", hits.join("\n"));
}

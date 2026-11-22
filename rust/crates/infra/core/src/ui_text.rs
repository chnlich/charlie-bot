/// Han, kana and Hangul ranges rejected in product UI text.
pub const UI_TEXT_RANGES: [(char, char); 5] = [
  ('\u{3040}', '\u{30ff}'),
  ('\u{3400}', '\u{4dbf}'),
  ('\u{4e00}', '\u{9fff}'),
  ('\u{ac00}', '\u{d7af}'),
  ('\u{f900}', '\u{faff}'),
];

/// Whether a character is in a Han, kana or Hangul range rejected in product UI text.
pub fn is_ui_text(character: char) -> bool {
  UI_TEXT_RANGES.iter().any(|&(low, high)| (low..=high).contains(&character))
}

/// Refuse a label that product UI shows when it holds a rejected character. `path` names the config key the label
/// came from, so the error points at the line the operator edits.
pub fn check_label(path: &str, value: &str) -> Result<(), String> {
  if let Some(character) = value.chars().find(|&character| is_ui_text(character)) {
    return Err(format!("{path} contains a disallowed UI text character {character:?}"));
  }
  Ok(())
}

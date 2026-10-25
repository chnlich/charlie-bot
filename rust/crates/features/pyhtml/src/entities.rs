use std::collections::HashMap;
use std::sync::LazyLock;

static HTML5_ENTITIES: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
  serde_json::from_str(include_str!("html5_entities.json")).expect("generated HTML5 entity table is valid JSON")
});

/// Escape ampersands, angle brackets, and optionally both quote characters, as CPython's `html.escape` does
/// (`quote` selects between its `quote=True` and `quote=False` entity sets; `/` is never escaped).
pub fn escape(input: &str, quote: bool) -> String {
  if quote {
    html_escape::encode_quoted_attribute(input).into_owned()
  } else {
    html_escape::encode_text(input).into_owned()
  }
}

/// Decode the named and numeric character references recognized by CPython's HTML5 table.
pub fn unescape(input: &str) -> String {
  if !input.contains('&') {
    return input.to_owned();
  }

  let mut output = String::with_capacity(input.len());
  let mut cursor = 0;
  while let Some(relative_amp) = input[cursor..].find('&') {
    let amp = cursor + relative_amp;
    output.push_str(&input[cursor..amp]);
    if let Some((end, replacement)) = parse_charref(input, amp) {
      output.push_str(&replacement);
      cursor = end;
    } else {
      output.push('&');
      cursor = amp + 1;
    }
  }
  output.push_str(&input[cursor..]);
  output
}

pub(crate) fn unescape_attribute(input: &str) -> String {
  let mut output = String::with_capacity(input.len());
  let mut cursor = 0;
  while cursor < input.len() {
    let Some(relative_amp) = input[cursor..].find('&') else {
      output.push_str(&input[cursor..]);
      break;
    };
    let amp = cursor + relative_amp;
    output.push_str(&input[cursor..amp]);
    let Some((end, reference)) = attribute_reference(input, amp) else {
      output.push('&');
      cursor = amp + 1;
      continue;
    };

    if reference.starts_with("&#") || (!reference.ends_with('=') && HTML5_ENTITIES.contains_key(&reference[1..])) {
      output.push_str(&unescape(&reference));
    } else {
      output.push_str(&reference);
    }
    cursor = end;
  }
  output
}

fn parse_charref(input: &str, amp: usize) -> Option<(usize, String)> {
  let after_amp = amp + 1;
  let tail = &input[after_amp..];
  if tail.starts_with('#') {
    if let Some((end, number, radix)) = numeric_reference(input, amp) {
      return Some((end, numeric_replacement(&number, radix)));
    }
    return None;
  }

  let mut end = after_amp;
  let mut count = 0;
  for (offset, ch) in input[after_amp..].char_indices() {
    if is_unescape_name_boundary(ch) || count == 32 {
      break;
    }
    count += 1;
    end = after_amp + offset + ch.len_utf8();
  }
  if count == 0 {
    return None;
  }
  if input[end..].starts_with(';') {
    end += 1;
  }
  let name = &input[after_amp..end];
  if let Some(value) = HTML5_ENTITIES.get(name) {
    return Some((end, value.clone()));
  }
  let without_semicolon = name.strip_suffix(';').unwrap_or(name);
  for boundary in (2..without_semicolon.len()).rev() {
    if !without_semicolon.is_char_boundary(boundary) {
      continue;
    }
    if let Some(value) = HTML5_ENTITIES.get(&without_semicolon[..boundary]) {
      let suffix_start = after_amp + boundary;
      let mut replacement = value.clone();
      replacement.push_str(&input[suffix_start..end]);
      return Some((end, replacement));
    }
  }
  let mut unchanged = String::with_capacity(name.len() + 1);
  unchanged.push('&');
  unchanged.push_str(name);
  Some((end, unchanged))
}

fn numeric_reference(input: &str, amp: usize) -> Option<(usize, String, u32)> {
  let digits_start = amp + 2;
  let after_hash = input.get(digits_start..)?;
  let (digits_start, radix, is_valid_digit): (usize, u32, fn(u8) -> bool) =
    if matches!(after_hash.chars().next(), Some('x' | 'X')) {
      (digits_start + 1, 16, is_ascii_hex_digit)
    } else {
      (digits_start, 10, is_ascii_digit)
    };
  let bytes = input.as_bytes();
  let mut end = digits_start;
  while end < bytes.len() && is_valid_digit(bytes[end]) {
    end += 1;
  }
  if end == digits_start {
    return None;
  }
  if bytes.get(end) == Some(&b';') {
    end += 1;
  }
  Some((end, input[digits_start..end].trim_end_matches(';').to_owned(), radix))
}

fn numeric_replacement(digits: &str, radix: u32) -> String {
  let significant = digits.trim_start_matches('0');
  if significant.is_empty() {
    return "\u{fffd}".to_owned();
  }
  let maximum_digits = if radix == 10 { 7 } else { 6 };
  if significant.len() > maximum_digits {
    return "\u{fffd}".to_owned();
  }
  let number = u32::from_str_radix(significant, radix).expect("validated numeric character reference");
  match number {
    0 => "\u{fffd}".to_owned(),
    0x0d => "\r".to_owned(),
    0x80 => "\u{20ac}".to_owned(),
    0x81 => "\u{81}".to_owned(),
    0x82 => "\u{201a}".to_owned(),
    0x83 => "\u{192}".to_owned(),
    0x84 => "\u{201e}".to_owned(),
    0x85 => "\u{2026}".to_owned(),
    0x86 => "\u{2020}".to_owned(),
    0x87 => "\u{2021}".to_owned(),
    0x88 => "\u{2c6}".to_owned(),
    0x89 => "\u{2030}".to_owned(),
    0x8a => "\u{160}".to_owned(),
    0x8b => "\u{2039}".to_owned(),
    0x8c => "\u{152}".to_owned(),
    0x8d => "\u{8d}".to_owned(),
    0x8e => "\u{17d}".to_owned(),
    0x8f => "\u{8f}".to_owned(),
    0x90 => "\u{90}".to_owned(),
    0x91 => "\u{2018}".to_owned(),
    0x92 => "\u{2019}".to_owned(),
    0x93 => "\u{201c}".to_owned(),
    0x94 => "\u{201d}".to_owned(),
    0x95 => "\u{2022}".to_owned(),
    0x96 => "\u{2013}".to_owned(),
    0x97 => "\u{2014}".to_owned(),
    0x98 => "\u{2dc}".to_owned(),
    0x99 => "\u{2122}".to_owned(),
    0x9a => "\u{161}".to_owned(),
    0x9b => "\u{203a}".to_owned(),
    0x9c => "\u{153}".to_owned(),
    0x9d => "\u{9d}".to_owned(),
    0x9e => "\u{17e}".to_owned(),
    0x9f => "\u{178}".to_owned(),
    0xd800..=0xdfff | 0x110000..=u32::MAX => "\u{fffd}".to_owned(),
    _ if is_invalid_codepoint(number) => String::new(),
    _ => char::from_u32(number).expect("validated Unicode scalar value").to_string(),
  }
}

fn is_invalid_codepoint(number: u32) -> bool {
  (0x1..=0x8).contains(&number)
    || (0xe..=0x1f).contains(&number)
    || number == 0xb
    || number == 0x7f
    || (0x80..=0x9f).contains(&number)
    || (0xfdd0..=0xfdef).contains(&number)
    || (number & 0xffff) >= 0xfffe
}

fn is_unescape_name_boundary(ch: char) -> bool {
  matches!(ch, '\t' | '\n' | '\u{c}' | ' ' | '<' | '&' | '#' | ';')
}

fn is_ascii_hex_digit(byte: u8) -> bool {
  byte.is_ascii_hexdigit()
}

fn is_ascii_digit(byte: u8) -> bool {
  byte.is_ascii_digit()
}

fn attribute_reference(input: &str, amp: usize) -> Option<(usize, String)> {
  let mut end = amp + 1;
  let bytes = input.as_bytes();
  if bytes.get(end) == Some(&b'#') {
    end += 1;
    let radix_hex = matches!(bytes.get(end), Some(b'x' | b'X'));
    if radix_hex {
      end += 1;
    }
    let digit_start = end;
    while end < bytes.len() && if radix_hex { is_ascii_hex_digit(bytes[end]) } else { bytes[end].is_ascii_digit() } {
      end += 1;
    }
    if end == digit_start {
      return None;
    }
  } else if bytes.get(end).is_some_and(u8::is_ascii_alphabetic) {
    end += 1;
    while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
      end += 1;
    }
  } else {
    return None;
  }
  if matches!(bytes.get(end), Some(b';' | b'=')) {
    end += 1;
  }
  Some((end, input[amp..end].to_owned()))
}

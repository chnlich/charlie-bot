//! Weighted text length: the counting rule of the inter-agent message limit (`src/infra/text_length.py`).

/// The weighted length of `text`: one per character, plus one more for each CJK punctuation mark (U+3000-303F), CJK
/// ideograph (U+4E00-9FFF) or fullwidth form (U+FF00-FFEF). Python's `len` counts code points, and so does this.
pub fn weighted_length(text: &str) -> usize {
  text
    .chars()
    .map(|c| if matches!(c as u32, 0x3000..=0x303F | 0x4E00..=0x9FFF | 0xFF00..=0xFFEF) { 2 } else { 1 })
    .sum()
}

#[cfg(test)]
mod tests {
  use super::weighted_length;

  #[test]
  fn weighted_length_counts_cjk_punctuation_and_fullwidth_double() {
    // Python: tests/test_agent_message.py::test_weighted_length_counts_cjk_punctuation_and_fullwidth_double
    assert_eq!(weighted_length("plain ascii"), 11);
    assert_eq!(weighted_length(&"一".repeat(1000)), 2000);
    assert_eq!(weighted_length("。"), 2);
    assert_eq!(weighted_length("Ａ"), 2);
    assert_eq!(weighted_length(""), 0);
  }
}

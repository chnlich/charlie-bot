//! The HTML element-name sets that consumers outside the parser agree on.

/// The HTML void elements: an element with no end tag, so a start tag opens nothing that an end tag closes.
pub const VOID_TAGS: &[&str] =
  &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn void_tags_are_the_html_standard_void_elements() {
    assert_eq!(
      VOID_TAGS,
      &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"]
    );
  }
}

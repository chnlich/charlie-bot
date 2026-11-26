//! The HTML element-name sets that consumers outside the parser agree on.

/// The HTML void elements: an element with no end tag, so a start tag opens nothing that an end tag closes.
pub const VOID_TAGS: &[&str] =
  &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

/// The block-level HTML elements: each one starts and ends its own block, so text does not run across its edge.
pub const BLOCK_TAGS: &[&str] = &[
  "address",
  "article",
  "aside",
  "blockquote",
  "body",
  "caption",
  "dd",
  "details",
  "dialog",
  "div",
  "dl",
  "dt",
  "fieldset",
  "figcaption",
  "figure",
  "footer",
  "form",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "header",
  "hgroup",
  "hr",
  "li",
  "main",
  "nav",
  "ol",
  "p",
  "pre",
  "section",
  "summary",
  "table",
  "tbody",
  "td",
  "tfoot",
  "th",
  "thead",
  "tr",
  "ul",
];

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

//! The one page-open fold definition that the artifact check and the cold-read extract share.

/// Whether an element folds its content at page open: a `details` start tag with no `open` attribute. `attributes`
/// names every attribute the start tag carries. The template's fold is `details.details-layer`, and its class plays no
/// part here: a plain `details` folds the same way. Both the fork-options check and the cold-read visible-text extract
/// decide what a reader sees with this predicate.
pub fn folds_at_open<'a>(tag: &str, attributes: impl IntoIterator<Item = &'a str>) -> bool {
  tag == "details" && !attributes.into_iter().any(|name| name == "open")
}

//! The HTML document that the file server's pages share: the head with the viewport and the style, and the body.
//!
//! Below 600 px each listing row is a grid: the icon beside the name, and the size and modified time beneath the name.
//! The viewport meta makes a phone use its own width, so the query matches the screen and not a 980 px desktop layout.

/// The page titled `title`, with `body` in its body. The caller escapes each value taken from the file system or the
/// request before it puts the value into either argument.
///
/// The format reads the style's doubled braces as single ones.
pub(crate) fn document(title: &str, body: &str) -> String {
  format!(
    r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1.0"><title>{title}</title>
<style>
  body {{ font-family: monospace; margin: 2em; }}
  table {{ border-collapse: collapse; }}
  td, th {{ padding: 4px 12px; text-align: left; }}
  td.size {{ text-align: right; }}
  /* Inline, the icon sits on the text baseline, and the line box below the baseline makes the
     cell taller than a text row. Block drops that line box; the negative vertical margin keeps the
     16 px icon's box from making the row taller than the text line beside it. */
  td.icon > .cb-icon {{ display: block; margin: -1px 0; }}
  a {{ text-decoration: none; color: #0366d6; }}
  a:hover {{ text-decoration: underline; }}
  @media (max-width: 600px) {{
    body {{ margin: 1em; }}
    h2 {{ overflow-wrap: anywhere; }}
    table, tbody {{ display: block; }}
    tr {{ display: grid; grid-template-columns: 2em 6em minmax(0, 1fr); padding: 6px 0; border-bottom: 1px solid #e1e4e8; }}
    tr.head {{ display: none; }}
    td {{ padding: 0; }}
    td.icon {{ grid-row: 1 / span 2; }}
    td.name {{ grid-column: 2 / span 2; overflow-wrap: anywhere; }}
    td.size {{ grid-column: 2; grid-row: 2; text-align: left; }}
    td.modified {{ grid-column: 3; grid-row: 2; }}
  }}
</style>
</head>
<body>
{body}
</body>
</html>"#
  )
}

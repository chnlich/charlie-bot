//! The one home of the CLI's JSON encoding.

use charliebot_core::text_repr::{self, JsonStyle};
use serde_json::Value;

/// One line, `", "` between items, with non-ASCII passed through.
const ONE_LINE: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// The rendered `value`: non-ASCII characters kept as written, insertion order, one line when
/// `indent` is `None` else an indent block.
///
/// The result ends with a trailing newline.
pub fn render(value: &Value, indent: Option<usize>) -> String {
  let style = match indent {
    None => ONE_LINE,
    Some(width) => {
      JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ": ", indent: Some(width) }
    }
  };
  format!("{}\n", text_repr::json_dumps(value, &style))
}

/// The printed `value` as a command's result: the two-space-indented JSON block that `render` writes.
pub fn print_result(value: &Value) {
  print!("{}", render(value, Some(2)));
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn one_line_keeps_non_ascii_and_separators() {
    assert_eq!(render(&json!({ "now": "中文" }), None), "{\"now\": \"中文\"}\n");
  }

  #[test]
  fn indent_two_uses_the_json_dumps_item_separator() {
    assert_eq!(render(&json!({ "a": [1, 2] }), Some(2)), "{\n  \"a\": [\n    1,\n    2\n  ]\n}\n");
  }
}

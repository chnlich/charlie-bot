//! `print_json`: the one home of the CLI's JSON encoding.

use charliebot_core::text_repr::{self, JsonStyle};
use serde_json::Value;

/// `json.dumps(value, ensure_ascii=False, indent=None)`: one line, `", "` between items.
const ONE_LINE: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// `json.dumps(value, ensure_ascii=False, indent=N)`: an indent block, `","` between items.
const INDENTED: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ": ", indent: Some(2) };

/// Render `value` the way `print_json` writes it: non-ASCII characters kept as written, insertion
/// order, one line when `indent` is `None` else an indent block, plus the trailing newline.
pub fn render(value: &Value, indent: Option<usize>) -> String {
  let style = match indent {
    None => ONE_LINE,
    Some(width) => JsonStyle { indent: Some(width), ..INDENTED },
  };
  format!("{}\n", text_repr::json_dumps(value, &style))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn one_line_keeps_non_ascii_and_python_separators() {
    assert_eq!(render(&json!({ "now": "中文" }), None), "{\"now\": \"中文\"}\n");
  }

  #[test]
  fn indent_two_uses_the_json_dumps_item_separator() {
    assert_eq!(render(&json!({ "a": [1, 2] }), Some(2)), "{\n  \"a\": [\n    1,\n    2\n  ]\n}\n");
  }
}

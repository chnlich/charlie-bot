//! The form a script takes when a page carries it inline.
//!
//! A page inlines its scripts so the first paint waits for no second request. The same source files carry the
//! comments that explain them to an editor, and a browser runs those as nothing. They also carry the indentation
//! that shows their nesting, and the spaces between tokens, which the parser folds away. On the mock home's chat
//! page, those sit in the scripts and the stylesheets. Leaving them out of the inline copy saves 67 KB of the page's
//! 700 KB and 13 KB of its 164 KB gzip form, and the token spaces take another 2 KB off that form. The strip is
//! textual and keeps every other byte. That includes a `//` or `/*` that a string, a template literal or a regular
//! expression holds. It also keeps the whitespace inside a template literal, which is part of the text it builds.

use crate::inlined_literal::{char_len, escape_end, string_end};

/// The JavaScript `source` with its comments and its insignificant whitespace removed.
///
/// A line comment loses its text and keeps the newline that ends it. A block comment becomes one newline when it
/// spans lines and, otherwise, the space [`needs_space`] asks for between the bytes around it. A whitespace run
/// becomes one newline when it holds a line terminator, and [`needs_space`]'s space or nothing when it does not, so
/// a blank line and a line's indentation leave no byte and no two tokens join. The scanner follows the literals that
/// can hold comment text or whitespace. A quoted string ends at its quote or its line's end, a template literal at
/// its backtick, and a regex literal at its closing slash.
pub fn without_comments(source: &str) -> String {
  let bytes = source.as_bytes();
  let mut out = String::with_capacity(source.len());
  let mut stack = vec![Place::code()];
  let mut expect_expression = true;
  let mut i = 0;
  while i < bytes.len() {
    if matches!(stack.last(), Some(Place::Template)) {
      match bytes[i] {
        b'\\' => {
          let end = escape_end(source, i);
          i = copy(&mut out, source, i, end);
        }
        b'`' => {
          copy(&mut out, source, i, i + 1);
          stack.pop();
          expect_expression = false;
          i += 1;
        }
        b'$' if bytes.get(i + 1) == Some(&b'{') => {
          copy(&mut out, source, i, i + 2);
          stack.push(Place::code());
          expect_expression = true;
          i += 2;
        }
        _ => i = copy_char(&mut out, source, i),
      }
      continue;
    }
    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
      i = line_end(bytes, i).unwrap_or(bytes.len());
      continue;
    }
    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
      let end = source[i + 2..].find("*/").map_or(bytes.len(), |end| i + 2 + end + 2);
      if spans_lines(&bytes[i..end]) {
        out.push('\n');
      } else if needs_space(out.as_bytes().last().copied(), bytes.get(end).copied()) {
        out.push(' ');
      }
      i = end;
      continue;
    }
    if bytes[i] == b'/' && expect_expression {
      match regex_end(source, i) {
        Some(end) => {
          clear_control(&mut stack);
          copy(&mut out, source, i, end);
          expect_expression = false;
          i = end;
        }
        None => {
          copy(&mut out, source, i, i + 1);
          i += 1;
        }
      }
      continue;
    }
    match bytes[i] {
      b'\'' | b'"' => {
        let end = string_end(source, i);
        clear_control(&mut stack);
        copy(&mut out, source, i, end);
        expect_expression = false;
        i = end;
      }
      b'`' => {
        clear_control(&mut stack);
        copy(&mut out, source, i, i + 1);
        stack.push(Place::Template);
        i += 1;
      }
      b'{' => {
        if let Some(Place::Code { braces, control, .. }) = stack.last_mut() {
          *braces += 1;
          *control = false;
        }
        copy(&mut out, source, i, i + 1);
        expect_expression = true;
        i += 1;
      }
      b'}' => {
        let decremented = match stack.last_mut() {
          Some(Place::Code { braces, control, .. }) if *braces > 0 => {
            *braces -= 1;
            *control = false;
            true
          }
          Some(Place::Code { control, .. }) => {
            *control = false;
            false
          }
          Some(Place::Template) | None => false,
        };
        if !decremented && stack.len() > 1 {
          stack.pop();
        }
        copy(&mut out, source, i, i + 1);
        expect_expression = false;
        i += 1;
      }
      byte if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' => {
        let end = identifier_end(bytes, i);
        let word = &source[i..end];
        expect_expression = expects_expression(word);
        if let Some(Place::Code { control, .. }) = stack.last_mut() {
          *control = matches!(word, "if" | "while" | "for" | "with");
        }
        copy(&mut out, source, i, end);
        i = end;
      }
      byte if byte.is_ascii_digit() || byte == b'.' => {
        let end = number_end(bytes, i);
        clear_control(&mut stack);
        copy(&mut out, source, i, end);
        expect_expression = false;
        i = end;
      }
      b'(' => {
        if let Some(Place::Code { parens, control, .. }) = stack.last_mut() {
          parens.push(*control);
          *control = false;
        }
        copy(&mut out, source, i, i + 1);
        expect_expression = true;
        i += 1;
      }
      b')' => {
        let follows_control = match stack.last_mut() {
          Some(Place::Code { parens, .. }) => parens.pop().unwrap_or(false),
          Some(Place::Template) | None => false,
        };
        clear_control(&mut stack);
        copy(&mut out, source, i, i + 1);
        expect_expression = follows_control;
        i += 1;
      }
      b']' => {
        clear_control(&mut stack);
        copy(&mut out, source, i, i + 1);
        expect_expression = false;
        i += 1;
      }
      byte if byte.is_ascii_whitespace() => {
        i = copy_whitespace(&mut out, source, i);
      }
      _ => {
        clear_control(&mut stack);
        let end = copy_char(&mut out, source, i);
        expect_expression = bytes[i] != b'.';
        i = end;
      }
    }
  }
  out
}

/// The shortest form of the whitespace run at `start`, and the index after it.
///
/// A run that holds a line terminator becomes one newline: the blank line after it and the indentation of the next
/// line separate no tokens, and the newline the automatic semicolons need is still there. A run of spaces and tabs
/// becomes one space where the bytes on either side would otherwise run together, and nothing otherwise.
fn copy_whitespace(out: &mut String, source: &str, start: usize) -> usize {
  let bytes = source.as_bytes();
  let mut end = start;
  let mut line_terminator = false;
  while let Some(&byte) = bytes.get(end) {
    match byte {
      b'\n' | b'\r' => line_terminator = true,
      b' ' | b'\t' | b'\x0c' => {}
      _ => break,
    }
    end += 1;
  }
  if line_terminator {
    out.push('\n');
  } else if needs_space(out.as_bytes().last().copied(), bytes.get(end).copied()) {
    out.push(' ');
  }
  end
}

/// Whether the bytes `prev` and `next` need a space between them to stay two tokens.
///
/// A token that a name, a number or a literal continues takes everything the parser reads of it, so two such bytes
/// run into one token without the space: `return x` must not become `returnx`. Every other pair is two tokens with
/// or without it, except the pairs that are one token when joined (`+` `+`, `/` `*`, `<` `!`, `?` `.`) and the
/// number whose `.` would join its integer part (`1 .5`).
fn needs_space(prev: Option<u8>, next: Option<u8>) -> bool {
  let (Some(prev), Some(next)) = (prev, next) else {
    return false;
  };
  merges_into_one_token(prev, next) || (is_name_byte(prev) && is_name_byte(next))
}

/// Whether `prev` followed by `next` reads as one token when the space between them leaves.
///
/// The last three pairs are not JavaScript's: the inline copy is also the template engine's source, so a `{` beside
/// a second `{`, a `%` or a `#` would open `{{`, `{%` or `{#` where the source had two tokens.
fn merges_into_one_token(prev: u8, next: u8) -> bool {
  if prev == next && matches!(prev, b'+' | b'-' | b'*' | b'/' | b'&' | b'|' | b'<' | b'>' | b'=' | b'!' | b'?') {
    return true;
  }
  matches!((prev, next), (b'/', b'*') | (b'<', b'!') | (b'-', b'>') | (b'?', b'.') | (b'.', b'.'))
    || (prev.is_ascii_digit() && next == b'.')
    || matches!((prev, next), (b'{', b'{') | (b'{', b'%') | (b'{', b'#'))
}

/// Whether `byte` continues a name, a number or a literal, or holds one on its own.
fn is_name_byte(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'\\') || byte >= 0x80
}

/// Where the scanner is: JavaScript code, or a template literal's text.
enum Place {
  /// Code with its open-brace count and, for each open parenthesis, whether a control keyword introduced it.
  ///
  /// A `/` after the `)` of `if (…)` opens a statement's regex literal, while a `/` after any other `)` divides.
  Code {
    braces: usize,
    parens: Vec<bool>,
    control: bool,
  },
  Template,
}

impl Place {
  fn code() -> Place {
    Place::Code { braces: 0, parens: Vec::new(), control: false }
  }
}

/// Forget a control keyword the scanner has left behind.
fn clear_control(stack: &mut [Place]) {
  if let Some(Place::Code { control, .. }) = stack.last_mut() {
    *control = false;
  }
}

/// The index of the first JavaScript line terminator at or after `from`: LF, CR, U+2028 or U+2029.
fn line_end(bytes: &[u8], from: usize) -> Option<usize> {
  let mut i = from;
  while i < bytes.len() {
    match bytes[i] {
      b'\n' | b'\r' => return Some(i),
      0xE2 if bytes.get(i + 1) == Some(&0x80) && matches!(bytes.get(i + 2), Some(0xA8 | 0xA9)) => return Some(i),
      _ => i += 1,
    }
  }
  None
}

/// Whether the text holds a JavaScript line terminator.
fn spans_lines(bytes: &[u8]) -> bool {
  line_end(bytes, 0).is_some()
}

/// The copy of `source[start..end]` to `out`, returning `end`.
fn copy(out: &mut String, source: &str, start: usize, end: usize) -> usize {
  out.push_str(&source[start..end]);
  end
}

/// The copy of the one character at `start` to `out`, returning the index after it.
fn copy_char(out: &mut String, source: &str, start: usize) -> usize {
  let end = start + char_len(&source[start..]);
  copy(out, source, start, end)
}

/// The index after the identifier that starts at `start`.
fn identifier_end(bytes: &[u8], start: usize) -> usize {
  let mut end = start;
  while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'$') {
    end += 1;
  }
  end
}

/// The index after the number that starts at `start`: its digits, decimal point, exponent and suffix letters.
fn number_end(bytes: &[u8], start: usize) -> usize {
  let mut end = start;
  while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'.' | b'_' | b'$')) {
    end += 1;
  }
  end
}

/// Whether the word `word` is followed by an expression, so a `/` after it opens a regex literal rather than
/// dividing. Every other word is a value, and a `/` after a value divides.
fn expects_expression(word: &str) -> bool {
  matches!(
    word,
    "return"
      | "typeof"
      | "void"
      | "delete"
      | "in"
      | "of"
      | "instanceof"
      | "new"
      | "do"
      | "else"
      | "case"
      | "throw"
      | "yield"
      | "await"
  )
}

/// The index after the regex literal that starts at `start`, when the line holds its closing slash and flag
/// letters. A slash that opens no regex on its line is division.
fn regex_end(source: &str, start: usize) -> Option<usize> {
  let bytes = source.as_bytes();
  let mut i = start + 1;
  let mut in_class = false;
  while i < bytes.len() {
    match bytes[i] {
      b'\\' => {
        if matches!(bytes.get(i + 1), Some(&b'\n') | Some(&b'\r')) {
          return None;
        }
        i = escape_end(source, i);
      }
      b'\n' | b'\r' => return None,
      b'[' => {
        in_class = true;
        i += 1;
      }
      b']' => {
        in_class = false;
        i += 1;
      }
      b'/' if !in_class => {
        i += 1;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
          i += 1;
        }
        return Some(i);
      }
      _ => i += 1,
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn code_indentation_is_dropped_and_template_text_keeps_its_own() {
    assert_eq!(without_comments("let a = 1;\n    let b = 2;\n"), "let a=1;\nlet b=2;\n");
    assert_eq!(without_comments("let s = `\n  indented text\n`;"), "let s=`\n  indented text\n`;");
    assert_eq!(without_comments("let s = `${a}\n  ${b}`;"), "let s=`${a}\n  ${b}`;");
    assert_eq!(without_comments("let s = `${a +\n    b}`;"), "let s=`${a+\nb}`;");
  }

  #[test]
  fn a_line_terminator_run_keeps_one_newline() {
    assert_eq!(without_comments("a();\n\n\nb();"), "a();\nb();");
    assert_eq!(without_comments("a();\n   \n\t\nb();"), "a();\nb();");
    assert_eq!(without_comments("a();\r\n\r\nb();"), "a();\nb();");
  }

  #[test]
  fn token_spaces_leave_where_the_tokens_stay_apart() {
    assert_eq!(without_comments("x = -1;"), "x=-1;");
    assert_eq!(without_comments("if (x) { y(); }"), "if(x){y();}");
    assert_eq!(without_comments("return x"), "return x");
    assert_eq!(without_comments("typeof x"), "typeof x");
    assert_eq!(without_comments("x = typeof y;"), "x=typeof y;");
    assert_eq!(without_comments("a ? .5 : b"), "a? .5:b");
    assert_eq!(without_comments("a++ + b"), "a++ +b");
  }

  #[test]
  fn token_spaces_stay_where_the_tokens_would_merge() {
    assert_eq!(without_comments("a + +b"), "a+ +b");
    assert_eq!(without_comments("a - -b"), "a- -b");
    assert_eq!(without_comments("a / /re/"), "a/ /re/");
    assert_eq!(without_comments("a < !--b"), "a< !--b");
    assert_eq!(without_comments("1 .5"), "1 .5");
  }

  #[test]
  fn a_space_stays_where_its_removal_would_open_a_template_tag() {
    assert_eq!(without_comments("function f() { { return 1; } }"), "function f(){ {return 1;}}");
    assert_eq!(without_comments("a{/* note */{b}}"), "a{ {b}}");
    assert_eq!(without_comments("class A { #x; }"), "class A{ #x;}");
  }

  #[test]
  fn line_comments_leave_the_code_and_keep_their_newline() {
    assert_eq!(without_comments("let a = 1; // one\nlet b = 2;\n"), "let a=1;\nlet b=2;\n");
    assert_eq!(without_comments("// only a comment"), "");
    assert_eq!(without_comments("a(); // trailing"), "a();");
  }

  #[test]
  fn block_comments_keep_the_tokens_apart() {
    assert_eq!(without_comments("let a = 1;/* note */let b = 2;"), "let a=1;let b=2;");
    assert_eq!(without_comments("let a = 1;/* one\ntwo */let b = 2;"), "let a=1;\nlet b=2;");
    assert_eq!(without_comments("/** a doc comment */function f() {}"), "function f(){}");
    assert_eq!(without_comments("a/* note */b"), "a b");
  }

  #[test]
  fn a_comment_marker_inside_a_string_is_text() {
    assert_eq!(
      without_comments(r#"let url = "https://example.com/a"; // gone"#),
      r#"let url="https://example.com/a";"#
    );
    assert_eq!(without_comments(r#"let s = '/* not a comment */';"#), r#"let s='/* not a comment */';"#);
    assert_eq!(without_comments("let s = 'a // b';"), "let s='a // b';");
  }

  #[test]
  fn a_comment_marker_inside_a_template_literal_is_text() {
    assert_eq!(without_comments("let s = `a // b`; // gone"), "let s=`a // b`;");
    assert_eq!(without_comments("let s = `a /* b */ c`;"), "let s=`a /* b */ c`;");
    assert_eq!(without_comments("let s = `\\` // still text`;"), "let s=`\\` // still text`;");
  }

  #[test]
  fn a_template_literal_expression_is_code() {
    assert_eq!(without_comments("let s = `a ${b /* gone */ + c}`;"), "let s=`a ${b+c}`;");
    assert_eq!(without_comments("let s = `${`inner // text`}`;"), "let s=`${`inner // text`}`;");
  }

  #[test]
  fn a_comment_marker_inside_a_regex_is_text() {
    assert_eq!(without_comments("let r = /[//]/; // gone"), "let r=/[//]/;");
    assert_eq!(without_comments("let r = /a\\/\\/b/g; // gone"), "let r=/a\\/\\/b/g;");
  }

  #[test]
  fn a_slash_after_a_control_statements_paren_opens_a_regex() {
    assert_eq!(without_comments("if (ready) /[//]/.test(value); // gone"), "if(ready)/[//]/.test(value);");
    assert_eq!(without_comments("while (a) /x/.test(b);"), "while(a)/x/.test(b);");
  }

  #[test]
  fn a_slash_after_another_paren_divides() {
    assert_eq!(without_comments("let c = (a) / b; // gone"), "let c=(a)/b;");
  }

  #[test]
  fn a_template_escape_of_a_multibyte_character_is_kept() {
    assert_eq!(without_comments("let s = `\\é // text`; // gone"), "let s=`\\é // text`;");
  }

  #[test]
  fn a_string_continued_over_a_line_keeps_its_text() {
    assert_eq!(without_comments("let s = \"one\\\r\n  // two\"; // gone"), "let s=\"one\\\r\n  // two\";");
    assert_eq!(without_comments("let s = 'one\\\n  // two'; // gone"), "let s='one\\\n  // two';");
    assert_eq!(without_comments("let s = '\\é /* kept */'; // gone"), "let s='\\é /* kept */';");
  }

  #[test]
  fn a_line_comment_ends_at_any_line_terminator() {
    assert_eq!(without_comments("// note\rcount += 1;"), "\ncount+=1;");
    assert_eq!(without_comments("// note\u{2028}count += 1;"), "\u{2028}count+=1;");
  }

  #[test]
  fn a_slash_after_a_value_divides_and_a_slash_after_an_operator_opens_a_regex() {
    assert_eq!(without_comments("let c = a / b; // gone"), "let c=a/b;");
    assert_eq!(without_comments("let r = return_value / 2; // gone"), "let r=return_value/2;");
    assert_eq!(without_comments("function f() { return /x/.test(y); } // gone"), "function f(){return/x/.test(y);}");
  }

  #[test]
  fn a_line_comment_ending_the_source_needs_no_newline() {
    assert_eq!(without_comments("a(); // end"), "a();");
  }
}

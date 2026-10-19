//! Python's `csv.DictReader` with the default `excel` dialect, for the text `ncu --csv --page details` prints.
//!
//! The state machine follows CPython's `_csv.c` (`parse_process_char`). The text splits into lines at `\n`, as
//! `io.StringIO` yields them, and the end of each line is one more input character. So a `\r` before a `\n` ends an
//! unquoted field, and a quoted field may run across lines.

use std::collections::HashMap;
use std::fmt;
use std::mem;

/// CPython's default `csv.field_size_limit()`, in characters.
const FIELD_LIMIT: usize = 131_072;

/// A `_csv.Error`: the text is not a CSV record under the default dialect.
#[derive(Debug, PartialEq, Eq)]
pub struct CsvError(&'static str);

impl fmt::Display for CsvError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(self.0)
  }
}

/// One `csv.DictReader` row. A header name the record ends before maps to `None`, as DictReader fills it.
pub struct Row {
  values: HashMap<String, Option<String>>,
}

impl Row {
  /// `row.get(name)`: `None` when the header has no such column, `Some(None)` when the record ends before it.
  pub fn get(&self, name: &str) -> Option<Option<&str>> {
    self.values.get(name).map(Option::as_deref)
  }

  /// `dict(zip(fieldnames, record))`, then the header names past the record's end set to `None`.
  fn new(fieldnames: &[String], record: &[String]) -> Row {
    let mut values = HashMap::new();
    for (name, value) in fieldnames.iter().zip(record) {
      values.insert(name.clone(), Some(value.clone()));
    }
    for name in fieldnames.iter().skip(record.len()) {
      values.insert(name.clone(), None);
    }
    Row { values }
  }
}

/// `list(csv.DictReader(text))`: the records after the header, with blank records skipped.
pub fn dict_rows(text: &str) -> Result<Vec<Row>, CsvError> {
  let mut records = records(text)?.into_iter();
  let Some(fieldnames) = records.next() else {
    return Ok(Vec::new());
  };
  Ok(records.filter(|record| !record.is_empty()).map(|record| Row::new(&fieldnames, &record)).collect())
}

/// `list(csv.reader(text))`: every record, each as its fields.
fn records(text: &str) -> Result<Vec<Vec<String>>, CsvError> {
  let mut lines = text.split_inclusive('\n');
  let mut reader = Reader::default();
  let mut records = Vec::new();
  while let Some(record) = reader.next_record(&mut lines)? {
    records.push(record);
  }
  Ok(records)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum State {
  #[default]
  StartRecord,
  StartField,
  InField,
  InQuotedField,
  QuoteInQuotedField,
  EatCrnl,
}

#[derive(Default)]
struct Reader {
  state: State,
  field: String,
  field_len: usize,
  record: Vec<String>,
}

impl Reader {
  /// `Reader_iternext`: the next record, or `None` once the lines run out between records.
  fn next_record<'a>(&mut self, lines: &mut impl Iterator<Item = &'a str>) -> Result<Option<Vec<String>>, CsvError> {
    self.state = State::StartRecord;
    loop {
      let Some(line) = lines.next() else {
        // Non-strict end of input: an unfinished quoted field is saved as it stands.
        if self.state == State::InQuotedField || self.field_len != 0 {
          self.save_field();
          return Ok(Some(mem::take(&mut self.record)));
        }
        return Ok(None);
      };
      for c in line.chars() {
        self.process(Some(c))?;
      }
      self.process(None)?;
      if self.state == State::StartRecord {
        return Ok(Some(mem::take(&mut self.record)));
      }
    }
  }

  /// `parse_process_char`: one character, or `None` for the end of a line.
  fn process(&mut self, c: Option<char>) -> Result<(), CsvError> {
    match self.state {
      State::StartRecord => match c {
        None => {}
        Some('\n' | '\r') => self.state = State::EatCrnl,
        Some(_) => {
          self.state = State::StartField;
          self.start_field(c)?;
        }
      },
      State::StartField => self.start_field(c)?,
      State::InField => match c {
        None | Some('\n' | '\r') => self.end_field(c),
        Some(',') => {
          self.save_field();
          self.state = State::StartField;
        }
        Some(ch) => self.add(ch)?,
      },
      State::InQuotedField => match c {
        None => {}
        Some('"') => self.state = State::QuoteInQuotedField,
        Some(ch) => self.add(ch)?,
      },
      State::QuoteInQuotedField => match c {
        Some('"') => {
          self.add('"')?;
          self.state = State::InQuotedField;
        }
        Some(',') => {
          self.save_field();
          self.state = State::StartField;
        }
        None | Some('\n' | '\r') => self.end_field(c),
        Some(ch) => {
          self.add(ch)?;
          self.state = State::InField;
        }
      },
      State::EatCrnl => match c {
        Some('\n' | '\r') => {}
        None => self.state = State::StartRecord,
        Some(_) => {
          return Err(CsvError(
            "new-line character seen in unquoted field - do you need to open the file with newline=''?",
          ));
        }
      },
    }
    Ok(())
  }

  /// The `START_FIELD` transitions.
  fn start_field(&mut self, c: Option<char>) -> Result<(), CsvError> {
    match c {
      None | Some('\n' | '\r') => self.end_field(c),
      Some('"') => self.state = State::InQuotedField,
      Some(',') => self.save_field(),
      Some(ch) => {
        self.add(ch)?;
        self.state = State::InField;
      }
    }
    Ok(())
  }

  /// Save the field, then leave it for the next record at the end of a line, or for the next field otherwise.
  fn end_field(&mut self, c: Option<char>) {
    self.save_field();
    self.state = if c.is_none() { State::StartRecord } else { State::EatCrnl };
  }

  fn add(&mut self, ch: char) -> Result<(), CsvError> {
    if self.field_len >= FIELD_LIMIT {
      return Err(CsvError("field larger than field limit (131072)"));
    }
    self.field.push(ch);
    self.field_len += 1;
    Ok(())
  }

  fn save_field(&mut self) {
    self.record.push(mem::take(&mut self.field));
    self.field_len = 0;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn names(text: &str) -> Vec<Vec<String>> {
    records(text).unwrap()
  }

  #[test]
  fn records_follow_the_excel_dialect() {
    assert_eq!(names("a,\"b,\"\"c\"\"\",d\r\n"), vec![vec!["a", "b,\"c\"", "d"]]);
    assert_eq!(names("x,\"two\nlines\"\n\nlast"), vec![vec!["x", "two\nlines"], vec![], vec!["last"]]);
    assert_eq!(names("\"quoted\"tail,end\n"), vec![vec!["quotedtail", "end"]]);
    assert_eq!(names("\"never closed"), vec![vec!["never closed"]]);
    assert_eq!(names(""), Vec::<Vec<String>>::new());
  }

  #[test]
  fn a_carriage_return_inside_an_unquoted_field_is_an_error() {
    assert!(records("a\rb\n").is_err());
  }

  #[test]
  fn dict_rows_pad_short_records_and_skip_blank_ones() {
    let rows = dict_rows("ID,Name,Unit\n1,alpha\n\n2,beta,ns\n").unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("Name"), Some(Some("alpha")));
    assert_eq!(rows[0].get("Unit"), Some(None));
    assert_eq!(rows[1].get("Unit"), Some(Some("ns")));
    assert_eq!(rows[1].get("Missing"), None);
  }
}

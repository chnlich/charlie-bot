//! Python's `csv.DictReader` with the default `excel` dialect, for the text `ncu --csv --page details` prints.
//!
//! The `csv` crate owns record splitting and unescaping (`ReaderBuilder` with the excel dialect's double-quote
//! quoting and CRLF record terminators). A sequential pre-scan keeps the two `_csv` behaviors the crate does not
//! reproduce, in the order Python's parser hits them: a bare carriage return in an unquoted field followed by
//! content is a `_csv.Error`, and so is a field past CPython's 131072-character `csv.field_size_limit()`.
//! `DictReader`'s short-record padding stays here too.

use std::collections::HashMap;
use std::fmt;

/// CPython's default `csv.field_size_limit()`, in characters.
const FIELD_LIMIT: usize = 131_072;

/// A `_csv.Error`: the text is not a CSV record under the default dialect.
#[derive(Debug, PartialEq, Eq)]
pub struct CsvError(String);

impl fmt::Display for CsvError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.0)
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
  // DictReader takes the first record as its fieldnames. A blank first line makes that record empty, so every
  // header name is absent from the rows; the crate skips blank lines instead of yielding empty records.
  let mut records: Box<dyn Iterator<Item = Vec<String>>> = if text.starts_with('\n') || text.starts_with("\r\n") {
    Box::new(std::iter::once(Vec::new()).chain(excel_records(text)?))
  } else {
    Box::new(excel_records(text)?.into_iter())
  };
  let Some(fieldnames) = records.next() else {
    return Ok(Vec::new());
  };
  Ok(records.filter(|record| !record.is_empty()).map(|record| Row::new(&fieldnames, &record)).collect())
}

/// `list(csv.reader(text))` as far as the rows DictReader consumes: every record, each as its fields, with the
/// blank records the `csv` crate skips in place of Python's empty ones.
fn excel_records(text: &str) -> Result<Vec<Vec<String>>, CsvError> {
  parse_checks(text)?;
  let mut reader = csv::ReaderBuilder::new().has_headers(false).flexible(true).from_reader(text.as_bytes());
  let mut records = Vec::new();
  for record in reader.records() {
    let record = record.map_err(|error| CsvError(error.to_string()))?;
    records.push(record.iter().map(str::to_string).collect());
  }
  Ok(records)
}

/// The `_csv` error checks over the same characters Python's `parse_process_char` visits, in order, so the first
/// error is the one Python raises. States mirror `_csv.c` without the field buffers, which the crate owns.
fn parse_checks(text: &str) -> Result<(), CsvError> {
  #[derive(Clone, Copy, PartialEq, Eq)]
  enum Scan {
    StartRecord,
    StartField,
    InField,
    InQuoted,
    QuoteInQuoted,
    EatCrnl,
  }
  use Scan::{EatCrnl, InField, InQuoted, QuoteInQuoted, StartField, StartRecord};

  let mut state = StartRecord;
  let mut field_len = 0usize;
  for line in text.split_inclusive('\n') {
    for ch in line.chars() {
      match state {
        // At record start a terminator only ends the blank record; Python stays in `START_RECORD`.
        StartRecord => match ch {
          '\n' | '\r' => {}
          '"' => {
            field_len = 0;
            state = InQuoted;
          }
          ',' => {
            field_len = 0;
            state = StartField;
          }
          _ => {
            field_len = 0;
            state = InField;
            add(&mut field_len)?;
          }
        },
        StartField => match ch {
          '\n' | '\r' => {
            field_len = 0;
            state = EatCrnl;
          }
          '"' => {
            field_len = 0;
            state = InQuoted;
          }
          ',' => {
            field_len = 0;
            state = StartField;
          }
          _ => {
            field_len = 0;
            state = InField;
            add(&mut field_len)?;
          }
        },
        InField => match ch {
          '\n' | '\r' => {
            field_len = 0;
            state = EatCrnl;
          }
          ',' => {
            field_len = 0;
            state = StartField;
          }
          _ => add(&mut field_len)?,
        },
        InQuoted => match ch {
          '"' => state = QuoteInQuoted,
          _ => add(&mut field_len)?,
        },
        QuoteInQuoted => match ch {
          '"' => {
            state = InQuoted;
            add(&mut field_len)?;
          }
          ',' => {
            field_len = 0;
            state = StartField;
          }
          '\n' | '\r' => {
            field_len = 0;
            state = EatCrnl;
          }
          _ => {
            state = InField;
            add(&mut field_len)?;
          }
        },
        EatCrnl => match ch {
          '\n' | '\r' => {}
          _ => {
            return Err(CsvError(
              "new-line character seen in unquoted field - do you need to open the file with newline=''?".into(),
            ));
          }
        },
      }
    }
    // The end of a line: a quoted field runs on to the next line, anything else ends its record.
    if state != InQuoted {
      field_len = 0;
      state = StartRecord;
    }
  }
  Ok(())
}

/// One field character: `_csv` raises once a field passes `csv.field_size_limit()` characters.
fn add(field_len: &mut usize) -> Result<(), CsvError> {
  *field_len += 1;
  if *field_len > FIELD_LIMIT {
    return Err(CsvError("field larger than field limit (131072)".into()));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn names(text: &str) -> Vec<Vec<String>> {
    excel_records(text).unwrap()
  }

  #[test]
  fn records_follow_the_excel_dialect() {
    assert_eq!(names("a,\"b,\"\"c\"\"\",d\r\n"), vec![vec!["a", "b,\"c\"", "d"]]);
    // A quoted field runs across record terminators; blank lines yield no record.
    assert_eq!(names("x,\"two\nlines\"\n\nlast"), vec![vec!["x", "two\nlines"], vec!["last"]]);
    assert_eq!(names("\"quoted\"tail,end\n"), vec![vec!["quotedtail", "end"]]);
    assert_eq!(names("\"never closed"), vec![vec!["never closed"]]);
    assert_eq!(names(""), Vec::<Vec<String>>::new());
  }

  #[test]
  fn a_carriage_return_inside_an_unquoted_field_is_an_error() {
    assert!(excel_records("a\rb\n").is_err());
  }

  #[test]
  fn a_field_past_the_python_field_limit_is_an_error() {
    let mut text = String::from("a,\"");
    text.extend(std::iter::repeat_n('x', FIELD_LIMIT + 1));
    text.push('"');
    assert!(excel_records(&text).is_err());
    let mut text = String::from("a,");
    text.extend(std::iter::repeat_n('x', FIELD_LIMIT));
    assert_eq!(names(&text), vec![vec!["a".to_string(), "x".repeat(FIELD_LIMIT)]]);
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

  #[test]
  fn a_blank_first_line_makes_every_header_name_absent() {
    let rows = dict_rows("\nID,Name\n1,alpha\n").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("ID"), None);
    assert_eq!(rows[1].get("ID"), None);
  }
}

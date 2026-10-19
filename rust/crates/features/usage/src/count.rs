//! A token count as the ledger stores and reads it.
//!
//! Python's capture adds each usage count through `... or 0` arithmetic and binds the result as it comes. An integer
//! binds as an INTEGER, and a sum with a fractional operand is a float that binds as a REAL. The ledger's INTEGER
//! columns keep either type, so the stored value and its type match Python's only when a `Count` keeps the variant
//! that the arithmetic produced.
//!
//! `Int` and `Real` follow Python's `int` and `float`: an operation on two integers gives an integer, and any real
//! operand makes the result a real.

use std::cmp::Ordering;

use anyhow::anyhow;
use rusqlite::ToSql;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, Value, ValueRef};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Count {
  Int(i64),
  Real(f64),
}

impl Default for Count {
  fn default() -> Self {
    Count::Int(0)
  }
}

impl Count {
  /// The sum, bounded to the signed 64-bit range that an INTEGER column holds. `label` names the sum in the error.
  pub fn add(self, other: Count, label: &str) -> anyhow::Result<Count> {
    match (self, other) {
      (Count::Int(left), Count::Int(right)) => {
        left.checked_add(right).map(Count::Int).ok_or_else(|| anyhow!("{label} exceed the signed 64-bit range"))
      }
      _ => Ok(Count::Real(self.as_f64() + other.as_f64())),
    }
  }

  /// The difference, bounded like `add`.
  pub fn sub(self, other: Count, label: &str) -> anyhow::Result<Count> {
    match (self, other) {
      (Count::Int(left), Count::Int(right)) => {
        left.checked_sub(right).map(Count::Int).ok_or_else(|| anyhow!("{label} exceed the signed 64-bit range"))
      }
      _ => Ok(Count::Real(self.as_f64() - other.as_f64())),
    }
  }

  /// The order by value: two integers compare exactly, and any other pair compares as reals.
  pub fn compare(self, other: Count) -> Ordering {
    match (self, other) {
      (Count::Int(left), Count::Int(right)) => left.cmp(&right),
      _ => self.as_f64().total_cmp(&other.as_f64()),
    }
  }

  fn as_f64(self) -> f64 {
    match self {
      Count::Int(number) => number as f64,
      Count::Real(number) => number,
    }
  }

  /// The integer a test expects; a real count fails the assertion that reads it.
  #[cfg(test)]
  pub fn int(self) -> i64 {
    match self {
      Count::Int(number) => number,
      Count::Real(number) => panic!("expected an integer count, found {number}"),
    }
  }
}

impl ToSql for Count {
  fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
    Ok(match *self {
      Count::Int(number) => ToSqlOutput::from(number),
      Count::Real(number) => ToSqlOutput::from(number),
    })
  }
}

impl FromSql for Count {
  fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
    match value {
      ValueRef::Integer(number) => Ok(Count::Int(number)),
      ValueRef::Real(number) => Ok(Count::Real(number)),
      _ => Err(FromSqlError::InvalidType),
    }
  }
}

impl From<Count> for Value {
  fn from(count: Count) -> Self {
    match count {
      Count::Int(number) => Value::Integer(number),
      Count::Real(number) => Value::Real(number),
    }
  }
}

impl From<Count> for serde_json::Value {
  fn from(count: Count) -> Self {
    match count {
      Count::Int(number) => serde_json::Value::from(number),
      Count::Real(number) => serde_json::Value::from(number),
    }
  }
}

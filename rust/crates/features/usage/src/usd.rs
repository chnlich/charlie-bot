//! The usage page's API list-price equivalent: a week's merged rows priced at their rows of the shared table
//! (`charliebot_core::list_price`). A model with no row, or with a ledger category its row cannot price, reads N/A:
//! its tokens leave the week's total and are counted apart.

use charliebot_core::list_price::{self, TokenCounts};

use crate::count::Count;
use crate::page_rows::{PageRow, group_thousands};

/// One week's list-price figures.
pub(crate) struct WeekUsd {
  /// The list-price equivalent of the priced models, in USD.
  pub usd: f64,
  /// The tokens of the priced models.
  pub priced_tokens: Count,
  /// The tokens of the models that read N/A.
  pub left_out_tokens: Count,
}

impl WeekUsd {
  fn of(rows: &[PageRow]) -> anyhow::Result<WeekUsd> {
    let mut week = WeekUsd { usd: 0.0, priced_tokens: Count::Int(0), left_out_tokens: Count::Int(0) };
    for row in rows {
      match row_usd(row) {
        Some(usd) => {
          week.usd += usd;
          week.priced_tokens = week.priced_tokens.add(row.total, "page priced tokens")?;
        }
        None => week.left_out_tokens = week.left_out_tokens.add(row.total, "page left-out tokens")?,
      }
    }
    Ok(week)
  }
}

/// The last Pacific week and the week before it, priced.
pub(crate) struct UsdWeeks {
  pub last: WeekUsd,
  pub previous: WeekUsd,
}

impl UsdWeeks {
  /// The figures of the two weeks' merged rows.
  pub fn of(last_rows: &[PageRow], previous_rows: &[PageRow]) -> anyhow::Result<UsdWeeks> {
    Ok(UsdWeeks { last: WeekUsd::of(last_rows)?, previous: WeekUsd::of(previous_rows)? })
  }

  /// The change from the week before in USD, or `None` when the week before has no priced tokens to measure against.
  pub fn change(&self) -> Option<f64> {
    (self.previous.priced_tokens.as_f64() != 0.0).then_some(self.last.usd - self.previous.usd)
  }

  /// The change from the week before as a percent of it, or `None` when the week before has no priced tokens.
  pub fn change_percent(&self) -> Option<f64> {
    self.change().map(|change| change / self.previous.usd * 100.0)
  }
}

/// A row's list-price equivalent in USD, or `None` when the row reads N/A. The ledger's `in_unsplit` input has no split
/// into uncached and cached input, so a row that holds any cannot be priced.
pub(crate) fn row_usd(row: &PageRow) -> Option<f64> {
  let price = list_price::lookup(&row.model)?;
  if row.in_unsplit.as_f64() > 0.0 {
    return None;
  }
  price.usd(TokenCounts {
    input: row.in_fresh.as_f64(),
    cache_read: row.cache_read.as_f64(),
    cache_write: row.cache_write.as_f64(),
    output: row.output.as_f64(),
  })
}

/// A dollar amount with two decimals and comma groups, such as `$1,234.56`.
pub(crate) fn text(usd: f64) -> String {
  let cents = (usd * 100.0).round() as i64;
  format!("${}.{:02}", group_thousands(Count::Int(cents / 100)), cents % 100)
}

/// A signed change in dollars, such as `+$12.34` or `-$12.34`.
pub(crate) fn signed_text(change: f64) -> String {
  let sign = if change < 0.0 { "-" } else { "+" };
  format!("{sign}{}", text(change.abs()))
}

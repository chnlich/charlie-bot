//! The usage page's rows (`src/features/usage/api.py` `_merge_ledger_rows`, `_account_source`, `_model_leaf`,
//! `_compact`): the ledger's per-(source, model) rows folded into one page row per model, and each account attributed
//! to the card it counts under.

use std::collections::BTreeMap;

use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::Registry;
use charliebot_registry::usage::UsageSource;
use serde_json::{Value, json};

use crate::capture::{CHARLIE_BOT_SOURCE, backend_source};
use crate::count::Count;
use crate::ledger::LedgerRow;

/// One model's usage over every ledger part that names it. `accounts` ranks the (card · account) sub-rows by total,
/// and `segments` holds one entry per card the model ran on, in card order.
pub(crate) struct PageRow {
  pub model: String,
  pub calls: i64,
  pub in_fresh: Count,
  pub cache_write: Count,
  pub cache_read: Count,
  pub in_unsplit: Count,
  pub output: Count,
  pub total: Count,
  pub fallback_output: Count,
  pub accounts: Vec<PageAccount>,
  pub segments: Vec<Segment>,
  pub window: String,
}

pub(crate) struct PageAccount {
  pub name: String,
  pub calls: i64,
  pub output: Count,
  pub total: Count,
}

pub(crate) struct Segment {
  /// The card's position from 1, in card order.
  pub slot: usize,
  pub total: Count,
  pub output: Count,
}

impl PageRow {
  /// The row as the page's payload and the template context carry it: the keys in the order the Python dict holds them.
  pub fn to_json(&self) -> Value {
    json!({
      "model": self.model,
      "calls": self.calls,
      "in_fresh": count_json(self.in_fresh),
      "cache_write": count_json(self.cache_write),
      "cache_read": count_json(self.cache_read),
      "in_unsplit": count_json(self.in_unsplit),
      "output": count_json(self.output),
      "total": count_json(self.total),
      "fallback_output": count_json(self.fallback_output),
      "accounts": self.accounts.iter().map(PageAccount::to_json).collect::<Vec<_>>(),
      "segments": self.segments.iter().map(Segment::to_json).collect::<Vec<_>>(),
      "window": self.window,
    })
  }
}

impl PageAccount {
  fn to_json(&self) -> Value {
    json!({"name": self.name, "calls": self.calls, "output": count_json(self.output), "total": count_json(self.total)})
  }
}

impl Segment {
  fn to_json(&self) -> Value {
    json!({"slot": self.slot, "total": count_json(self.total), "output": count_json(self.output)})
  }
}

#[derive(Default)]
struct Tally {
  calls: i64,
  output: Count,
  total: Count,
}

/// `_merge_ledger_rows`: the page rows, ranked by total then model. Rows group on the casefolded model leaf, so a
/// spelling one source gives a model merges with the others; versions stay apart.
pub(crate) fn merge(
  rows: &[LedgerRow],
  config: &CharlieBotConfig,
  registry: &Registry,
  cards: &[&UsageSource],
) -> anyhow::Result<Vec<PageRow>> {
  let mut groups: BTreeMap<String, Vec<&LedgerRow>> = BTreeMap::new();
  for row in rows {
    groups.entry(canonical_model(&row.model)).or_default().push(row);
  }
  let mut merged =
    groups.values().map(|parts| merge_group(parts, config, registry, cards)).collect::<anyhow::Result<Vec<_>>>()?;
  merged.sort_by(|left, right| right.total.compare(left.total).then_with(|| left.model.cmp(&right.model)));
  Ok(merged)
}

fn merge_group(
  parts: &[&LedgerRow],
  config: &CharlieBotConfig,
  registry: &Registry,
  cards: &[&UsageSource],
) -> anyhow::Result<PageRow> {
  let mut accounts: BTreeMap<(String, String, bool), Tally> = BTreeMap::new();
  let mut seg_totals: BTreeMap<String, Count> = BTreeMap::new();
  let mut seg_outputs: BTreeMap<String, Count> = BTreeMap::new();
  for part in parts {
    for account in &part.accounts {
      let (source, fallback) = account_source(part, &account.name, config, registry)?;
      let tally = accounts.entry((source.clone(), account.name.clone(), fallback)).or_default();
      tally.calls += account.calls;
      tally.output = tally.output.add(account.output, "page account output")?;
      tally.total = tally.total.add(account.total, "page account total")?;
      let seg_total = seg_totals.entry(source.clone()).or_default();
      *seg_total = seg_total.add(account.total, "page segment total")?;
      let seg_output = seg_outputs.entry(source).or_default();
      *seg_output = seg_output.add(account.output, "page segment output")?;
    }
  }
  let mut ranked: Vec<((String, String, bool), Tally)> = accounts.into_iter().collect();
  ranked.sort_by(|(left_key, left), (right_key, right)| {
    right.total.compare(left.total).then_with(|| left_key.cmp(right_key))
  });
  let segments = cards
    .iter()
    .enumerate()
    .filter_map(|(index, card)| {
      seg_totals.get(card.name).map(|&total| Segment {
        slot: index + 1,
        total,
        output: seg_outputs.get(card.name).copied().unwrap_or_default(),
      })
    })
    .collect();
  let first = parts.iter().map(|part| part.first.as_str()).filter(|stamp| !stamp.is_empty()).min().unwrap_or("");
  let last = parts.iter().map(|part| part.last.as_str()).filter(|stamp| !stamp.is_empty()).max().unwrap_or("");
  let spelling = first_max_by(parts, |part| part.total).expect("a group holds at least one part");
  Ok(PageRow {
    model: model_leaf(&spelling.model).to_string(),
    calls: parts.iter().map(|part| part.calls).sum(),
    in_fresh: sum_counts(parts.iter().map(|part| &part.in_fresh), "page row in-fresh")?,
    cache_write: sum_counts(parts.iter().map(|part| &part.cache_write), "page row cache-write")?,
    cache_read: sum_counts(parts.iter().map(|part| &part.cache_read), "page row cache-read")?,
    in_unsplit: sum_counts(parts.iter().map(|part| &part.in_unsplit), "page row in-unsplit")?,
    output: sum_counts(parts.iter().map(|part| &part.output), "page row output")?,
    total: sum_counts(parts.iter().map(|part| &part.total), "page row total")?,
    fallback_output: sum_counts(parts.iter().map(|part| &part.fallback_output), "page row fallback output")?,
    accounts: ranked
      .into_iter()
      .map(|((source, name, fallback), tally)| PageAccount {
        name: format!("{source} · {name}{}", if fallback { " (fallback)" } else { "" }),
        calls: tally.calls,
        output: tally.output,
        total: tally.total,
      })
      .collect(),
    segments,
    window: format!("{first} → {last}"),
  })
}

/// `_account_source`: the card name an account counts under, and whether it is a fallback. A row of another source
/// keeps its own source. A charlie-bot row's account is a backend id, which attributes to the CLI that ran the call,
/// and that CLI's card marks the account a fallback unless the card reads only CharlieBot's own logs.
pub(crate) fn account_source(
  row: &LedgerRow,
  account: &str,
  config: &CharlieBotConfig,
  registry: &Registry,
) -> anyhow::Result<(String, bool)> {
  if row.source != CHARLIE_BOT_SOURCE {
    return Ok((row.source.clone(), false));
  }
  let source = backend_page_source(account, config, registry)?;
  Ok((source.name.to_string(), !source.run_logs_only))
}

/// `backend_page_source`: the usage source one charlie-bot account id counts under. An id with no source is an error,
/// so a backend with no collection rule never lands under a wrong CLI.
pub(crate) fn backend_page_source<'a>(
  backend: &str,
  config: &CharlieBotConfig,
  registry: &'a Registry,
) -> anyhow::Result<&'a UsageSource> {
  backend_source(backend, config, registry)?.ok_or_else(|| {
    anyhow::anyhow!("{backend}: neither a registered backend type nor a known type prefix has a usage source")
  })
}

/// `_model_leaf`: the last `/` segment minus a trailing ` (provider)` suffix, with its case kept.
pub(crate) fn model_leaf(model: &str) -> &str {
  let leaf = model.rsplit('/').next().expect("rsplit yields at least one piece");
  let Some(inner) = leaf.strip_suffix(')') else {
    return leaf;
  };
  let Some(open) = inner.rfind('(') else {
    return leaf;
  };
  if inner[open + 1..].contains(')') {
    return leaf;
  }
  leaf[..open].trim_end()
}

/// The name a model's rows group on: `_model_leaf(model).casefold()`. The full case fold of Unicode's `CaseFolding`
/// table is what `str.casefold` applies, so names whose letters the upper-then-lower round trip spells differently
/// (`ı` stays itself, `ẞ` folds to `ss`) group the way the Python page groups them.
pub(crate) fn canonical_model(model: &str) -> String {
  caseless::default_case_fold_str(model_leaf(model))
}

/// `_compact`: a count in K, M or B with at most two decimals (`1.23M`, `456K`), else the count with comma groups.
pub(crate) fn compact(count: Count) -> String {
  let value = count.as_f64();
  for (cut, suffix) in [(1e9, "B"), (1e6, "M"), (1e3, "K")] {
    if value.abs() >= cut {
      let text = format!("{:.2}", value / cut);
      return format!("{}{suffix}", text.trim_end_matches('0').trim_end_matches('.'));
    }
  }
  group_thousands(count)
}

/// Python's `f"{int(value):,}"`: `int()` truncates toward zero, then the digits go in groups of three, with a leading
/// minus for a negative value.
pub(crate) fn group_thousands(value: Count) -> String {
  let whole = match value {
    Count::Int(number) => number,
    Count::Real(number) => number as i64,
  };
  let digits = whole.unsigned_abs().to_string();
  let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
  for (index, digit) in digits.chars().enumerate() {
    if index > 0 && (digits.len() - index).is_multiple_of(3) {
      grouped.push(',');
    }
    grouped.push(digit);
  }
  if whole < 0 { format!("-{grouped}") } else { grouped }
}

/// The count as a JSON number keeping its Python type: an integer stays an integer, a real prints with its decimal
/// point.
pub(crate) fn count_json(count: Count) -> Value {
  match count {
    Count::Int(number) => json!(number),
    Count::Real(number) => json!(number),
  }
}

/// The sum of the counts, as Python's `sum()` of the rows' values gives it.
pub(crate) fn sum_counts<'a>(items: impl Iterator<Item = &'a Count>, label: &'static str) -> anyhow::Result<Count> {
  let mut sum = Count::Int(0);
  for item in items {
    sum = sum.add(*item, label)?;
  }
  Ok(sum)
}

/// Python's `max(items, key=...)`: the first item of the largest key, so a tie keeps the earlier item.
pub(crate) fn first_max_by<T>(items: &[T], key: impl Fn(&T) -> Count) -> Option<&T> {
  items
    .iter()
    .reduce(|best, item| if key(item).compare(key(best)) == std::cmp::Ordering::Greater { item } else { best })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn compact_counts_in_the_largest_unit_with_two_decimals() {
    let cases: [(Count, &str); 8] = [
      (Count::Int(999), "999"),
      (Count::Int(1_000), "1K"),
      (Count::Int(1_500), "1.5K"),
      (Count::Int(456_000), "456K"),
      (Count::Int(1_000_000), "1M"),
      (Count::Int(1_234_567), "1.23M"),
      (Count::Int(2_000_000_000), "2B"),
      (Count::Real(1_500.5), "1.5K"),
    ];
    for (count, expected) in cases {
      assert_eq!(compact(count), expected, "compact({count:?})");
    }
  }

  #[test]
  fn group_thousands_puts_commas_between_digit_groups() {
    assert_eq!(group_thousands(Count::Int(1_234_567)), "1,234,567");
    assert_eq!(group_thousands(Count::Int(999)), "999");
    assert_eq!(group_thousands(Count::Int(-1_234)), "-1,234");
    assert_eq!(group_thousands(Count::Real(-1_500.5)), "-1,500");
  }

  #[test]
  fn canonical_model_casefolds_the_leaf_the_way_python_does() {
    assert_eq!(canonical_model("zai-org/GLM-5.3-Flash"), "glm-5.3-flash");
    assert_eq!(canonical_model("Kimi-K3 (amd-kimi-k3)"), "kimi-k3");
    assert_eq!(canonical_model("a/b/ß-Model"), "ss-model");
    assert_eq!(canonical_model("ẞ-Model"), "ss-model");
    assert_eq!(canonical_model("ı-Model"), "ı-model");
    assert_eq!(canonical_model("ST-Model"), canonical_model("ﬅ-Model"));
  }

  #[test]
  fn model_leaf_drops_the_path_and_the_provider_suffix() {
    assert_eq!(model_leaf("zai-org/GLM-5.3-Flash"), "GLM-5.3-Flash");
    assert_eq!(model_leaf("Kimi-K3 (amd-kimi-k3)"), "Kimi-K3");
    assert_eq!(model_leaf("a (b) (c)"), "a (b)");
    assert_eq!(model_leaf("claude-fable-5-1"), "claude-fable-5-1");
  }

  #[test]
  fn first_max_by_keeps_the_earlier_of_equal_keys() {
    let items = [("a", 5), ("b", 5), ("c", 1)];
    assert_eq!(first_max_by(&items, |item| Count::Int(item.1)).map(|item| item.0), Some("a"));
  }
}

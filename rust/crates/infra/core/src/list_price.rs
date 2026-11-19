//! API list prices: the standard per-token rates, in USD per million tokens, of the models the usage ledger and the
//! Codex backend name. This module owns every per-token rate in the repository.
//!
//! A row holds the provider's published standard rate in effect on its `read_on` day. Promotions, free periods,
//! off-peak discounts and batch or flex tiers are not rates here. Where a provider prices a long prompt higher, the row
//! takes the base tier, because the ledger keeps no prompt length.
//!
//! The ledger keeps four token categories: uncached input, cache read (the provider's cached input), cache write and
//! output. A row's comment states the rule for a category the ledger merges with another the provider prices
//! differently.

/// One model's standard rates, USD per million tokens.
pub struct ListPrice {
  /// The model name as the ledger spells it, in lower case.
  pub model: &'static str,
  /// Uncached input.
  pub input: f64,
  /// Cache read: the provider's cached-input rate.
  pub cache_read: f64,
  /// Cache write, or `None` when the provider publishes no cache-write rate.
  pub cache_write: Option<f64>,
  /// Output, reasoning tokens included where the provider bills them as output.
  pub output: f64,
  /// The page the rates were read from.
  pub source_url: &'static str,
  /// The day the page was read, as `YYYY-MM-DD`.
  pub read_on: &'static str,
}

/// The token counts of one model, in tokens.
pub struct TokenCounts {
  /// Uncached input.
  pub input: f64,
  /// Cache read.
  pub cache_read: f64,
  /// Cache write.
  pub cache_write: f64,
  /// Output.
  pub output: f64,
}

impl ListPrice {
  /// The USD of `tokens` at this row's rates, or `None` when cache writes are counted and the row has no cache-write
  /// rate.
  pub fn usd(&self, tokens: TokenCounts) -> Option<f64> {
    let cache_write_rate = match self.cache_write {
      Some(rate) => rate,
      None if tokens.cache_write == 0.0 => 0.0,
      None => return None,
    };
    let dollars = tokens.input * self.input
      + tokens.cache_read * self.cache_read
      + tokens.cache_write * cache_write_rate
      + tokens.output * self.output;
    Some(dollars / 1_000_000.0)
  }
}

/// The row of `model`, matched on its lower-case name, or `None` when the table has no row for it.
pub fn lookup(model: &str) -> Option<&'static ListPrice> {
  let lowered = model.to_lowercase();
  LIST_PRICES.iter().find(|price| price.model == lowered)
}

const ANTHROPIC: &str = "https://platform.claude.com/docs/en/about-claude/pricing";
const GOOGLE: &str = "https://aistudio.google.com/docs/pricing";
const DEEPSEEK: &str = "https://api-docs.deepseek.com/quick_start/pricing";
const ZHIPU: &str = "https://docs.z.ai/guides/overview/pricing";
const OPENAI: &str = "https://developers.openai.com/api/docs/pricing";
const KIMI: &str = "https://platform.kimi.ai/docs/pricing/chat";
const DEVIN: &str = "https://docs.devin.ai/desktop/models";
const READ_ON: &str = "2026-11-19";

/// Every priced model. A ledger model with no row reads N/A on the usage page. `lookup` is the table's only reader
/// outside this module.
///
/// Anthropic: the ledger adds 5-minute and 1-hour cache writes into one count, so every cache write is priced at the
/// 5-minute rate. A 1-hour write costs 1.6 times that rate on these rows, so the 1-hour share is understated.
/// Claude Haiku 5.5 prices a prompt over 100,000 tokens higher. The ledger keeps no prompt length, so every Haiku 5.5
/// token is priced at the up-to-100,000 rate, which understates the longer prompts.
static LIST_PRICES: &[ListPrice] = &[
  ListPrice {
    model: "claude-fable-5-2",
    input: 8.00,
    cache_read: 0.20,
    cache_write: Some(10.00),
    output: 40.00,
    source_url: ANTHROPIC,
    read_on: READ_ON,
  },
  ListPrice {
    model: "claude-fable-5-1",
    input: 10.00,
    cache_read: 0.25,
    cache_write: Some(12.50),
    output: 50.00,
    source_url: ANTHROPIC,
    read_on: READ_ON,
  },
  ListPrice {
    model: "claude-opus-5-5",
    input: 4.00,
    cache_read: 0.20,
    cache_write: Some(5.00),
    output: 20.00,
    source_url: ANTHROPIC,
    read_on: READ_ON,
  },
  ListPrice {
    model: "claude-sonnet-5-5",
    input: 2.00,
    cache_read: 0.10,
    cache_write: Some(2.50),
    output: 10.00,
    source_url: ANTHROPIC,
    read_on: READ_ON,
  },
  ListPrice {
    model: "claude-haiku-5-5",
    input: 0.10,
    cache_read: 0.01,
    cache_write: Some(0.125),
    output: 0.50,
    source_url: ANTHROPIC,
    read_on: READ_ON,
  },
  // Google's rates for the text, image and video input of the standard paid tier. Context-cache storage fees are not
  // in these rates, and the page publishes no cache-write rate for these models.
  ListPrice {
    model: "gemini-3.8-flash",
    input: 0.75,
    cache_read: 0.075,
    cache_write: None,
    output: 3.75,
    source_url: GOOGLE,
    read_on: READ_ON,
  },
  ListPrice {
    model: "gemini-4-argon",
    input: 2.00,
    cache_read: 0.10,
    cache_write: None,
    output: 10.00,
    source_url: GOOGLE,
    read_on: READ_ON,
  },
  ListPrice {
    model: "gemini-4-carbon",
    input: 2.00,
    cache_read: 0.10,
    cache_write: None,
    output: 10.00,
    source_url: GOOGLE,
    read_on: READ_ON,
  },
  ListPrice {
    model: "gemini-4-hydrogen",
    input: 1.50,
    cache_read: 0.075,
    cache_write: None,
    output: 7.50,
    source_url: GOOGLE,
    read_on: READ_ON,
  },
  // DeepSeek's peak-hour rates, which the page gives as its standard rates. Off-peak rates are half these and are not
  // used.
  ListPrice {
    model: "deepseek-flash",
    input: 0.30,
    cache_read: 0.006,
    cache_write: None,
    output: 1.20,
    source_url: DEEPSEEK,
    read_on: READ_ON,
  },
  // The page gives no date. Its rates are the table's.
  ListPrice {
    model: "glm-5.3",
    input: 1.40,
    cache_read: 0.26,
    cache_write: None,
    output: 4.40,
    source_url: ZHIPU,
    read_on: READ_ON,
  },
  ListPrice {
    model: "glm-5.3-flash",
    input: 0.15,
    cache_read: 0.03,
    cache_write: None,
    output: 0.50,
    source_url: ZHIPU,
    read_on: READ_ON,
  },
  ListPrice {
    model: "kimi-k3.1-pro",
    input: 2.00,
    cache_read: 0.20,
    cache_write: Some(2.00),
    output: 10.00,
    source_url: KIMI,
    read_on: READ_ON,
  },
  // OpenAI's short-context rates, for prompts up to 272,000 input tokens. The ledger keeps no prompt length, so a
  // longer prompt is priced at these rates too.
  ListPrice {
    model: "gpt-6-astra",
    input: 10.00,
    cache_read: 1.00,
    cache_write: None,
    output: 50.00,
    source_url: OPENAI,
    read_on: READ_ON,
  },
  ListPrice {
    model: "gpt-6-luna",
    input: 0.10,
    cache_read: 0.01,
    cache_write: None,
    output: 0.50,
    source_url: OPENAI,
    read_on: READ_ON,
  },
  ListPrice {
    model: "gpt-6.1-sol",
    input: 2.00,
    cache_read: 0.10,
    cache_write: None,
    output: 10.00,
    source_url: OPENAI,
    read_on: READ_ON,
  },
  ListPrice {
    model: "gpt-5.5",
    input: 5.00,
    cache_read: 0.50,
    cache_write: None,
    output: 30.00,
    source_url: OPENAI,
    read_on: READ_ON,
  },
  ListPrice {
    model: "gpt-5.4",
    input: 2.50,
    cache_read: 0.25,
    cache_write: None,
    output: 15.00,
    source_url: OPENAI,
    read_on: READ_ON,
  },
  ListPrice {
    model: "gpt-5.4-mini",
    input: 0.75,
    cache_read: 0.075,
    cache_write: None,
    output: 4.50,
    source_url: OPENAI,
    read_on: READ_ON,
  },
  ListPrice {
    model: "gpt-5.3-codex",
    input: 1.75,
    cache_read: 0.175,
    cache_write: None,
    output: 14.00,
    source_url: OPENAI,
    read_on: READ_ON,
  },
  // Carried over unchanged from the Codex module's rate card of 2026-06-01 and not read again. The page read on
  // 2026-11-19 lists other rates for these names: gpt-5.6-sol $4.00 / $0.40 / $20.00 as a promotion until 2026-11-30,
  // gpt-5.6-terra $2.00 / $0.20 / $12.00, and gpt-5.6-luna $0.20 / $0.02 / $1.20.
  ListPrice {
    model: "gpt-5.6-sol",
    input: 5.00,
    cache_read: 0.50,
    cache_write: None,
    output: 30.00,
    source_url: OPENAI,
    read_on: "2026-06-01",
  },
  ListPrice {
    model: "gpt-5.6-terra",
    input: 2.50,
    cache_read: 0.25,
    cache_write: None,
    output: 15.00,
    source_url: OPENAI,
    read_on: "2026-06-01",
  },
  ListPrice {
    model: "gpt-5.6-luna",
    input: 1.00,
    cache_read: 0.10,
    cache_write: None,
    output: 6.00,
    source_url: OPENAI,
    read_on: "2026-06-01",
  },
  // Devin's rates after the SWE-2 Flash free period, which ends 2026-12-04 18:00 UTC. The three SWE-2 Flash variants
  // share them.
  ListPrice {
    model: "swe-2-flash-medium",
    input: 0.10,
    cache_read: 0.01,
    cache_write: Some(0.125),
    output: 0.50,
    source_url: DEVIN,
    read_on: READ_ON,
  },
];

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn each_model_has_one_row() {
    let mut names: Vec<&str> = LIST_PRICES.iter().map(|price| price.model).collect();
    names.sort_unstable();
    let distinct = names.len();
    names.dedup();
    assert_eq!(names.len(), distinct, "a model name appears in two rows, so the lookup would shadow one of them");
  }
}

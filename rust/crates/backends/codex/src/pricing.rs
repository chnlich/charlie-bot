//! Codex token cost in USD, priced from the shared API list-price table in `charliebot_core::list_price`.

use charliebot_core::list_price::{self, ListPrice, TokenCounts};
use serde_json::{Map, Value};

use super::usage_fields;

/// The cost of one usage object, or `None` when the model is unpriced or unnamed. Usage is read only for a priced
/// model.
pub fn calculate_codex_usage_cost_usd(model: &str, usage: &Map<String, Value>) -> anyhow::Result<Option<f64>> {
  let Some(price) = list_price::lookup(model) else {
    return Ok(None);
  };
  let input = number(usage, usage_fields::INPUT_TOKENS)?;
  let cached = number(usage, usage_fields::CACHED_INPUT_TOKENS)?;
  let output = number(usage, usage_fields::OUTPUT_TOKENS)?;
  Ok(priced(price, input, cached, output))
}

/// The cost of token counts, or `None` when the model is unpriced or unnamed.
pub fn cost_usd(model: &str, input: f64, cached: f64, output: f64) -> Option<f64> {
  list_price::lookup(model).and_then(|price| priced(price, input, cached, output))
}

/// `input` includes the cached tokens, so the uncached part is `input - cached`. Codex has no cache-write tokens.
fn priced(price: &ListPrice, input: f64, cached: f64, output: f64) -> Option<f64> {
  price.usd(TokenCounts { input: input - cached, cache_read: cached, cache_write: 0.0, output })
}

fn number(usage: &Map<String, Value>, key: &str) -> anyhow::Result<f64> {
  match usage.get(key) {
    None => Ok(0.0),
    Some(Value::Number(number)) => {
      number.as_f64().ok_or_else(|| anyhow::anyhow!("codex usage {key} must be a number, got {number}"))
    }
    Some(other) => anyhow::bail!("codex usage {key} must be a number, got {other}"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn cached_input_is_billed_at_the_cached_rate() {
    let usage = json!({"input_tokens": 1000, "cached_input_tokens": 400, "output_tokens": 20});
    let cost = calculate_codex_usage_cost_usd("gpt-5.5", usage.as_object().unwrap()).unwrap();
    assert!((cost.unwrap() - 0.0038).abs() < 1e-12);
  }

  #[test]
  fn gpt_6_2_ledger_ids_are_billed_at_their_list_rates() {
    let usage = json!({"input_tokens": 1000000, "cached_input_tokens": 400000, "output_tokens": 1000000});
    let astra = calculate_codex_usage_cost_usd("gpt-6.2-astra", usage.as_object().unwrap()).unwrap();
    assert!((astra.unwrap() - 44.96).abs() < 1e-9);
    let sol = calculate_codex_usage_cost_usd("gpt-6.2-sol", usage.as_object().unwrap()).unwrap();
    assert!((sol.unwrap() - 11.24).abs() < 1e-9);
  }

  #[test]
  fn unpriced_and_unnamed_models_have_no_cost() {
    assert_eq!(cost_usd("gpt-unknown", 1.0, 0.0, 0.0), None);
    assert_eq!(cost_usd("", 1.0, 0.0, 0.0), None);
    assert!(cost_usd("GPT-5.5", 1.0, 0.0, 0.0).is_some());
  }
}

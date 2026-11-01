//! Codex token cost in USD.

use serde_json::{Map, Value};

/// Dollars per million tokens: (input, cached input, output). Rates from the 2026-06-01 rate card.
const USD_PER_1M_TOKENS: &[(&str, (f64, f64, f64))] = &[
  ("gpt-5.6-sol", (5.00, 0.50, 30.0)),
  ("gpt-5.6-terra", (2.50, 0.25, 15.0)),
  ("gpt-5.6-luna", (1.00, 0.10, 6.00)),
  ("gpt-5.5", (5.00, 0.50, 30.0)),
  ("gpt-5.4", (2.50, 0.25, 15.0)),
  ("gpt-5.4-mini", (0.75, 0.075, 4.50)),
  ("gpt-5.3-codex", (1.75, 0.175, 14.0)),
];

/// The cost of one usage object, or `None` when the model is unpriced or unnamed. Usage is read only for a priced
/// model.
pub fn calculate_codex_usage_cost_usd(model: &str, usage: &Map<String, Value>) -> anyhow::Result<Option<f64>> {
  let Some(rates) = rates_for(model) else {
    return Ok(None);
  };
  let input = number(usage, "input_tokens")?;
  let cached = number(usage, "cached_input_tokens")?;
  let output = number(usage, "output_tokens")?;
  Ok(Some(priced(rates, input, cached, output)))
}

/// The cost of token counts, or `None` when the model is unpriced or unnamed.
pub fn cost_usd(model: &str, input: f64, cached: f64, output: f64) -> Option<f64> {
  rates_for(model).map(|rates| priced(rates, input, cached, output))
}

fn rates_for(model: &str) -> Option<(f64, f64, f64)> {
  if model.is_empty() {
    return None;
  }
  let lowered = model.to_lowercase();
  USD_PER_1M_TOKENS.iter().find(|(name, _)| *name == lowered).map(|(_, rates)| *rates)
}

fn priced((input_rate, cached_rate, output_rate): (f64, f64, f64), input: f64, cached: f64, output: f64) -> f64 {
  ((input - cached) * input_rate + cached * cached_rate + output * output_rate) / 1_000_000.0
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
  fn unpriced_and_unnamed_models_have_no_cost() {
    assert_eq!(cost_usd("gpt-unknown", 1.0, 0.0, 0.0), None);
    assert_eq!(cost_usd("", 1.0, 0.0, 0.0), None);
    assert!(cost_usd("GPT-5.5", 1.0, 0.0, 0.0).is_some());
  }
}

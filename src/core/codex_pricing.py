"""Codex pricing helpers."""

from collections.abc import Mapping
from typing import Any

# 2026-06-01: Codex rate-card credits / 25 = OpenAI API list price;
# update if OpenAI changes pricing.
_CODEX_USD_PER_1M_TOKENS: dict[str, tuple[float, float, float]] = {
    "gpt-5.5": (5.00, 0.50, 30.0),
    "gpt-5.4": (2.50, 0.25, 15.0),
    "gpt-5.4-mini": (0.75, 0.075, 4.50),
    "gpt-5.3-codex": (1.75, 0.175, 14.0),
}


def calculate_codex_usage_cost_usd(model: str | None, usage: Mapping[str, Any]) -> float | None:
  """Return Codex token cost in USD, or None when the model is not priced."""
  if not model:
    return None

  rates = _CODEX_USD_PER_1M_TOKENS.get(model.lower())
  if rates is None:
    return None

  input_tokens = usage.get("input_tokens", 0)
  cached_input_tokens = usage.get("cached_input_tokens", 0)
  output_tokens = usage.get("output_tokens", 0)
  input_rate, cached_input_rate, output_rate = rates
  return (
      (input_tokens - cached_input_tokens) * input_rate + cached_input_tokens * cached_input_rate +
      output_tokens * output_rate) / 1_000_000

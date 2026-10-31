# charliebot-gemini

The `gemini` backend type: the gemini CLI in stream-json mode, and the two legacy credential keys (`gemini_api_key`
and `gemini_model`) that the config migrates to `gemini.api_key` and `gemini.model`.

## Public API

- `charliebot_gemini::register` and `charliebot_gemini::register_with_host`, which register the `gemini` type, its
  option schema and its two legacy credential keys
- `charliebot_gemini::factory::GeminiFactory`

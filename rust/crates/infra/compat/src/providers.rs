//! The provider hosts and endpoints the outbound HTTP and WebSocket clients call.
//!
//! One module owns each provider's host, so a caller builds its path under the shared name instead of spelling
//! the host again.

/// The Google Generative Language host: the REST `generateContent` call, the live-transcription WebSocket and
/// the Gemini-routed OpenAI-compatible upstream all reach it.
pub const GEMINI_HOST: &str = "generativelanguage.googleapis.com";

/// The Google REST base: the path a `generateContent` call appends its model to.
pub fn gemini_rest_base() -> String {
  format!("https://{GEMINI_HOST}/v1beta")
}

/// The Google live-transcription WebSocket endpoint (`BidiGenerateContent`).
pub fn gemini_live_transcription_websocket() -> String {
  format!("wss://{GEMINI_HOST}/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent")
}

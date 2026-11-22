//! The provider hosts and endpoints the outbound HTTP and WebSocket clients call.
//!
//! One module owns each provider's host, so a caller builds its path under the shared name instead of spelling
//! the host again.

/// The Google Generative Language host: the REST `generateContent` call and the Gemini-routed OpenAI-compatible
/// upstream both reach it.
pub const GEMINI_HOST: &str = "generativelanguage.googleapis.com";

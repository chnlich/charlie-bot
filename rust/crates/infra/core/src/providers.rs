//! The provider hosts that code matches by value.
//!
//! The OpenAI-compatible backend compares a configured `api_base` host with a host here to choose its stream rules.
//! The backend-check text scan flags each host here when instruction text names it.

/// The Google Generative Language host. A configured `api_base` on this host takes the Gemini stream rules.
pub const GEMINI_HOST: &str = "generativelanguage.googleapis.com";

//! Signed run tokens: the agent caller identity (`src/runtime/run_token.py`).
//!
//! A token is `<payload>.<signature>`, both unpadded base64url. The payload is the compact, key-sorted
//! JSON of `{"agent", "run_id", "session_id"}`; the signature is HMAC-SHA256 over the payload's base64
//! text, keyed by the operator access key. A token carries no expiry: validity is the signature here
//! and the referenced Run being active, which the caller-identity dependency checks per route.

use base64::Engine;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::{DecodePaddingMode, general_purpose};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::text_repr;

type HmacSha256 = Hmac<Sha256>;

/// `constants.RUN_TOKEN_ENV`: the environment variable that carries a run's token into its agent process, and the
/// bearer the CLIs present.
pub const RUN_TOKEN_ENV: &str = "CHARLIEBOT_RUN_TOKEN";

/// `constants.SESSION_ID_ENV_VAR`: the server writes the master's session id into every spawned process environment,
/// and the CLIs read it back.
pub const SESSION_ID_ENV: &str = "CHARLIEBOT_SESSION_ID";

/// `base64.urlsafe_b64decode` tolerance: missing padding and nonzero trailing bits both decode.
const LENIENT_URL_SAFE: GeneralPurpose = GeneralPurpose::new(
  &base64::alphabet::URL_SAFE,
  GeneralPurposeConfig::new()
    .with_decode_padding_mode(DecodePaddingMode::Indifferent)
    .with_decode_allow_trailing_bits(true),
);

/// The identity one run token binds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunTokenClaims {
  pub session_id: String,
  pub run_id: String,
  pub agent: String,
}

/// Any verification failure; the message is the Python `RunTokenError` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTokenError(pub &'static str);

impl std::fmt::Display for RunTokenError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.0)
  }
}

impl std::error::Error for RunTokenError {}

/// The unpadded base64url wire form.
pub fn b64url_encode(raw: &[u8]) -> String {
  general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// The decode side of [`b64url_encode`].
///
/// CPython's decoder also skips characters outside the alphabet before decoding; this decoder refuses
/// them, so a token padded with junk characters fails here where Python could accept it.
pub fn b64url_decode(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
  LENIENT_URL_SAFE.decode(text.trim_end_matches('='))
}

fn signature_of(payload_b64: &str, signing_key: &str) -> Vec<u8> {
  let mut mac = HmacSha256::new_from_slice(signing_key.as_bytes()).expect("HMAC takes a key of any length");
  mac.update(payload_b64.as_bytes());
  mac.finalize().into_bytes().to_vec()
}

/// Sign `claims` with the operator access key.
pub fn sign_run_token(claims: &RunTokenClaims, signing_key: &str) -> Result<String, RunTokenError> {
  if signing_key.is_empty() {
    return Err(RunTokenError("cannot sign a run token without a configured charliebot.access_key"));
  }
  let payload = json!({"session_id": claims.session_id, "run_id": claims.run_id, "agent": claims.agent});
  let payload_b64 = b64url_encode(text_repr::json_dumps(&payload, &text_repr::RUN_TOKEN_PAYLOAD_JSON).as_bytes());
  let signature = signature_of(&payload_b64, signing_key);
  Ok(format!("{payload_b64}.{}", b64url_encode(&signature)))
}

/// Verify the signature and the payload shape; the signature compares in constant time.
pub fn verify_run_token(token: &str, signing_key: &str) -> Result<RunTokenClaims, RunTokenError> {
  if signing_key.is_empty() {
    return Err(RunTokenError("run token presented but no signing key is configured"));
  }
  let (payload_b64, signature_b64) = token.split_once('.').ok_or(RunTokenError("malformed run token"))?;
  let expected = signature_of(payload_b64, signing_key);
  let presented = b64url_decode(signature_b64).map_err(|_| RunTokenError("malformed run token signature"))?;
  if !bool::from(presented.ct_eq(&expected)) {
    return Err(RunTokenError("run token signature mismatch"));
  }
  let payload_bytes = b64url_decode(payload_b64).map_err(|_| RunTokenError("malformed run token payload"))?;
  let payload: Value =
    serde_json::from_slice(&payload_bytes).map_err(|_| RunTokenError("malformed run token payload"))?;
  let Value::Object(payload) = payload else {
    return Err(RunTokenError("run token payload must be an object"));
  };
  let field = |key: &str| payload.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string);
  match (field("session_id"), field("run_id"), field("agent")) {
    (Some(session_id), Some(run_id), Some(agent)) => Ok(RunTokenClaims { session_id, run_id, agent }),
    _ => Err(RunTokenError("run token payload must bind session_id, run_id and agent")),
  }
}

/// The bearer credential of an `Authorization` header value, or `""`.
pub fn bearer_from_authorization(header_value: Option<&str>) -> &str {
  match header_value {
    Some(value) => value.strip_prefix("Bearer ").unwrap_or(""),
    None => "",
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn claims() -> RunTokenClaims {
    RunTokenClaims { session_id: "s-1".to_string(), run_id: "r-1".to_string(), agent: "worker".to_string() }
  }

  #[test]
  fn a_signed_token_verifies_and_matches_the_python_wire_form() {
    let token = sign_run_token(&claims(), "k").unwrap();
    // `run_token.sign_run_token(RunTokenClaims("s-1", "r-1", "worker"), "k")` in the Python baseline.
    assert_eq!(
      token,
      "eyJhZ2VudCI6IndvcmtlciIsInJ1bl9pZCI6InItMSIsInNlc3Npb25faWQiOiJzLTEifQ.oK3czuAVKmjjcdikPLcWwQCe_8bZzTAXJyirCd5LGtg"
    );
    assert_eq!(verify_run_token(&token, "k").unwrap(), claims());
  }

  #[test]
  fn a_bad_signature_is_refused() {
    let token = sign_run_token(&claims(), "k").unwrap();
    assert_eq!(verify_run_token(&token, "other-key").unwrap_err().0, "run token signature mismatch");
    let (payload, _) = token.split_once('.').unwrap();
    assert_eq!(verify_run_token(&format!("{payload}.AAAA"), "k").unwrap_err().0, "run token signature mismatch");
    assert_eq!(verify_run_token("no-dot", "k").unwrap_err().0, "malformed run token");
    assert_eq!(verify_run_token(&token, "").unwrap_err().0, "run token presented but no signing key is configured");
  }

  #[test]
  fn a_validly_signed_payload_must_bind_every_claim() {
    let payload_b64 = b64url_encode(br#"{"session_id":"s","run_id":"","agent":"a"}"#);
    let token = format!("{payload_b64}.{}", b64url_encode(&signature_of(&payload_b64, "k")));
    assert_eq!(
      verify_run_token(&token, "k").unwrap_err().0,
      "run token payload must bind session_id, run_id and agent"
    );
    let payload_b64 = b64url_encode(b"[1]");
    let token = format!("{payload_b64}.{}", b64url_encode(&signature_of(&payload_b64, "k")));
    assert_eq!(verify_run_token(&token, "k").unwrap_err().0, "run token payload must be an object");
  }

  #[test]
  fn bearer_parsing_takes_the_exact_prefix() {
    assert_eq!(bearer_from_authorization(Some("Bearer abc")), "abc");
    assert_eq!(bearer_from_authorization(Some("bearer abc")), "");
    assert_eq!(bearer_from_authorization(None), "");
  }
}

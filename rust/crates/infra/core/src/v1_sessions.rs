//! The v1 session conversion directions the store's startup refusal and metadata decoder quote.

/// The deleted v1 session conversion script.
pub const CONVERSION_SCRIPT: &str = "scripts/v1_session_conversion.py";

/// Directions for restoring the deleted v1 session conversion script.
pub const V1_SESSION_CONVERSION_GUIDANCE: &str = "the conversion script was deleted. From the repository root, run `git show d4021ce:scripts/v1_session_conversion.py > scripts/v1_session_conversion.py` to restore it.";

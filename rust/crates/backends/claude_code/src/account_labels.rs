//! The label rule of the `accounts.claude` entries: a label that product UI shows holds no rejected character.

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::ui_text::check_label;

use crate::claude_config::AccountsConfig;

/// Refuse an account whose label holds a character that product UI text rejects. The config load runs this check,
/// so a rejected label keeps the previous config.
pub fn check_labels(cfg: &CharlieBotConfig) -> Result<(), String> {
  for account in &cfg.section::<AccountsConfig>("accounts").claude {
    check_label(&format!("accounts.claude[{}].label", account.label), &account.label)?;
  }
  Ok(())
}

//! The checks' public surface: the check names, the violation a check reports, and the checker for a reply or a page.

use std::fs;
use std::path::Path;

use anyhow::{Context as AnyhowContext, anyhow};
use charliebot_core::config::sessions_dir_in;

use crate::config::UserTextChecksConfig;
use crate::page::parse_page;
use crate::sessions::SessionIndex;
use crate::text::line_blocks;

/// The internal-id check name shared by artifact assertions and web-reply refusals.
pub const TRACKER_ID_NEARBY: &str = "tracker-id-nearby";
/// The session-link check name shared by artifact assertions and web-reply refusals.
pub const SESSION_LINKED: &str = "session-linked";
/// The renderable-link check name shared by artifact assertions and web-reply refusals.
pub const LINK_RENDERABLE: &str = "link-renderable";
/// The commit-hash check name: an identifier must stand beside a readable name that says what it identifies.
pub const COMMIT_NAMED: &str = "commit-named";
/// The vocabulary check name: words must be the user's own or everyday words, with rule-defined agent terms kept out.
pub const VOCABULARY_RULE_TERM: &str = "vocabulary-rule-term";
/// The three checks, in the order each entry point reports them.
pub const CHECK_NAMES: [&str; 3] = [TRACKER_ID_NEARBY, SESSION_LINKED, LINK_RENDERABLE];

const TRACKER_FIX: &str = "write the Linear id right next to the internal id";
const TRACKER_PURPOSE: &str = "This lets the reader identify the work item.";
const SESSION_FIX: &str =
  "write the id as [<readable alias> · <first 8 characters>](<CharlieBot URL>/?session=<full id>)";
const SESSION_PURPOSE: &str = "This lets the reader open the correct session.";
const LINK_FIX: &str = "link an HTML page, or the directory that holds the file";
const LINK_PURPOSE: &str = "This gives the reader a rendered result.";
const COMMIT_FIX: &str = "name the commit by what it changes, with its hash in the link";
const COMMIT_PURPOSE: &str = "This lets the reader know what changed.";
const VOCABULARY_FIX: &str = "use everyday words instead of rule-defined agent terms";
const VOCABULARY_PURPOSE: &str = "This keeps agent jargon out of text for the user.";

/// Rule-defined agent terms that must stay out of text for the user (Vocabulary).
pub const RULE_DEFINED_TERMS: &[&str] = &[
  "读回",
  "主线",
  "子线",
  "本线",
  "这条线",
  "上级会话",
  "父节点",
  "子会话",
  "子节点",
  "冷读",
  "分歧",
  "取舍",
  "接手",
  "起飞",
  "Waiting on you",
  "审批人",
  "放行",
];

/// One user-text violation and the fixed correction shown at the writing entry point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextViolation {
  pub check: &'static str,
  pub snippet: String,
  pub block: Option<String>,
  fix: &'static str,
  purpose: &'static str,
}

/// The three checks and the session directory names read once for one input batch.
#[derive(Clone)]
pub struct UserTextChecks {
  pub(crate) config: UserTextChecksConfig,
  pub(crate) sessions: SessionIndex,
}

impl UserTextChecks {
  /// The checks for `home`: the configured patterns, and the session directory names that resolve session ids.
  pub fn new(config: &UserTextChecksConfig, home: &Path) -> anyhow::Result<UserTextChecks> {
    let sessions_path = sessions_dir_in(home);
    let mut names = Vec::new();
    match fs::read_dir(&sessions_path) {
      Ok(entries) => {
        for entry in entries {
          let entry = entry.with_context(|| format!("could not read an entry in {}", sessions_path.display()))?;
          if entry.file_type().with_context(|| format!("could not inspect {}", entry.path().display()))?.is_dir() {
            names.push(
              entry
                .file_name()
                .into_string()
                .map_err(|name| anyhow!("session directory name is not Unicode: {name:?}"))?,
            );
          }
        }
      }
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
      Err(error) => {
        return Err(error)
          .with_context(|| format!("could not read session directories at {}", sessions_path.display()));
      }
    }
    names.sort();
    Ok(UserTextChecks { config: config.clone(), sessions: SessionIndex { names } })
  }

  /// The violations in one web reply, found in its one-line blocks and at its link targets.
  pub fn check_reply(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let mut violations = self.tracker_violations(line_blocks(text))?;
    violations.extend(self.reply_session_violations(text)?);
    violations.extend(self.reply_link_violations(text)?);
    violations.extend(self.reply_commit_violations(text)?);
    violations.extend(self.reply_vocabulary_violations(text)?);
    Ok(violations)
  }

  /// The violations in one rendered HTML page, found in its configured text blocks and at its anchor targets.
  pub fn check_page(&self, html: &str) -> anyhow::Result<Vec<TextViolation>> {
    let page = parse_page(html)?;
    let blocks = page.blocks.iter().enumerate().map(|(index, text)| (index, text.as_str())).collect::<Vec<_>>();
    let mut violations = self.tracker_violations(blocks)?;
    violations.extend(self.page_session_violations(&page)?);
    violations.extend(self.page_link_violations(&page)?);
    Ok(violations)
  }
}

impl TextViolation {
  pub(crate) fn new(check: &'static str, snippet: String, block: Option<String>) -> TextViolation {
    let (fix, purpose) = match check {
      TRACKER_ID_NEARBY => (TRACKER_FIX, TRACKER_PURPOSE),
      SESSION_LINKED => (SESSION_FIX, SESSION_PURPOSE),
      LINK_RENDERABLE => (LINK_FIX, LINK_PURPOSE),
      COMMIT_NAMED => (COMMIT_FIX, COMMIT_PURPOSE),
      VOCABULARY_RULE_TERM => (VOCABULARY_FIX, VOCABULARY_PURPOSE),
      other => unreachable!("unknown user-text check {other:?}"),
    };
    TextViolation { check, snippet, block, fix, purpose }
  }
}

/// One page failure as a line of text, with its snippet, containing block, fix and reason.
pub fn page_failure_detail(violation: &TextViolation) -> String {
  format!(
    "snippet {:?}; block {:?}; fix: {}. {}",
    violation.snippet,
    violation.block.as_deref().unwrap_or(""),
    violation.fix,
    violation.purpose
  )
}

/// Web-reply refusals as one line per hit, with each correction's purpose.
pub fn reply_refusal_detail(violations: &[TextViolation]) -> String {
  violations
    .iter()
    .map(|violation| {
      format!("{}: snippet {:?}; fix: {}. {}", violation.check, violation.snippet, violation.fix, violation.purpose)
    })
    .collect::<Vec<_>>()
    .join("\n")
}

//! The GitHub draft target: a draft pull request, or a pull-request or issue comment.
//!
//! The CLI builds and validates the target with this module before it registers, and the server validates the same
//! target again when it writes the event. Both sides share one rule: `Target::parse` reads the object once and names
//! the kind, so the label and the HTTP request work from the named kind.

use serde_json::{Value, json};

/// The target object's field names. The CLI builds the object, the event carries it, and the HTTP request reads it,
/// so every site names the field through these constants.
const REPO: &str = "repo";
const NUMBER: &str = "number";
const TITLE: &str = "title";
const HEAD: &str = "head";
const BASE: &str = "base";

/// One GitHub target: a comment on a pull request or issue, or a new draft pull request.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
  /// A comment on a pull request or issue, named by its number.
  Comment { repo: String, number: u64 },
  /// A new draft pull request from `head` into `base`.
  DraftPullRequest { repo: String, title: String, head: String, base: String },
}

impl Target {
  /// The target one JSON object holds. An object that carries `number` is a comment, and an object without it is a
  /// draft pull request that needs `title`, `head` and `base`. Any other set of fields is an error that names what is
  /// wrong.
  pub fn parse(target: &Value) -> Result<Target, String> {
    let Some(object) = target.as_object() else {
      return Err("the GitHub target must be an object".to_string());
    };
    let number = object.get(NUMBER);
    let allowed: &[&str] = if number.is_some() { &[REPO, NUMBER] } else { &[REPO, TITLE, HEAD, BASE] };
    if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
      return Err(format!("the GitHub target has no field {unknown}"));
    }
    let repo = object
      .get(REPO)
      .and_then(Value::as_str)
      .filter(|repo| valid_repo(repo))
      .ok_or_else(|| "target.repo must name owner/repository".to_string())?
      .to_string();
    if let Some(number) = number {
      let number = number
        .as_u64()
        .filter(|number| *number > 0)
        .ok_or_else(|| "target.number must be a positive comment number".to_string())?;
      return Ok(Target::Comment { repo, number });
    }
    let title = object
      .get(TITLE)
      .and_then(Value::as_str)
      .filter(|title| !title.trim().is_empty())
      .ok_or_else(|| "target.title must be a non-empty pull-request title".to_string())?
      .to_string();
    let head = object
      .get(HEAD)
      .and_then(Value::as_str)
      .filter(|head| valid_branch(head))
      .ok_or_else(|| "target.head must be a pull-request branch".to_string())?
      .to_string();
    let base = object
      .get(BASE)
      .and_then(Value::as_str)
      .filter(|base| valid_branch(base))
      .ok_or_else(|| "target.base must be a pull-request branch".to_string())?
      .to_string();
    Ok(Target::DraftPullRequest { repo, title, head, base })
  }

  /// The comment target for `repo` and `number`, validated as an object that carries `number` alone.
  pub fn comment(repo: &str, number: u64) -> Result<Target, String> {
    Self::parse(&json!({(REPO): repo, (NUMBER): number}))
  }

  /// The draft pull-request target for these parts, validated as an object that carries `title`, `head` and `base`.
  pub fn draft_pull_request(repo: &str, title: &str, head: &str, base: &str) -> Result<Target, String> {
    Self::parse(&json!({(REPO): repo, (TITLE): title, (HEAD): head, (BASE): base}))
  }

  /// The canonical `Value` the event carries.
  pub fn to_value(&self) -> Value {
    match self {
      Target::Comment { repo, number } => json!({(REPO): repo, (NUMBER): number}),
      Target::DraftPullRequest { repo, title, head, base } => {
        json!({(REPO): repo, (TITLE): title, (HEAD): head, (BASE): base})
      }
    }
  }

  /// The target's one-line card label.
  pub fn label(&self) -> String {
    match self {
      Target::Comment { repo, number } => format!("GitHub comment on {repo}#{number}"),
      Target::DraftPullRequest { repo, title, head, base } => {
        format!("GitHub draft PR {repo} {head} -> {base}: {title}")
      }
    }
  }

  /// The path and JSON body of the HTTP request that sends `body` to this target.
  pub fn request(&self, body: &str) -> (String, Value) {
    match self {
      Target::Comment { repo, number } => (format!("repos/{repo}/issues/{number}/comments"), json!({"body": body})),
      Target::DraftPullRequest { repo, title, head, base } => (
        format!("repos/{repo}/pulls"),
        json!({(TITLE): title, (HEAD): head, (BASE): base, "body": body, "draft": true}),
      ),
    }
  }
}

/// One GitHub target in canonical form: `{repo, title, head, base}` for a draft pull request,
/// `{repo, number}` for a comment.
pub fn validate_target(target: &Value) -> Result<Value, String> {
  Target::parse(target).map(|target| target.to_value())
}

/// The target's one-line card label. The caller passes a target that `validate_target` accepted; a target it refuses
/// gets a label that names the refusal.
pub fn target_label(target: &Value) -> String {
  match Target::parse(target) {
    Ok(target) => target.label(),
    Err(error) => format!("GitHub target ({error})"),
  }
}

/// `owner/repository`: exactly one slash, both sides made of GitHub's name characters.
fn valid_repo(repo: &str) -> bool {
  match repo.split_once('/') {
    Some((owner, name)) => {
      !owner.is_empty()
        && !name.is_empty()
        && !name.contains('/')
        && owner.chars().all(valid_repo_char)
        && name.chars().all(valid_repo_char)
    }
    None => false,
  }
}

fn valid_repo_char(character: char) -> bool {
  character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
}

/// A head or base: a branch ref, or `<owner>:<branch>` for a branch of another owner's fork. The owner is non-empty and
/// made of repository-name characters; the branch is a branch ref.
fn valid_branch(branch: &str) -> bool {
  match branch.split_once(':') {
    Some((owner, branch_ref)) => {
      !owner.is_empty() && owner.chars().all(valid_repo_char) && valid_branch_ref(branch_ref)
    }
    None => valid_branch_ref(branch),
  }
}

/// A branch ref: non-empty, no whitespace, and free of the characters git refuses.
fn valid_branch_ref(branch: &str) -> bool {
  !branch.is_empty()
    && !branch.chars().any(char::is_whitespace)
    && !branch.chars().any(|character| matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_branch_may_name_another_owners_branch_but_not_a_malformed_owner_or_ref() {
    assert!(valid_branch("chnlich:fix/okta-429-tryagain"));
    assert!(valid_branch("fix/okta-429-tryagain"));
    for branch in [":branch", "owner:", "a:b:c"] {
      assert!(!valid_branch(branch), "{branch}");
    }
  }

  #[test]
  fn a_pull_request_target_canonicalizes_its_four_fields() {
    assert_eq!(
      validate_target(&json!({"repo": "owner/repo", "title": "T", "head": "feature", "base": "main"})).unwrap(),
      json!({"repo": "owner/repo", "title": "T", "head": "feature", "base": "main"})
    );
  }

  #[test]
  fn a_comment_target_keeps_its_number() {
    assert_eq!(
      validate_target(&json!({"repo": "owner/repo", "number": 12})).unwrap(),
      json!({"repo": "owner/repo", "number": 12})
    );
  }

  #[test]
  fn the_label_names_the_pull_request_or_the_comment() {
    assert_eq!(
      target_label(&json!({"repo": "owner/repo", "title": "T", "head": "feature", "base": "main"})),
      "GitHub draft PR owner/repo feature -> main: T"
    );
    assert_eq!(target_label(&json!({"repo": "owner/repo", "number": 12})), "GitHub comment on owner/repo#12");
  }

  #[test]
  fn an_unparseable_repo_number_or_branch_fails() {
    for target in [
      json!({"repo": "owner", "number": 1}),
      json!({"repo": "owner/repo/extra", "number": 1}),
      json!({"repo": "owner /repo", "number": 1}),
      json!({"repo": "owner/repo", "number": 0}),
      json!({"repo": "owner/repo", "number": -1}),
      json!({"repo": "owner/repo", "number": "1"}),
      json!({"repo": "owner/repo", "title": "", "head": "h", "base": "b"}),
      json!({"repo": "owner/repo", "title": "T", "head": "a b", "base": "b"}),
      json!({"repo": "owner/repo", "title": "T", "head": "h"}),
      json!({"repo": "owner/repo", "title": "T", "head": "h", "base": "b", "number": 1}),
    ] {
      assert!(validate_target(&target).is_err(), "{target}");
    }
  }
}

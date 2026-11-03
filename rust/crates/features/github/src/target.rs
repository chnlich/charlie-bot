//! The GitHub draft target: a draft pull request, or a pull-request or issue comment.
//!
//! The CLI validates the flags with this module before it registers, and the server validates the same target again
//! when it writes the event, so both sides share one rule.

use serde_json::{Value, json};

/// Validate one GitHub target and return it in canonical form: `{repo, title, head, base}` for a draft pull request,
/// `{repo, number}` for a comment.
pub fn validate_target(target: &Value) -> Result<Value, String> {
  let Some(object) = target.as_object() else {
    return Err("the GitHub target must be an object".to_string());
  };
  let allowed: &[&str] =
    if object.contains_key("number") { &["repo", "number"] } else { &["repo", "title", "head", "base"] };
  if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
    return Err(format!("the GitHub target has no field {unknown}"));
  }
  let repo = object
    .get("repo")
    .and_then(Value::as_str)
    .filter(|repo| valid_repo(repo))
    .ok_or_else(|| "target.repo must name owner/repository".to_string())?;
  if object.contains_key("number") {
    let number = object
      .get("number")
      .and_then(Value::as_u64)
      .filter(|number| *number > 0)
      .ok_or_else(|| "target.number must be a positive comment number".to_string())?;
    return Ok(json!({"repo": repo, "number": number}));
  }
  let title = object
    .get("title")
    .and_then(Value::as_str)
    .filter(|title| !title.trim().is_empty())
    .ok_or_else(|| "target.title must be a non-empty pull-request title".to_string())?;
  let head = object
    .get("head")
    .and_then(Value::as_str)
    .filter(|head| valid_branch(head))
    .ok_or_else(|| "target.head must be a pull-request branch".to_string())?;
  let base = object
    .get("base")
    .and_then(Value::as_str)
    .filter(|base| valid_branch(base))
    .ok_or_else(|| "target.base must be a pull-request branch".to_string())?;
  Ok(json!({"repo": repo, "title": title, "head": head, "base": base}))
}

/// The target's one-line card label.
pub fn target_label(target: &Value) -> String {
  let repo = target.get("repo").and_then(Value::as_str).unwrap_or_default();
  match (
    target.get("number").and_then(Value::as_u64),
    target.get("head").and_then(Value::as_str),
    target.get("base").and_then(Value::as_str),
  ) {
    (Some(number), _, _) => format!("GitHub comment on {repo}#{number}"),
    (_, Some(head), Some(base)) => {
      let title = target.get("title").and_then(Value::as_str).unwrap_or_default();
      format!("GitHub draft PR {repo} {head} -> {base}: {title}")
    }
    _ => format!("GitHub {repo}"),
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

/// A branch ref: non-empty, no whitespace, and free of the characters git refuses.
fn valid_branch(branch: &str) -> bool {
  !branch.is_empty()
    && !branch.chars().any(char::is_whitespace)
    && !branch.chars().any(|character| matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

#[cfg(test)]
mod tests {
  use super::*;

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

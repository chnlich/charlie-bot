//! The session directory names of one home, which the checks resolve a session id against.

#[derive(Clone)]
pub(crate) struct SessionIndex {
  pub(crate) names: Vec<String>,
}

impl SessionIndex {
  pub(crate) fn resolve(&self, candidate: &str, full_uuid: bool) -> Option<&str> {
    if full_uuid {
      return self.names.iter().find(|name| name.as_str() == candidate).map(String::as_str);
    }
    if let Some(exact) = self.names.iter().find(|name| name.as_str() == candidate) {
      return Some(exact);
    }
    let mut matches = self.names.iter().filter(|name| name.starts_with(candidate));
    let only = matches.next()?;
    matches.next().is_none().then_some(only)
  }
}

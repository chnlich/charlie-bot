pub(super) fn is_heading(line: &str, heading: &str) -> bool {
  line.strip_prefix("## ").is_some_and(|name| name.trim_end_matches([' ', '\t']) == heading)
}

pub(super) fn is_next_heading(line: &&str) -> bool {
  line.starts_with("## ") && line.len() > 3
}

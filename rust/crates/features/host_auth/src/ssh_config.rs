//! `parse_ssh_config_hosts`: the aliases the panel probes, read from the operator's ssh config.
//!
//! A block is collected only when its `Host` line names one plain alias and the block carries a `HostName`. The
//! first block for an alias wins, as ssh's own first-value rule does.

use std::collections::HashSet;

use anyhow::bail;
use charliebot_core::str_methods;

use crate::text_lines;

/// `(alias, hostname)` for every collectable `Host` block of `text`, in file order.
pub fn parse_hosts(text: &str) -> anyhow::Result<Vec<(String, String)>> {
  let mut hosts = Vec::new();
  let mut seen = HashSet::new();
  let mut block = Block::default();
  for raw_line in text_lines::splitlines(text) {
    let line = str_methods::strip(raw_line.split_once('#').map_or(raw_line, |(before, _)| before));
    if line.is_empty() {
      continue;
    }
    let separated = line.replace('=', " ");
    let tokens = str_methods::split_whitespace(&separated);
    let Some(&keyword) = tokens.first() else {
      bail!("ssh config line {line:?} has no keyword");
    };
    match keyword.to_lowercase().as_str() {
      "host" => {
        block.flush(&mut hosts, &mut seen);
        block = Block::host(&tokens[1..]);
      }
      "hostname" => {
        if let Some(value) = tokens.get(1) {
          block.hostname = Some(unquote(value).to_string());
        }
      }
      "match" => {
        block.flush(&mut hosts, &mut seen);
        block = Block::default();
      }
      _ => {}
    }
  }
  block.flush(&mut hosts, &mut seen);
  Ok(hosts)
}

/// The `Host` block being read: its patterns, its `HostName` so far, and whether its `Host` line is collectable.
#[derive(Default)]
struct Block {
  patterns: Vec<String>,
  hostname: Option<String>,
  collectable: bool,
}

impl Block {
  fn host(tokens: &[&str]) -> Block {
    let patterns: Vec<String> = tokens.iter().map(|token| unquote(token).to_string()).collect();
    let collectable = patterns.len() == 1 && !patterns[0].contains(['*', '?', '!']);
    Block { patterns, hostname: None, collectable }
  }

  /// Collect the block when it is collectable, carries a `HostName`, and its alias is new.
  fn flush(&self, hosts: &mut Vec<(String, String)>, seen: &mut HashSet<String>) {
    if !self.collectable {
      return;
    }
    let (Some(alias), Some(hostname)) = (self.patterns.first(), self.hostname.as_ref()) else {
      return;
    };
    if seen.insert(alias.clone()) {
      hosts.push((alias.clone(), hostname.clone()));
    }
  }
}

/// `_unquote`: one pair of matching quotes around the whole value comes off.
fn unquote(value: &str) -> &str {
  let bytes = value.as_bytes();
  let quoted = bytes.len() >= 2 && (bytes[0] == b'"' || bytes[0] == b'\'') && bytes[0] == bytes[bytes.len() - 1];
  if quoted { &value[1..value.len() - 1] } else { value }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn only_plain_aliases_with_a_hostname_are_collected_first_block_wins() {
    let config = "\
# Synthetic operator config.
Host login-ok
  HostName login-ok.example.internal

Host \"login-held\"
  HostName login-held.example.internal

Host=login-eq
  HostName=login-eq.example.internal  # the separator and an inline comment

Host *.example.internal
  User synthetic-operator

Host login-pair-a login-pair-b
  HostName login-pair.example.internal

Match host login-matched
  HostName login-matched.example.internal

Host login-ok
  HostName login-ok-second.example.internal

Host login-nohost
  User synthetic-operator
";
    let hosts = parse_hosts(config).expect("the config parses");
    assert_eq!(
      hosts,
      [
        ("login-ok".to_string(), "login-ok.example.internal".to_string()),
        ("login-held".to_string(), "login-held.example.internal".to_string()),
        ("login-eq".to_string(), "login-eq.example.internal".to_string()),
      ]
    );
  }

  #[test]
  fn a_line_of_only_separators_has_no_keyword() {
    assert!(parse_hosts("=\n").is_err());
  }
}

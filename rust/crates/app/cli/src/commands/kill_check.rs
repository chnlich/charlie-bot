//! The kill-signal check: refuses each `kill` that the external binary can misparse into a signal to every process of the
//! user. `charliebot kill-check` runs it over the tracked files of the work tree, and the `rust-lint` CI job runs it.
//!
//! procps 4.0.4 keeps only the first digit of a negative pid, so `kill -TERM -190500` reads as `kill(-1)`. The check
//! refuses two forms:
//! - Rust, tests included: a `Command::new` whose program is `kill`. The safe form is `libc::kill` or
//!   `nix::sys::signal::kill`, with a negative pid for a process group.
//! - Shell, including the shell text of Rust string literals and the bodies of heredocs that a shell or `ssh` runs: a
//!   `kill` whose operand is a negative pid before `--`. The safe form is `kill -TERM -- -"$pgid"`.
//!
//! Rust is read as tokens, so a comment and a doc attribute are never a call. Shell is read by the command reader, so a
//! `kill` word that is an argument, a comment or a quoted mention is never a call. The agent kill guard keeps its own
//! rules in `kill_guard.rs`; this check does not extend them.

use std::ffi::OsString;
use std::fmt;
use std::path::Path;

use anyhow::Context;
use charliebot_backend_check::gate;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::git;
use proc_macro2::{Delimiter, Literal, TokenStream, TokenTree};

use super::shell_commands::{Bodies, Invocation, Killer, invocations, killer_named};

/// The shells whose scripts the check reads.
const SHELLS: &[&str] = &["bash", "dash", "ksh", "sh", "zsh"];

/// One refused site: its file, its line, and the message that names the safe form.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
  pub path: String,
  pub line: usize,
  pub message: String,
}

impl fmt::Display for Refusal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}:{}: {}", self.path, self.line, self.message)
  }
}

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  if !args.is_empty() {
    return Err(CliExit::usage("kill-check takes no arguments"));
  }
  let cwd =
    std::env::current_dir().map_err(|error| CliExit::error(format!("cannot read the working directory: {error}")))?;
  let root = gate::work_tree(&cwd)
    .map_err(|error| CliExit::error(format!("{error:#}")))?
    .ok_or_else(|| CliExit::usage("kill-check runs inside a git work tree"))?;
  let refusals = scan_repo(&root).map_err(|error| CliExit::error(format!("{error:#}")))?;
  for refusal in &refusals {
    eprintln!("{refusal}");
  }
  if !refusals.is_empty() {
    std::process::exit(1);
  }
  Ok(())
}

/// The refusals in the tracked files of the work tree at `root`.
pub fn scan_repo(root: &Path) -> anyhow::Result<Vec<Refusal>> {
  let output = git::output(root, &["ls-files", "-z"], &[])
    .with_context(|| format!("cannot list the tracked files of {}", root.display()))?;
  anyhow::ensure!(output.status.success(), "git ls-files failed in {}", root.display());
  let mut refusals = Vec::new();
  for path in output.stdout.split(|&byte| byte == 0).filter(|path| !path.is_empty()) {
    let path = std::str::from_utf8(path).context("a tracked path is not UTF-8")?;
    let bytes = std::fs::read(root.join(path)).with_context(|| format!("cannot read {path}"))?;
    // A file that is not UTF-8 is binary, and holds no Rust or shell text to read.
    let Ok(text) = String::from_utf8(bytes) else {
      continue;
    };
    refusals.extend(scan_text(path, &text)?);
  }
  Ok(refusals)
}

/// The refusals in one file's text. The path picks the reading: a `.rs` file is Rust; a `.sh` or `.bash` file, or a
/// file whose first line is a shell's shebang, is shell. Other text is not read.
pub fn scan_text(path: &str, text: &str) -> anyhow::Result<Vec<Refusal>> {
  let mut refusals = Vec::new();
  if path.ends_with(".rs") {
    scan_rust(path, text, &mut refusals)?;
  } else if path.ends_with(".sh") || path.ends_with(".bash") || is_shell_shebang(text) {
    scan_shell(path, text, &mut refusals)?;
  }
  Ok(refusals)
}

fn is_shell_shebang(text: &str) -> bool {
  let Some(line) = text.lines().next().and_then(|first| first.strip_prefix("#!")) else {
    return false;
  };
  line
    .split_whitespace()
    .map(base_name)
    .find(|word| *word != "env" && !word.starts_with('-'))
    .is_some_and(|interpreter| SHELLS.contains(&interpreter))
}

fn scan_shell(path: &str, text: &str, refusals: &mut Vec<Refusal>) -> anyhow::Result<()> {
  let found = invocations(text, Bodies::Script, &[])
    .map_err(|error| anyhow::anyhow!("{path} does not read as shell: {error}"))?;
  for invocation in found {
    if let Invocation::Call { words, line } = invocation
      && killer_named(&words[0]) == Some(Killer::Kill)
      && let Some(operand) = negative_pid(&words[1..])
    {
      refusals.push(Refusal { path: path.to_string(), line, message: shell_message(operand) });
    }
  }
  Ok(())
}

fn scan_rust(path: &str, text: &str, refusals: &mut Vec<Refusal>) -> anyhow::Result<()> {
  let tokens: TokenStream = text.parse().map_err(|error| anyhow::anyhow!("{path} does not lex as Rust: {error}"))?;
  scan_rust_tokens(path, tokens, refusals);
  Ok(())
}

/// Walks the tokens of a Rust file. Each `Command::new("kill")` is a refusal, and each string literal is read as shell.
/// An attribute such as a doc comment is skipped, and a plain comment has no token, so neither is a call.
fn scan_rust_tokens(path: &str, stream: TokenStream, refusals: &mut Vec<Refusal>) {
  let items: Vec<TokenTree> = stream.into_iter().collect();
  let mut at = 0;
  while at < items.len() {
    match &items[at] {
      TokenTree::Punct(hash) if hash.as_char() == '#' => {
        let bracket = if matches!(items.get(at + 1), Some(TokenTree::Punct(bang)) if bang.as_char() == '!') {
          at + 2
        } else {
          at + 1
        };
        if let Some(TokenTree::Group(group)) = items.get(bracket)
          && group.delimiter() == Delimiter::Bracket
        {
          at = bracket + 1;
          continue;
        }
      }
      TokenTree::Ident(ident) if ident == "Command" && spawns_kill(&items[at..]) => {
        refusals.push(Refusal {
          path: path.to_string(),
          line: items[at].span().start().line,
          message: SPAWN_MESSAGE.to_string(),
        });
      }
      TokenTree::Literal(literal) => check_string_literal(path, literal, refusals),
      TokenTree::Group(group) => scan_rust_tokens(path, group.stream(), refusals),
      _ => {}
    }
    at += 1;
  }
}

/// Whether the tokens at the head of `items` are `Command :: new ( "kill" )`, with the program's path optional.
fn spawns_kill(items: &[TokenTree]) -> bool {
  let is_colon = |token: Option<&TokenTree>| matches!(token, Some(TokenTree::Punct(colon)) if colon.as_char() == ':');
  let is_new = matches!(items.get(3), Some(TokenTree::Ident(ident)) if ident == "new");
  let Some(TokenTree::Group(args)) = items.get(4) else {
    return false;
  };
  args.delimiter() == Delimiter::Parenthesis
    && is_colon(items.get(1))
    && is_colon(items.get(2))
    && is_new
    && matches!(
      args.stream().into_iter().next(),
      Some(TokenTree::Literal(program)) if string_value(&program).is_some_and(|name| base_name(&name) == "kill")
    )
}

/// Reads a string literal as shell when its text names `kill`. Text that the reader cannot read is prose, not a shell
/// command, so it is not a refusal. A literal that spans several source lines counts its lines from its first one.
fn check_string_literal(path: &str, literal: &Literal, refusals: &mut Vec<Refusal>) {
  let Some(value) = string_value(literal) else {
    return;
  };
  if !value.contains("kill") {
    return;
  }
  let Ok(found) = invocations(&value, Bodies::Script, &[]) else {
    return;
  };
  let first_line = literal.span().start().line;
  let spans_lines = literal.to_string().contains('\n');
  for invocation in found {
    if let Invocation::Call { words, line } = invocation
      && killer_named(&words[0]) == Some(Killer::Kill)
      && let Some(operand) = negative_pid(&words[1..])
    {
      let line = if spans_lines { first_line + line - 1 } else { first_line };
      refusals.push(Refusal { path: path.to_string(), line, message: shell_message(operand) });
    }
  }
}

/// The value of a string literal, or `None` for a literal of another kind.
fn string_value(literal: &Literal) -> Option<String> {
  syn::parse_str::<syn::LitStr>(&literal.to_string()).ok().map(|string| string.value())
}

/// The operand of a `kill` that reads as a negative pid: the first word after the signal that starts with `-`, read
/// before any `--`. The signal is the first word that starts with `-`, or the word after `-s`, `-n` or `-q`. Any other
/// word ends the signal position, so a later `-` word is a pid.
fn negative_pid(args: &[String]) -> Option<&str> {
  let mut signal_seen = false;
  let mut operand_pending = false;
  for arg in args {
    if arg == "--" {
      return None;
    }
    if operand_pending {
      operand_pending = false;
      signal_seen = true;
    } else if arg.starts_with('-') && !signal_seen {
      operand_pending = matches!(arg.as_str(), "-s" | "-n" | "-q");
      signal_seen = true;
    } else if arg.starts_with('-') {
      return Some(arg);
    } else {
      signal_seen = true;
    }
  }
  None
}

fn base_name(text: &str) -> &str {
  text.rsplit_once('/').map_or(text, |(_, name)| name)
}

const SPAWN_MESSAGE: &str = "`Command::new(\"kill\")` runs the external kill, which can misparse a negative pid into every \
process of the user; send the signal with `libc::kill` or `nix::sys::signal::kill`, with a negative pid for a process \
group";

fn shell_message(operand: &str) -> String {
  format!(
    "`kill` takes `{operand}` as a negative pid without `--`, and the external kill can misparse it into every process \
of the user; write `kill -TERM -- -\"$pgid\"` (the signal, then `--`, then the negative pid)"
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Writes each `@` as `-`, so that a test source holds no refused form. The refused forms are built at run time.
  fn fixture(template: &str) -> String {
    template.replace('@', "-")
  }

  fn lines(refusals: &[Refusal]) -> Vec<usize> {
    refusals.iter().map(|refusal| refusal.line).collect()
  }

  #[test]
  fn a_rust_spawn_of_kill_is_refused_with_the_safe_form() {
    let text =
      "use std::process::Command;\n\nfn stop(pid: u32) {\n  Command::new(\"kill\")\n    .args([\"-TERM\"]);\n}\n";
    let refusals = scan_text("rust/x.rs", text).unwrap();
    assert_eq!(lines(&refusals), [4]);
    assert!(refusals[0].message.contains("libc::kill"), "{}", refusals[0].message);
  }

  #[test]
  fn a_path_qualified_kill_spawn_is_refused() {
    let text = "fn stop() { std::process::Command::new(\"/usr/bin/kill\").status().ok(); }";
    assert_eq!(lines(&scan_text("rust/x.rs", text).unwrap()), [1]);
  }

  #[test]
  fn a_spawn_of_another_program_and_a_libc_call_are_accepted() {
    let text =
      "fn stop(pid: i32) {\n  Command::new(\"true\").status().ok();\n  unsafe { libc::kill(pid, libc::SIGTERM) };\n}\n";
    assert!(scan_text("rust/x.rs", text).unwrap().is_empty());
  }

  #[test]
  fn a_comment_and_a_doc_attribute_are_not_calls() {
    let text = fixture(
      "// Command::new(\"kill\") is refused, and so is kill -TERM @$pid in a comment.\n\
       /// Prose: kill -TERM @$pid stops the group.\n\
       fn stop() {}\n",
    );
    assert!(scan_text("rust/x.rs", &text).unwrap().is_empty());
  }

  #[test]
  fn a_shell_kill_with_a_negative_pid_is_refused_with_the_safe_form() {
    let text = fixture("#!/bin/sh\nset -eu\nkill -TERM @$pgid\n");
    let refusals = scan_text("scripts/stop.sh", &text).unwrap();
    assert_eq!(lines(&refusals), [3]);
    assert!(refusals[0].message.contains("kill -TERM -- -\"$pgid\""), "{}", refusals[0].message);
  }

  #[test]
  fn a_quoted_negative_pid_and_a_negative_pid_after_dash_s_are_refused() {
    let text = fixture("kill -9 @\"$pid\"\nkill -s TERM @$pid\n");
    assert_eq!(lines(&scan_text("scripts/stop.sh", &text).unwrap()), [1, 2]);
  }

  #[test]
  fn a_kill_in_a_case_arm_a_substitution_and_a_sh_c_argument_is_refused() {
    let text = fixture(
      "case $x in\n  stop) kill -KILL @$p ;;\n  *) true ;;\nesac\n\
       echo $(kill -TERM @$q)\n\
       sh -c 'kill -TERM @$r'\n",
    );
    assert_eq!(lines(&scan_text("scripts/stop.sh", &text).unwrap()), [2, 5, 6]);
  }

  #[test]
  fn a_kill_in_a_heredoc_that_a_shell_or_ssh_runs_is_refused() {
    let text = fixture("bash <<'EOF'\nkill -9 @$p\nEOF\nssh host <<'EOF2'\nkill -TERM @$q\nEOF2\n");
    assert_eq!(lines(&scan_text("scripts/stop.sh", &text).unwrap()), [2, 5]);
  }

  #[test]
  fn a_heredoc_body_that_no_shell_runs_is_text() {
    let text = "read -r -d '' program <<'JQ' || true\nscan(\"'`\")\nJQ\n";
    assert!(scan_text("scripts/skill-reads.sh", text).unwrap().is_empty());
  }

  #[test]
  fn a_shell_kill_in_a_rust_string_is_refused() {
    let text = fixture("fn stop() {\n  run(\"sh -c 'kill -TERM @$pid'\");\n}\n");
    assert_eq!(lines(&scan_text("rust/x.rs", &text).unwrap()), [2]);
  }

  #[test]
  fn the_accepted_kill_forms_pass() {
    let text = "kill -TERM -- -\"$pgid\"\nkill -0 \"$pid\"\nkill -TERM $$\nkill -l\nkill -s KILL -- -$$\n";
    assert!(scan_text("scripts/stop.sh", text).unwrap().is_empty());
  }

  #[test]
  fn a_quoted_mention_or_a_comment_is_not_a_call() {
    let text = fixture(
      "# kill -TERM @$pid stops the group\n\
       echo \"kill -TERM @$pid\"\n\
       printf 'do not run kill -9 @$pid\\n'\n\
       grep kill -TERM @$pid log.txt\n",
    );
    assert!(scan_text("scripts/stop.sh", &text).unwrap().is_empty());
  }

  #[test]
  fn a_rust_string_that_is_prose_is_not_a_call() {
    let text = fixture("fn note() -> &'static str { \"do not run kill -TERM @$pid\" }\n");
    assert!(scan_text("rust/x.rs", &text).unwrap().is_empty());
  }

  #[test]
  fn a_shell_file_that_does_not_read_fails() {
    let error = scan_text("scripts/broken.sh", "echo 'unclosed\n").unwrap_err();
    assert!(error.to_string().contains("does not read as shell"), "{error}");
  }

  #[test]
  fn a_shebang_names_a_shell_file_without_a_shell_extension() {
    let text = fixture("#!/usr/bin/env bash\nkill -TERM @$pid\n");
    assert_eq!(lines(&scan_text("tools/stop", &text).unwrap()), [2]);
  }
}

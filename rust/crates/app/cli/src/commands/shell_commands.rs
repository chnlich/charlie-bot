//! Reads a shell command text into its command positions, for the kill guard, the kill-signal check and the wait guard.
//!
//! The reader follows POSIX word rules closely enough to tell a command word from an argument. Quotes and escapes are
//! removed. A `$(...)` or backtick body is a command of its own. A `case` arm's pattern is no command, so the reader
//! reads through the arm's `)`. A heredoc body is data, except that an unquoted delimiter runs the body's substitutions;
//! `Bodies::Script` also reads the body of a heredoc that a shell or `ssh` runs as a script. A command word that holds a
//! variable or a substitution has no known name, so it reads as `Invocation::Dynamic`. Each command reads with the line
//! its first word is on. A wrapper such as `sudo` or `timeout` runs the command named by one of its arguments, so the
//! caller names the commands it classifies, and the reader descends into a wrapper's argument that names one.

/// How a heredoc body reads: as text, or, when a shell or `ssh` runs it, as a script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bodies {
  Data,
  Script,
}

/// The commands the readers classify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Killer {
  Pkill,
  Pgrep,
  Killall,
  Kill,
}

/// One command position: a command with its words, or a command word whose name the reader cannot know.
#[derive(Debug)]
pub enum Invocation {
  Dynamic,
  /// The command's words from its name on, and the line its first word is on. A wrapper that runs the command is not
  /// among them.
  Call {
    words: Vec<String>,
    line: usize,
  },
}

/// Stands in for a command substitution inside a word. The shell fills in the word's value only when it runs the text.
const SUBSTITUTION: char = '\u{2}';

/// The wrappers that run the command named by one of their arguments.
const WRAPPERS: &[&str] = &[
  "builtin", "command", "env", "exec", "ionice", "nice", "nohup", "setsid", "stdbuf", "sudo", "time", "timeout",
  "watch", "xargs",
];

/// The shells whose `-c` argument is a command text.
const SHELLS: &[&str] = &["bash", "dash", "ksh", "sh", "zsh"];

/// Whether a command runs the text it is given as a script: a shell, or `ssh`, which hands its input to a remote shell.
fn runs_script(words: &[String]) -> bool {
  let runner = |word: &str| SHELLS.contains(&base_name(word)) || base_name(word) == "ssh";
  let Some(start) = command_start(words) else {
    return false;
  };
  let name = &words[start];
  runner(name) || (WRAPPERS.contains(&base_name(name)) && words[start + 1..].iter().any(|word| runner(word)))
}

/// The reserved words that can stand before a command word.
const KEYWORDS: &[&str] = &["!", "{", "}", "do", "done", "elif", "else", "fi", "if", "then", "until", "while"];

/// Every command position in a text: the commands at its top level, inside its substitutions and heredocs, and in the
/// text of a shell's `-c` argument or of `eval`. A wrapper argument that names one of `classified` is the command the
/// wrapper runs, so the position is that command's, and the wrapper itself is not a position. The text's first line is
/// line 1.
pub fn invocations(text: &str, bodies: Bodies, classified: &[&str]) -> Result<Vec<Invocation>, String> {
  invocations_from(text, 1, bodies, classified)
}

/// `invocations` for a text whose first line is line `first_line` of its file, such as a shell's `-c` argument.
pub fn invocations_from(
  text: &str,
  first_line: usize,
  bodies: Bodies,
  classified: &[&str],
) -> Result<Vec<Invocation>, String> {
  let mut reader = Reader::new(text, first_line, bodies);
  reader.read_list(None)?;
  // The old kill-guard reader refused `case` syntax as unparseable. Keep that fail-closed behavior while allowing
  // callers such as kill-check to inspect the commands that the shared reader can now parse inside each arm.
  let mut found = if reader.saw_case { vec![Invocation::Dynamic] } else { Vec::new() };
  for simple in &reader.simples {
    call(&simple.words, simple.line, bodies, classified, &mut found)?;
  }
  Ok(found)
}

/// Classifies one simple command: its command word, and the command that a wrapper runs.
fn call(
  words: &[String],
  line: usize,
  bodies: Bodies,
  classified: &[&str],
  found: &mut Vec<Invocation>,
) -> Result<(), String> {
  let Some(start) = command_start(words) else {
    return Ok(());
  };
  let command = &words[start];
  if is_dynamic(command) {
    found.push(Invocation::Dynamic);
    return Ok(());
  }
  let name = base_name(command);
  let arguments = &words[start + 1..];
  if name == "eval" {
    found.extend(invocations_from(&arguments.join(" "), line, bodies, classified)?);
  } else if SHELLS.contains(&name) {
    if let Some(flag) = arguments.iter().position(|word| is_c_flag(word))
      && let Some(script) = arguments.get(flag + 1)
    {
      found.extend(invocations_from(script, line, bodies, classified)?);
    }
  } else if WRAPPERS.contains(&name) {
    if name == "command" && is_command_query(arguments) {
      return Ok(());
    }
    for (offset, word) in arguments.iter().enumerate() {
      if is_dynamic(word) {
        found.push(Invocation::Dynamic);
        break;
      }
      if is_classified(word, classified) {
        call(&arguments[offset..], line, bodies, classified, found)?;
        break;
      }
    }
  } else {
    found.push(Invocation::Call { words: words[start..].to_vec(), line });
  }
  Ok(())
}

fn is_command_query(arguments: &[String]) -> bool {
  for arg in arguments {
    if arg == "--" || !arg.starts_with('-') || arg == "-" {
      break;
    }
    if arg.chars().skip(1).any(|flag| matches!(flag, 'v' | 'V')) {
      return true;
    }
  }
  false
}

fn command_start(words: &[String]) -> Option<usize> {
  words.iter().position(|word| !is_assignment(word) && !KEYWORDS.contains(&word.as_str()))
}

fn is_assignment(text: &str) -> bool {
  let Some((name, _)) = text.split_once('=') else {
    return false;
  };
  let mut chars = name.chars();
  chars.next().is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
    && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn is_dynamic(text: &str) -> bool {
  text.contains(['$', SUBSTITUTION])
}

/// Whether a wrapper's argument names a command that the wrapper runs: one of the callers' `classified` names, a
/// killer, or a shell or `eval`, whose text is read as commands.
fn is_classified(text: &str, classified: &[&str]) -> bool {
  let name = base_name(text);
  killer_named(name).is_some() || classified.contains(&name) || name == "eval" || SHELLS.contains(&name)
}

fn is_c_flag(text: &str) -> bool {
  text.len() > 1 && text.starts_with('-') && !text.starts_with("--") && text.contains('c')
}

/// The name of a command word without its directory.
pub fn base_name(text: &str) -> &str {
  text.rsplit_once('/').map_or(text, |(_, name)| name)
}

/// The killer that a command word names, if it names one.
pub fn killer_named(text: &str) -> Option<Killer> {
  match base_name(text) {
    "pkill" => Some(Killer::Pkill),
    "pgrep" => Some(Killer::Pgrep),
    "killall" => Some(Killer::Killall),
    "kill" => Some(Killer::Kill),
    _ => None,
  }
}

/// The words of one simple command, and the line its first word is on.
#[derive(Default)]
struct Simple {
  line: usize,
  words: Vec<String>,
}

/// The `case` constructs whose arms the reader is inside, and whether the next word is an arm's pattern.
#[derive(Default)]
struct Arms {
  open: usize,
  pattern: bool,
}

/// A heredoc whose body starts on the line after its opener.
struct Heredoc {
  delimiter: String,
  /// Whether the delimiter is unquoted, so the body's substitutions run.
  expands: bool,
  /// Whether the opener is `<<-`, which strips leading tabs from each body line before the delimiter test.
  strips_tabs: bool,
  /// Whether the command the opener belongs to runs the body as a script.
  runs_script: bool,
}

struct Reader {
  chars: Vec<char>,
  /// The index in `chars` of each newline, ascending.
  newlines: Vec<usize>,
  /// The line of `chars[0]`.
  first_line: usize,
  bodies: Bodies,
  pos: usize,
  heredocs: Vec<Heredoc>,
  /// The simple commands read so far. Substitutions and heredoc bodies add theirs here too.
  simples: Vec<Simple>,
  /// Whether this text contains a `case` construct, which the kill guard still treats as unparseable.
  saw_case: bool,
  /// Set by a redirection operator. The next word is its target, which is not a command word.
  drop_next_word: bool,
  /// The `case` state of the list being read. A nested list keeps its own and restores this one when it ends.
  arms: Arms,
}

impl Reader {
  fn new(text: &str, first_line: usize, bodies: Bodies) -> Self {
    let chars: Vec<char> = text.chars().collect();
    let newlines = chars.iter().enumerate().filter(|(_, character)| **character == '\n').map(|(at, _)| at).collect();
    Reader {
      chars,
      newlines,
      first_line,
      bodies,
      pos: 0,
      heredocs: Vec::new(),
      simples: Vec::new(),
      saw_case: false,
      drop_next_word: false,
      arms: Arms::default(),
    }
  }

  /// The line of the character at index `at`.
  fn line_at(&self, at: usize) -> usize {
    self.first_line + self.newlines.partition_point(|&newline| newline < at)
  }

  fn bump(&mut self) -> Option<char> {
    let character = self.chars.get(self.pos).copied();
    if character.is_some() {
      self.pos += 1;
    }
    character
  }

  fn peek(&self) -> Option<char> {
    self.chars.get(self.pos).copied()
  }

  /// Reads simple commands until `close`, which is consumed, or to the end of the text when `close` is `None`.
  /// `close` is `)` inside `$(...)` and a backtick inside a backtick substitution. A `)` that closes no group ends
  /// the list only when `close` is `)`.
  fn read_list(&mut self, close: Option<char>) -> Result<(), String> {
    let outer = std::mem::take(&mut self.arms);
    let result = self.read_commands(close);
    self.arms = outer;
    result
  }

  fn read_commands(&mut self, close: Option<char>) -> Result<(), String> {
    let mut words = Simple::default();
    let mut word: Option<String> = None;
    let mut groups = 0usize;
    loop {
      let Some(character) = self.bump() else {
        if let Some(expected) = close {
          return Err(format!("the text ends before `{expected}`"));
        }
        self.end(&mut word, &mut words);
        return Ok(());
      };
      match character {
        ' ' | '\t' => self.finish_word(&mut word, &mut words),
        '\n' => {
          if self.arms.pattern {
            self.finish_word(&mut word, &mut words);
          } else {
            self.end(&mut word, &mut words);
            self.read_heredocs()?;
          }
        }
        '&' | '|' => self.separate(&mut word, &mut words),
        ';' => {
          self.end(&mut word, &mut words);
          self.arm_terminator();
        }
        '(' => {
          if self.arms.pattern {
            self.finish_word(&mut word, &mut words);
          } else {
            self.end(&mut word, &mut words);
            groups += 1;
          }
        }
        ')' if self.arms.pattern => {
          self.finish_word(&mut word, &mut words);
          words.words.clear();
          self.arms.pattern = false;
        }
        ')' => {
          self.end(&mut word, &mut words);
          if groups > 0 {
            groups -= 1;
          } else if close == Some(')') {
            return Ok(());
          } else {
            return Err("the text closes a `)` that it never opened".to_string());
          }
        }
        '`' => {
          if close == Some('`') {
            self.end(&mut word, &mut words);
            return Ok(());
          }
          placeholder(&mut word);
          self.read_list(Some('`'))?;
        }
        '$' if self.peek() == Some('(') => {
          placeholder(&mut word);
          self.read_dollar_paren()?;
        }
        '<' | '>' => self.redirect(character, &mut word, &mut words),
        '#' if word.is_none() => self.skip_comment(),
        '\\' => match self.bump() {
          Some('\n') | None => {}
          Some(escaped) => push(&mut word, escaped),
        },
        '\'' => self.read_single_quoted(&mut word)?,
        '"' => self.read_double_quoted(&mut word)?,
        other => push(&mut word, other),
      }
    }
  }

  /// Reads `$(...)` or `$((...))` starting with the `(` that `peek()` saw after `$`.
  ///
  /// Arithmetic `$((...))` cannot open heredocs via `<<`, so it scans balanced parentheses first and falls back to
  /// `read_list` when the inner `(` closes without a trailing `)` (e.g. `$((echo a); (echo b))`).
  fn read_dollar_paren(&mut self) -> Result<(), String> {
    self.bump();
    if self.peek() == Some('(') {
      let saved_pos = self.pos;
      let saved_simples = self.simples.len();
      self.bump();
      if self.read_arithmetic()? {
        return Ok(());
      }
      self.pos = saved_pos;
      self.simples.truncate(saved_simples);
    }
    self.read_list(Some(')'))
  }

  /// Scans an arithmetic expansion body after `$((` up to its matching `))`, parsing any substitutions inside it.
  /// Returns `Ok(false)` if the form is not closed as arithmetic so the caller can backtrack to `$(...)`.
  fn read_arithmetic(&mut self) -> Result<bool, String> {
    let mut depth = 0usize;
    loop {
      let Some(character) = self.bump() else {
        return Ok(false);
      };
      match character {
        '(' => depth += 1,
        ')' => {
          if depth > 0 {
            depth -= 1;
          } else if self.peek() == Some(')') {
            self.bump();
            return Ok(true);
          } else {
            return Ok(false);
          }
        }
        ';' | '\n' if depth == 0 => return Ok(false),
        '$' if self.peek() == Some('(') => self.read_dollar_paren()?,
        '`' => self.read_list(Some('`'))?,
        '\'' if !self.skip_single_quoted() => return Ok(false),
        '"' if !self.read_arith_double_quoted()? => return Ok(false),
        '\\' => {
          self.bump();
        }
        _ => {}
      }
    }
  }

  fn skip_single_quoted(&mut self) -> bool {
    loop {
      match self.bump() {
        None => return false,
        Some('\'') => return true,
        Some(_) => {}
      }
    }
  }

  fn read_arith_double_quoted(&mut self) -> Result<bool, String> {
    loop {
      match self.bump() {
        None => return Ok(false),
        Some('"') => return Ok(true),
        Some('\\') => {
          self.bump();
        }
        Some('$') if self.peek() == Some('(') => self.read_dollar_paren()?,
        Some('`') => self.read_list(Some('`'))?,
        Some(_) => {}
      }
    }
  }

  /// A separator that ends a command. Inside an arm's pattern it only ends the word, so `|` joins alternatives.
  fn separate(&mut self, word: &mut Option<String>, words: &mut Simple) {
    if self.arms.pattern {
      self.finish_word(word, words);
    } else {
      self.end(word, words);
    }
  }

  /// Reads the rest of an arm's terminator after its first `;`: `;;`, `;;&` or `;&`. The next word then starts an
  /// arm's pattern.
  fn arm_terminator(&mut self) {
    match self.peek() {
      Some(';') => {
        self.bump();
        if self.peek() == Some('&') {
          self.bump();
        }
      }
      Some('&') => {
        self.bump();
      }
      _ => return,
    }
    self.arms.pattern = self.arms.open > 0;
  }

  fn end(&mut self, word: &mut Option<String>, words: &mut Simple) {
    self.finish_word(word, words);
    if !words.words.is_empty() {
      self.simples.push(std::mem::take(words));
    }
  }

  fn finish_word(&mut self, word: &mut Option<String>, words: &mut Simple) {
    let Some(finished) = word.take() else {
      return;
    };
    if self.drop_next_word {
      self.drop_next_word = false;
      return;
    }
    if words.words.is_empty() && !self.arms.pattern && finished == "case" {
      self.saw_case = true;
    }
    if words.words.is_empty() && self.arms.open > 0 && finished == "esac" {
      self.arms.open -= 1;
      self.arms.pattern = false;
      return;
    }
    if finished == "in" && words.words.len() == 2 && words.words[0] == "case" {
      words.words.clear();
      self.arms.open += 1;
      self.arms.pattern = true;
      return;
    }
    if words.words.is_empty() {
      words.line = self.line_at(self.pos.saturating_sub(1));
    }
    words.words.push(finished);
  }

  /// A redirection operator and its target. A descriptor number before the operator, such as `2>`, is part of the
  /// operator, so it is not a command word.
  fn redirect(&mut self, first: char, word: &mut Option<String>, words: &mut Simple) {
    if word.as_ref().is_some_and(|text| text.chars().all(|digit| digit.is_ascii_digit())) {
      *word = None;
    }
    self.finish_word(word, words);
    let mut operator = first.to_string();
    while let Some(next) = self.peek().filter(|character| matches!(*character, '<' | '>' | '&' | '|')) {
      operator.push(next);
      self.bump();
    }
    match operator.as_str() {
      "<<" => self.read_heredoc_opener(false, runs_script(&words.words)),
      "<<-" => self.read_heredoc_opener(true, runs_script(&words.words)),
      _ => self.drop_next_word = true,
    }
  }

  fn read_heredoc_opener(&mut self, strips_tabs: bool, runs_script: bool) {
    while matches!(self.peek(), Some(' ' | '\t')) {
      self.bump();
    }
    let mut delimiter = String::new();
    let mut expands = true;
    while let Some(character) = self.peek() {
      if matches!(character, ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')') {
        break;
      }
      self.bump();
      match character {
        '\'' | '"' => expands = false,
        '\\' => {
          expands = false;
          if let Some(escaped) = self.bump() {
            delimiter.push(escaped);
          }
        }
        _ => delimiter.push(character),
      }
    }
    self.heredocs.push(Heredoc { delimiter, expands, strips_tabs, runs_script });
  }

  /// Reads the bodies of the heredocs that the finished line opened. A body runs to its delimiter line, or to the
  /// end of the text when no such line exists.
  fn read_heredocs(&mut self) -> Result<(), String> {
    for heredoc in std::mem::take(&mut self.heredocs) {
      let body_line = self.line_at(self.pos);
      let mut body = String::new();
      while self.pos < self.chars.len() {
        let start = self.pos;
        let end =
          self.chars[start..].iter().position(|&character| character == '\n').map_or(self.chars.len(), |at| start + at);
        let line: String = self.chars[start..end].iter().collect();
        self.pos = end + 1;
        let candidate = if heredoc.strips_tabs { line.trim_start_matches('\t') } else { line.as_str() };
        if candidate == heredoc.delimiter.as_str() {
          break;
        }
        body.push_str(&line);
        body.push('\n');
      }
      if self.bodies == Bodies::Script && heredoc.runs_script {
        let mut script = Reader::new(&body, body_line, Bodies::Script);
        script.read_list(None)?;
        self.saw_case |= script.saw_case;
        self.simples.extend(script.simples);
      } else if heredoc.expands {
        let mut expansions = Reader::new(&body, body_line, self.bodies);
        expansions.read_expansions()?;
        self.saw_case |= expansions.saw_case;
        self.simples.extend(expansions.simples);
      }
    }
    Ok(())
  }

  /// Reads the substitutions of an unquoted heredoc body. Its quotes are text, and each `$(...)` or backtick runs.
  fn read_expansions(&mut self) -> Result<(), String> {
    while let Some(character) = self.bump() {
      match character {
        '\\' => {
          self.bump();
        }
        '$' if self.peek() == Some('(') => self.read_dollar_paren()?,
        '`' => self.read_list(Some('`'))?,
        _ => {}
      }
    }
    Ok(())
  }

  fn skip_comment(&mut self) {
    while self.peek().is_some_and(|character| character != '\n') {
      self.bump();
    }
  }

  fn read_single_quoted(&mut self, word: &mut Option<String>) -> Result<(), String> {
    let word = open(word);
    loop {
      match self.bump() {
        None => return Err("a single quote is never closed".to_string()),
        Some('\'') => return Ok(()),
        Some(character) => word.push(character),
      }
    }
  }

  fn read_double_quoted(&mut self, word: &mut Option<String>) -> Result<(), String> {
    open(word);
    loop {
      match self.bump() {
        None => return Err("a double quote is never closed".to_string()),
        Some('"') => return Ok(()),
        Some('\\') => {
          if let Some(escaped) = self.bump()
            && escaped != '\n'
          {
            push(word, escaped);
          }
        }
        Some('$') if self.peek() == Some('(') => {
          placeholder(word);
          self.read_dollar_paren()?;
        }
        Some('`') => {
          placeholder(word);
          self.read_list(Some('`'))?;
        }
        Some(character) => push(word, character),
      }
    }
  }
}

fn open(word: &mut Option<String>) -> &mut String {
  word.get_or_insert_with(String::new)
}

fn push(word: &mut Option<String>, character: char) {
  open(word).push(character);
}

fn placeholder(word: &mut Option<String>) {
  open(word).push(SUBSTITUTION);
}

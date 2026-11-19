//! Reads a Bash command line into its command positions, for the kill guard.
//!
//! The reader follows POSIX word rules closely enough to tell a command word from an argument. Quotes and escapes are
//! removed. A `$(...)` or backtick body is a command of its own. A heredoc body is data, except that an unquoted
//! delimiter runs the body's substitutions. A command word that holds a variable or a substitution has no known name,
//! so it reads as `Invocation::Dynamic`.

/// The commands the kill guard classifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Killer {
  Pkill,
  Pgrep,
  Killall,
  Kill,
}

/// One shell word after quote removal.
#[derive(Clone, Debug)]
pub struct Word {
  pub text: String,
  /// Whether the word holds a quote or an escape. The shell reads such a word as text, so it is never an option.
  pub quoted: bool,
}

/// One command position: a command the guard classifies, or a command word whose name the reader cannot know.
#[derive(Debug)]
pub enum Invocation {
  Dynamic,
  Call { killer: Killer, args: Vec<Word> },
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

/// The reserved words that can stand before a command word.
const KEYWORDS: &[&str] = &["!", "{", "}", "do", "done", "elif", "else", "fi", "if", "then", "until", "while"];

/// Every command position in a text: the commands at its top level, inside its substitutions and heredocs, and in the
/// text of a shell's `-c` argument or of `eval`.
pub fn invocations(text: &str) -> Result<Vec<Invocation>, String> {
  let mut reader = Reader::new(text);
  reader.read_list(None)?;
  let mut found = Vec::new();
  for words in &reader.simples {
    call(words, &mut found)?;
  }
  Ok(found)
}

/// Classifies one simple command: its command word, and the command that a wrapper runs.
fn call(words: &[Word], found: &mut Vec<Invocation>) -> Result<(), String> {
  let Some(start) = command_start(words) else {
    return Ok(());
  };
  let command = &words[start].text;
  if is_dynamic(command) {
    found.push(Invocation::Dynamic);
    return Ok(());
  }
  let name = base_name(command);
  let arguments = &words[start + 1..];
  if let Some(killer) = killer_named(name) {
    found.push(Invocation::Call { killer, args: arguments.to_vec() });
  } else if name == "eval" {
    let text: Vec<&str> = arguments.iter().map(|word| word.text.as_str()).collect();
    found.extend(invocations(&text.join(" "))?);
  } else if SHELLS.contains(&name) {
    if let Some(flag) = arguments.iter().position(|word| is_c_flag(&word.text))
      && let Some(script) = arguments.get(flag + 1)
    {
      found.extend(invocations(&script.text)?);
    }
  } else if WRAPPERS.contains(&name) {
    for (offset, word) in arguments.iter().enumerate() {
      if is_dynamic(&word.text) {
        found.push(Invocation::Dynamic);
        break;
      }
      if is_command_name(&word.text) {
        call(&arguments[offset..], found)?;
        break;
      }
    }
  }
  Ok(())
}

fn command_start(words: &[Word]) -> Option<usize> {
  words.iter().position(|word| !is_assignment(&word.text) && !KEYWORDS.contains(&word.text.as_str()))
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

fn is_command_name(text: &str) -> bool {
  let name = base_name(text);
  killer_named(name).is_some() || name == "eval" || SHELLS.contains(&name)
}

fn is_c_flag(text: &str) -> bool {
  text.len() > 1 && text.starts_with('-') && !text.starts_with("--") && text.contains('c')
}

fn base_name(text: &str) -> &str {
  text.rsplit_once('/').map_or(text, |(_, name)| name)
}

fn killer_named(name: &str) -> Option<Killer> {
  match name {
    "pkill" => Some(Killer::Pkill),
    "pgrep" => Some(Killer::Pgrep),
    "killall" => Some(Killer::Killall),
    "kill" => Some(Killer::Kill),
    _ => None,
  }
}

/// A heredoc whose body starts on the line after its opener.
struct Heredoc {
  delimiter: String,
  /// Whether the delimiter is unquoted, so the body's substitutions run.
  expands: bool,
  /// Whether the opener is `<<-`, which strips leading tabs from each body line before the delimiter test.
  strips_tabs: bool,
}

struct Reader {
  chars: Vec<char>,
  pos: usize,
  heredocs: Vec<Heredoc>,
  /// The simple commands read so far, each as its words. Substitutions and heredoc bodies add theirs here too.
  simples: Vec<Vec<Word>>,
  /// Set by a redirection operator. The next word is its target, which is not a command word.
  drop_next_word: bool,
}

impl Reader {
  fn new(text: &str) -> Self {
    Reader { chars: text.chars().collect(), pos: 0, heredocs: Vec::new(), simples: Vec::new(), drop_next_word: false }
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
    let mut words: Vec<Word> = Vec::new();
    let mut word: Option<Word> = None;
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
          self.end(&mut word, &mut words);
          self.read_heredocs()?;
        }
        ';' | '&' | '|' => self.end(&mut word, &mut words),
        '(' => {
          self.end(&mut word, &mut words);
          groups += 1;
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
          self.bump();
          placeholder(&mut word);
          self.read_list(Some(')'))?;
        }
        '<' | '>' => self.redirect(character, &mut word, &mut words),
        '#' if word.is_none() => self.skip_comment(),
        '\\' => match self.bump() {
          Some('\n') | None => {}
          Some(escaped) => push(&mut word, escaped, true),
        },
        '\'' => self.read_single_quoted(&mut word)?,
        '"' => self.read_double_quoted(&mut word)?,
        other => push(&mut word, other, false),
      }
    }
  }

  fn end(&mut self, word: &mut Option<Word>, words: &mut Vec<Word>) {
    self.finish_word(word, words);
    if !words.is_empty() {
      self.simples.push(std::mem::take(words));
    }
  }

  fn finish_word(&mut self, word: &mut Option<Word>, words: &mut Vec<Word>) {
    let Some(finished) = word.take() else {
      return;
    };
    if self.drop_next_word {
      self.drop_next_word = false;
    } else {
      words.push(finished);
    }
  }

  /// A redirection operator and its target. A descriptor number before the operator, such as `2>`, is part of the
  /// operator, so it is not a command word.
  fn redirect(&mut self, first: char, word: &mut Option<Word>, words: &mut Vec<Word>) {
    if word.as_ref().is_some_and(|text| !text.quoted && text.text.chars().all(|digit| digit.is_ascii_digit())) {
      *word = None;
    }
    self.finish_word(word, words);
    let mut operator = first.to_string();
    while let Some(next) = self.peek().filter(|character| matches!(*character, '<' | '>' | '&' | '|')) {
      operator.push(next);
      self.bump();
    }
    match operator.as_str() {
      "<<" => self.read_heredoc_opener(false),
      "<<-" => self.read_heredoc_opener(true),
      _ => self.drop_next_word = true,
    }
  }

  fn read_heredoc_opener(&mut self, strips_tabs: bool) {
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
    self.heredocs.push(Heredoc { delimiter, expands, strips_tabs });
  }

  /// Reads the bodies of the heredocs that the finished line opened. A body runs to its delimiter line, or to the
  /// end of the text when no such line exists.
  fn read_heredocs(&mut self) -> Result<(), String> {
    for heredoc in std::mem::take(&mut self.heredocs) {
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
      if heredoc.expands {
        let mut expansions = Reader::new(&body);
        expansions.read_expansions()?;
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
        '$' if self.peek() == Some('(') => {
          self.bump();
          self.read_list(Some(')'))?;
        }
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

  fn read_single_quoted(&mut self, word: &mut Option<Word>) -> Result<(), String> {
    open(word).quoted = true;
    loop {
      match self.bump() {
        None => return Err("a single quote is never closed".to_string()),
        Some('\'') => return Ok(()),
        Some(character) => open(word).text.push(character),
      }
    }
  }

  fn read_double_quoted(&mut self, word: &mut Option<Word>) -> Result<(), String> {
    open(word).quoted = true;
    loop {
      match self.bump() {
        None => return Err("a double quote is never closed".to_string()),
        Some('"') => return Ok(()),
        Some('\\') => {
          if let Some(escaped) = self.bump()
            && escaped != '\n'
          {
            push(word, escaped, true);
          }
        }
        Some('$') if self.peek() == Some('(') => {
          self.bump();
          placeholder(word);
          self.read_list(Some(')'))?;
        }
        Some('`') => {
          placeholder(word);
          self.read_list(Some('`'))?;
        }
        Some(character) => push(word, character, true),
      }
    }
  }
}

fn open(word: &mut Option<Word>) -> &mut Word {
  word.get_or_insert_with(|| Word { text: String::new(), quoted: false })
}

fn push(word: &mut Option<Word>, character: char, quoted: bool) {
  let word = open(word);
  word.text.push(character);
  word.quoted |= quoted;
}

fn placeholder(word: &mut Option<Word>) {
  open(word).text.push(SUBSTITUTION);
}

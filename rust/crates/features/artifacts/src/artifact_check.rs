use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context as AnyhowContext, anyhow, bail};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::regex_classes::compile_regex;
use charliebot_core::str_methods;
use charliebot_core::text_repr;
use charliebot_html::{EventKind, folds_at_open, parse_with, unescape};
use charliebot_user_text_checks::{self as user_text_checks, UserTextChecksConfig};
use fancy_regex::Regex;
use serde::Serialize;
use url::Url;
use uuid::Uuid;

use crate::headless_render;

const STYLE_VERBATIM: &str = "style-verbatim";
const HEAD_TEXT: &str = "head-text";
const SECTIONS_NUMBERED: &str = "sections-numbered";
const SCHEMA_DROPPED: &str = "schema-dropped";
pub(crate) const NON_GOALS_LINE: &str = "non-goals-line";
const FOOT_PRESENT: &str = "foot-present";
const EXPLAIN_TRIAD: &str = "explain-triad";
const EXPLAIN_FIGURES: &str = "explain-figures";
const EXPLAIN_EXAMPLES: &str = "explain-examples";
const FORK_OPEN_SHAPE: &str = "fork-open-shape";
const FORK_OPTIONS: &str = "fork-options";
const FORK_DEMOS: &str = "fork-demos";
const FORK_EXPLAINER: &str = "fork-explainer";
const FACT_ANCHORED: &str = "fact-anchored";
const REQ_CHIPS: &str = "req-chips";
const RENDER_PATH: &str = "render-path";
const GOAL_SECTION: &str = "goal-section";
const PAGE_HEIGHT: &str = "page-height";
const ORDINAL_NAMED: &str = "ordinal-named";
const BYTE_INTEGRITY: &str = "byte-integrity";
const TRACKER_ID_NEARBY: &str = user_text_checks::TRACKER_ID_NEARBY;
const SESSION_LINKED: &str = user_text_checks::SESSION_LINKED;
const LINK_RENDERABLE: &str = user_text_checks::LINK_RENDERABLE;

/// The registered genres in the order the parser states them. `waiting` exists only here.
pub const ARTIFACT_GENRES: [&str; 6] = ["plan", "understanding", "sitrep", "debug", "explain", "waiting"];

/// The shape and text elements that `explain-figures` counts in an svg figure. A `div.flow` box strip is not a figure.
const FIGURE_ELEMENT_TAGS: &[&str] = &["rect", "circle", "ellipse", "line", "polyline", "polygon", "path", "text"];
/// The fewest shape or text elements one svg figure holds.
const FIGURE_MIN_ELEMENTS: usize = 3;
const PAGE_HEIGHT_TARGET: i32 = 2000;
const PAGE_PROBE_WIDTH_PX: usize = 1280;
const CODE_START: char = '\u{0001}';
const CODE_END: char = '\u{0002}';

const PAGE_PROBE_TEMPLATE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8">
<style>html,body{margin:0;padding:0}iframe{width:{width}px;border:0;display:block}</style>
</head><body>
<iframe id="artifact" src="{src}"></iframe>
<script>
  const frame = document.getElementById('artifact');
  frame.addEventListener('load', () => {
    const doc = frame.contentDocument;
    for (const mark of doc.querySelectorAll('.revbadge,.revnote')) mark.style.display = 'none';
    const marker = document.createElement('pre');
    marker.id = 'page-height';
    marker.textContent = String(doc.documentElement.scrollHeight);
    document.body.appendChild(marker);
  });
</script>
</body></html>
"##;

const ORDINAL_PATTERN: &str = r"(?<![A-Za-z0-9_.])(?:(?:Trade-offs?|sections?|Section|fork|Fork|round|Round|divergence|Divergence|question|Question|item|Item|plan|Plan)(?:\s+\(|\s*)(\d+)\)?(?![\p{L}\p{N}_])|第\s*([0-9一二三四五六七八九十]+)\s*(?:节|章|题|轮|条|项|问)|分歧\s*([0-9一二三四五六七八九十]+)|(?:(?<=§)|(?<=section )|(?<=Section ))\s*([1-9])\.(\d)(?![\d.%])|(?<![\d.])([1-9])\.(\d)(?=\s*(?:节|Schema|Design Details|reading note)))";
const INTERNAL_UNLESS_QUALIFIED_PATTERN: &str = r"round|Round|question|Question|item|Item|轮|条|项|题|问";
const QUAL_PATTERN: &str = r"(计划|plan|Plan|sitrep|战况|理解页|understanding|任务书|debug|explain|讲解页|上一?份|上一?版|前一?份|那份|另一份|earlier|previous)(?:\s*的|\s*'s|\s*里|\s*中)?\s*$";
const ANCHOR_PATTERN: &str = r"(https?://\S+|(?<![\p{L}\p{N}_/])(?:~|\.{1,2})?/[\p{L}\p{N}_.~-]+(?:/[\p{L}\p{N}_.~-]+)*|\b[\p{L}\p{N}_.~-]+(?:/[\p{L}\p{N}_.~-]+){2,}|\b[\p{L}\p{N}_-]+\.(?:html|md|py|json|yaml|txt)\b|\b[A-Z]{2,}-\d+\b)";
const SENTENCE_SPLIT_PATTERN: &str = r"(?<=[。!?！？])";
const QUOTE_PATTERN: &str = r#"[「“][^」”]{1,80}[」”]|"[^"]{1,80}""#;
const GLOSS_PATTERN: &str = r"^\s*(?:v\d+\s*)?(?:[（(][^（）()]{2,}[）)]|·\s*\S{2,}|v\d+\s+[\p{L}\p{N}_-]{6,})";
const COLON_GLOSS_PATTERN: &str = r"^\s*(?:v\d+\s*)?[:：]\s*\S{2,}";
const TITLE_ANCHOR_PATTERN: &str = r"(?:标题|题为|题目|titled?|named|called)\s*[:：]?\s*[「“][^」”]{2,}[」”]";

const ORDINAL_BLOCK_TAGS: &[&str] =
  &["p", "li", "td", "th", "h1", "h2", "h3", "h4", "summary", "blockquote", "dt", "dd", "figcaption"];
const ORDINAL_SKIP_TAGS: &[&str] = &["style", "script", "pre"];
const EXPLAINER_BODY_TAGS: &[&str] = &["li", "p", "table", "pre"];

/// A fact label's block, which must also carry a `span.src`.
const FACT_ANCESTOR_TAGS: &[&str] = &["p", "li", "td", "blockquote", "h3"];
/// The blocks that can hold a requirement chip's gloss.
const REQ_BLOCK_TAGS: &[&str] = &["p", "li", "td", "th", "dt", "dd", "blockquote", "h1", "h2", "h3", "h4"];
/// The sentinels that fence a requirement chip inside its block's text.
const REQ_START: char = '\u{0003}';
const REQ_END: char = '\u{0004}';
/// The terminators that end the sentence holding a requirement chip.
const SENTENCE_END: [char; 5] = ['.', '。', '!', '?', '！'];
/// The math render path's resource names.
const KATEX_CSS_HINT: &str = "katex.min.css";
const KATEX_JS_HINT: &str = "katex.min.js";
const KATEX_AUTO_RENDER_HINT: &str = "auto-render.min.js";

/// `artifact_shared.GENRE_TEMPLATES`: the genre template whose `<style>` block a genre's pages carry.
pub(crate) fn genre_template(genre: &str) -> &'static str {
  match genre {
    "plan" | "understanding" | "waiting" => "plan_template.html",
    "sitrep" => "sitrep_template.html",
    "debug" => "debug_template.html",
    "explain" => "explain_template.html",
    other => unreachable!("genre {other:?} is registered, so it has a template"),
  }
}

/// A genre's numbered-h2 rule. `Run` requires the numbers to run 1..=n in order, with n in the range. `Subset` requires
/// the numbers to ascend within 1..=max, so a page carries any subset of its numbered sections, or none.
enum SectionRule {
  Run(RangeInclusive<usize>),
  Subset(usize),
}

fn section_count_rule(genre: &str) -> SectionRule {
  match genre {
    "plan" => SectionRule::Run(5..=6),
    "understanding" => SectionRule::Run(5..=usize::MAX),
    "sitrep" | "debug" => SectionRule::Run(5..=5),
    "explain" => SectionRule::Subset(5),
    "waiting" => unreachable!("the waiting set omits sections-numbered, so it has no section-count rule"),
    other => unreachable!("genre {other:?} is registered, so it has a section-count rule"),
  }
}

/// The words for a section count range in a failure, such as `at least 5` or `5 to 6`.
fn describe_count(count: &RangeInclusive<usize>) -> String {
  let (start, end) = (*count.start(), *count.end());
  if end == usize::MAX {
    format!("at least {start}")
  } else if start == end {
    format!("exactly {start}")
  } else {
    format!("{start} to {end}")
  }
}

/// The assertions of one genre, in the order their outcomes print.
fn assertion_set(genre: &str) -> Option<&'static [&'static str]> {
  match genre {
    "plan" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      HEAD_TEXT,
      SECTIONS_NUMBERED,
      SCHEMA_DROPPED,
      NON_GOALS_LINE,
      FOOT_PRESENT,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FORK_DEMOS,
      FORK_EXPLAINER,
      GOAL_SECTION,
      PAGE_HEIGHT,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    "understanding" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      HEAD_TEXT,
      SECTIONS_NUMBERED,
      FOOT_PRESENT,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FORK_DEMOS,
      FORK_EXPLAINER,
      PAGE_HEIGHT,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    "sitrep" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      HEAD_TEXT,
      SECTIONS_NUMBERED,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FORK_DEMOS,
      FORK_EXPLAINER,
      FACT_ANCHORED,
      REQ_CHIPS,
      PAGE_HEIGHT,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    "debug" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      HEAD_TEXT,
      SECTIONS_NUMBERED,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FORK_DEMOS,
      FACT_ANCHORED,
      PAGE_HEIGHT,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    "explain" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      HEAD_TEXT,
      RENDER_PATH,
      SECTIONS_NUMBERED,
      EXPLAIN_TRIAD,
      EXPLAIN_FIGURES,
      EXPLAIN_EXAMPLES,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FORK_DEMOS,
      PAGE_HEIGHT,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    "waiting" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      HEAD_TEXT,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FORK_DEMOS,
      FORK_EXPLAINER,
      PAGE_HEIGHT,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    _ => None,
  }
}

/// The genre whose `<genre>_` prefix starts `file_name`, such as `plan_` in `plan_export_interval_v2.html`.
pub fn genre_from_file_name(file_name: &str) -> Option<&'static str> {
  ARTIFACT_GENRES.into_iter().find(|genre| file_name.starts_with(&format!("{genre}_")))
}

/// The fork assertions a genre's set holds, in set order: the web status set gate runs exactly these.
pub fn fork_assertions(genre: &str) -> Vec<&'static str> {
  const FORK: [&str; 4] = [FORK_OPEN_SHAPE, FORK_OPTIONS, FORK_DEMOS, FORK_EXPLAINER];
  assertion_set(genre)
    .map(|names| names.iter().copied().filter(|name| FORK.contains(name)).collect())
    .unwrap_or_default()
}

/// The fork assertion outcomes of the genre that `page`'s file name names, or `None` when the name carries no genre
/// prefix. The web status set gate reaches this through `Registry::fork_assertions_runner`.
pub fn run_fork_assertions(page: &Path, home: &Path) -> anyhow::Result<Option<Vec<AssertionOutcome>>> {
  let Some(genre) = page.file_name().and_then(|name| name.to_str()).and_then(genre_from_file_name) else {
    return Ok(None);
  };
  let names = fork_assertions(genre);
  // The fork assertions read neither the repository nor a Chrome binary, so both paths stay empty.
  let outcomes = run_assertions_sync_with_user_text(
    genre,
    page,
    "",
    Path::new(""),
    &UserTextChecksConfig::default(),
    home,
    Some(names.iter().map(|name| (*name).to_owned()).collect()),
  )?;
  Ok(Some(outcomes))
}

static GOAL_SECTION_RE: LazyLock<Regex> = LazyLock::new(|| compile_regex(r"(?s)Problem / Goal</h2>.*?</section>"));
static ORD: LazyLock<Regex> = LazyLock::new(|| compile_regex(ORDINAL_PATTERN));
static INTERNAL_UNLESS_QUALIFIED: LazyLock<Regex> = LazyLock::new(|| compile_regex(INTERNAL_UNLESS_QUALIFIED_PATTERN));
static QUAL: LazyLock<Regex> = LazyLock::new(|| compile_regex(QUAL_PATTERN));
static ANCHOR: LazyLock<Regex> = LazyLock::new(|| compile_regex(ANCHOR_PATTERN));
static SENTENCE_SPLIT: LazyLock<Regex> = LazyLock::new(|| compile_regex(SENTENCE_SPLIT_PATTERN));
static QUOTE: LazyLock<Regex> = LazyLock::new(|| compile_regex(QUOTE_PATTERN));
static GLOSS: LazyLock<Regex> = LazyLock::new(|| compile_regex(GLOSS_PATTERN));
static COLON_GLOSS: LazyLock<Regex> = LazyLock::new(|| compile_regex(COLON_GLOSS_PATTERN));
static TITLE_ANCHOR: LazyLock<Regex> = LazyLock::new(|| compile_regex(TITLE_ANCHOR_PATTERN));

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssertionOutcome {
  pub name: String,
  pub passed: bool,
  pub detail: String,
}

#[derive(Clone, Debug)]
struct Element {
  tag: String,
  classes: HashSet<String>,
  /// Every attribute's raw value. A bare attribute is `None`.
  attrs: HashMap<String, Option<String>>,
  parent: Option<usize>,
  children: Vec<Child>,
}

#[derive(Clone, Debug)]
enum Child {
  Element(usize),
  Text(String),
}

struct Context<'a> {
  genre: &'a str,
  artifact: &'a Path,
  chrome_bin: &'a str,
  repo_root: &'a Path,
  nodes: Vec<Element>,
}

#[derive(Clone)]
struct CodeText {
  text: String,
  code_ranges: Vec<(usize, usize)>,
}

type AssertionCheck = for<'a> fn(&Context<'a>) -> anyhow::Result<Vec<AssertionOutcome>>;
type OrdinalFailure = (usize, Vec<String>, String);

/// One genre's assertion set, run on a blocking thread. `only` narrows the run to the named assertions. `None` runs
/// the whole set.
pub async fn run_assertions(
  genre: &str,
  artifact: PathBuf,
  config: &CharlieBotConfig,
  repo_root: PathBuf,
  only: Option<Vec<String>>,
) -> anyhow::Result<Vec<AssertionOutcome>> {
  run_assertions_with_chrome(
    genre,
    artifact,
    config.headless_chrome_bin.clone(),
    repo_root,
    config.section::<UserTextChecksConfig>(user_text_checks::CONFIG_SECTION).clone(),
    config.charliebot_home.clone(),
    only,
  )
  .await
}

/// One genre's assertion set, run with an explicit Chrome path on a blocking thread.
pub async fn run_assertions_with_chrome(
  genre: &str,
  artifact: PathBuf,
  chrome_bin: String,
  repo_root: PathBuf,
  checks_config: UserTextChecksConfig,
  home: PathBuf,
  only: Option<Vec<String>>,
) -> anyhow::Result<Vec<AssertionOutcome>> {
  let genre = genre.to_owned();
  tokio::task::spawn_blocking(move || {
    run_assertions_sync_with_user_text(&genre, &artifact, &chrome_bin, &repo_root, &checks_config, &home, only)
  })
  .await
  .context("plan assertion worker failed")?
}

fn run_assertions_sync_with_user_text(
  genre: &str,
  artifact: &Path,
  chrome_bin: &str,
  repo_root: &Path,
  checks_config: &UserTextChecksConfig,
  home: &Path,
  only: Option<Vec<String>>,
) -> anyhow::Result<Vec<AssertionOutcome>> {
  let names = assertion_set(genre).ok_or_else(|| {
    anyhow!(
      "no assertion set registered for genre {}; genres with a set: plan, understanding, sitrep, debug, explain, \
       waiting",
      text_repr::str_repr(genre)
    )
  })?;
  let source =
    fs::read_to_string(artifact).with_context(|| format!("could not read artifact {}", artifact.display()))?;
  // The user-text checks parse the page and read the session directories. A run narrowed to other assertions skips
  // both, so a missing or unreadable sessions directory cannot turn the assertion's verdict into an error.
  let wants_user_text =
    only.as_ref().is_none_or(|only| only.iter().any(|name| user_text_checks::CHECK_NAMES.contains(&name.as_str())));
  let text_violations = if wants_user_text {
    let text_checks = user_text_checks::UserTextChecks::new(checks_config, home)?;
    text_checks.check_page(&source)?
  } else {
    Vec::new()
  };
  let nodes = parse_dom(&source)?;
  let context = Context { genre, artifact, chrome_bin, repo_root, nodes };
  let mut outcomes = Vec::new();
  for name in names {
    if only.as_ref().is_some_and(|only| !only.iter().any(|wanted| wanted == name)) {
      continue;
    }
    if user_text_checks::CHECK_NAMES.contains(name) {
      let hits = text_violations.iter().filter(|violation| violation.check == *name).collect::<Vec<_>>();
      if hits.is_empty() {
        outcomes.push(outcome(name, true, "no violations"));
      } else {
        outcomes.extend(
          hits.into_iter().map(|violation| outcome(name, false, user_text_checks::page_failure_detail(violation))),
        );
      }
      continue;
    }
    let check: AssertionCheck = match *name {
      BYTE_INTEGRITY => check_byte_integrity,
      STYLE_VERBATIM => check_style_verbatim,
      HEAD_TEXT => check_head_text,
      SECTIONS_NUMBERED => check_sections_numbered,
      SCHEMA_DROPPED => check_schema_dropped,
      NON_GOALS_LINE => check_non_goals_line,
      FOOT_PRESENT => check_foot_present,
      EXPLAIN_TRIAD => check_explain_triad,
      EXPLAIN_FIGURES => check_explain_figures,
      EXPLAIN_EXAMPLES => check_explain_examples,
      FORK_OPEN_SHAPE => check_fork_open_shape,
      FORK_OPTIONS => check_fork_options,
      FORK_DEMOS => check_fork_demos,
      FORK_EXPLAINER => check_fork_explainer,
      FACT_ANCHORED => check_fact_anchored,
      REQ_CHIPS => check_req_chips,
      RENDER_PATH => check_render_path,
      GOAL_SECTION => check_goal_section,
      PAGE_HEIGHT => check_page_height,
      ORDINAL_NAMED => check_ordinal_named,
      other => unreachable!("{other} is an assertion the sets do not name"),
    };
    outcomes.extend(check(&context)?);
  }
  Ok(outcomes)
}

fn parse_dom(source: &str) -> anyhow::Result<Vec<Element>> {
  let mut nodes = vec![Element {
    tag: "#root".to_owned(),
    classes: HashSet::new(),
    attrs: HashMap::new(),
    parent: None,
    children: Vec::new(),
  }];
  let mut stack = vec![0];
  parse_with(source, true, |mut event| match event.kind {
    EventKind::StartTag | EventKind::StartEndTag => {
      let arguments = event.args.as_array().expect("a start-tag event has arguments");
      let tag = arguments[0].as_str().expect("a start-tag event has a tag").to_owned();
      let attributes = arguments[1]
        .as_array()
        .expect("a start-tag event has attributes")
        .iter()
        .map(|attribute| {
          let pair = attribute.as_array().expect("an attribute has a pair");
          let name = pair[0].as_str().expect("an attribute name is text").to_owned();
          let value = pair[1].as_str().map(str::to_owned);
          (name, value)
        })
        .collect::<HashMap<_, _>>();
      let class_value = attributes.get("class").and_then(Option::as_deref).unwrap_or("");
      let classes = str_methods::split_whitespace(class_value).into_iter().map(str::to_owned).collect();
      let parent = *stack.last().expect("the root remains on the parser stack");
      let index = nodes.len();
      nodes.push(Element { tag: tag.clone(), classes, attrs: attributes, parent: Some(parent), children: Vec::new() });
      nodes[parent].children.push(Child::Element(index));
      if event.kind == EventKind::StartTag && !crate::plan_diff::VOID_TAGS.contains(&tag.as_str()) {
        stack.push(index);
      }
    }
    EventKind::EndTag => {
      let tag = event.args[0].as_str().expect("an end-tag event has a tag");
      if let Some(position) = (1..stack.len()).rev().find(|&position| nodes[stack[position]].tag == tag) {
        stack.truncate(position);
      }
    }
    EventKind::Data => {
      let text = match std::mem::take(&mut event.args[0]) {
        serde_json::Value::String(text) => text,
        other => unreachable!("a data event has text, got {other:?}"),
      };
      let parent = *stack.last().expect("the root remains on the parser stack");
      nodes[parent].children.push(Child::Text(text));
    }
    EventKind::EntityReference
    | EventKind::CharacterReference
    | EventKind::Comment
    | EventKind::Declaration
    | EventKind::ProcessingInstruction
    | EventKind::UnknownDeclaration => {}
  })
  .map_err(|error| anyhow!("{}: {}", error.exception, error.message))?;
  Ok(nodes)
}

fn descendants(nodes: &[Element], element: usize) -> Vec<usize> {
  fn walk(nodes: &[Element], element: usize, result: &mut Vec<usize>) {
    for child in &nodes[element].children {
      if let Child::Element(child) = child {
        result.push(*child);
        walk(nodes, *child, result);
      }
    }
  }
  let mut result = Vec::new();
  walk(nodes, element, &mut result);
  result
}

fn find(nodes: &[Element], element: usize, tag: &str, classes: &[&str]) -> Vec<usize> {
  descendants(nodes, element)
    .into_iter()
    .filter(|&candidate| {
      nodes[candidate].tag == tag && classes.iter().all(|class| nodes[candidate].classes.contains(*class))
    })
    .collect()
}

fn element_text(nodes: &[Element], element: usize) -> String {
  fn append(nodes: &[Element], element: usize, output: &mut String) {
    for child in &nodes[element].children {
      match child {
        Child::Element(child) => append(nodes, *child, output),
        Child::Text(text) => output.push_str(text),
      }
    }
  }
  let mut text = String::new();
  append(nodes, element, &mut text);
  text
}

fn section_heading(nodes: &[Element], element: usize) -> String {
  let mut ancestor = nodes[element].parent;
  while let Some(index) = ancestor {
    if nodes[index].tag == "section" {
      return named_section(nodes, index);
    }
    ancestor = nodes[index].parent;
  }
  "<no section>".to_owned()
}

/// The collapsed text of a section's first h2, or `<unnamed section>` when the section has none.
fn named_section(nodes: &[Element], section: usize) -> String {
  find(nodes, section, "h2", &[])
    .first()
    .map(|&heading| str_methods::collapse_whitespace(&element_text(nodes, heading)))
    .unwrap_or_else(|| "<unnamed section>".to_owned())
}

/// The section whose first h2's collapsed text ends with `title`, or `None` when no section carries one. The heading
/// text leads with its span.n number, so the match runs on the title's tail.
fn numbered_section(nodes: &[Element], title: &str) -> Option<usize> {
  find(nodes, 0, "section", &[]).into_iter().find(|&section| named_section(nodes, section).ends_with(title))
}

fn outcome(name: &str, passed: bool, detail: impl Into<String>) -> AssertionOutcome {
  AssertionOutcome { name: name.to_owned(), passed, detail: detail.into() }
}

fn check_byte_integrity(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let bytes = fs::read(context.artifact)
    .with_context(|| format!("could not read artifact bytes {}", context.artifact.display()))?;
  let failures = bytes
    .iter()
    .enumerate()
    .filter(|(_, byte)| **byte < 0x20 && **byte != b'\n')
    .map(|(offset, byte)| format!("0x{byte:02x} at offset {offset}"))
    .collect::<Vec<_>>();
  if failures.is_empty() {
    Ok(vec![outcome(BYTE_INTEGRITY, true, "")])
  } else {
    Ok(vec![outcome(
      BYTE_INTEGRITY,
      false,
      format!("{} non-LF control bytes: {}", failures.len(), failures.join(", ")),
    )])
  }
}

fn check_style_verbatim(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let page_styles = find(&context.nodes, 0, "style", &[]);
  if page_styles.len() != 1 {
    return Ok(vec![outcome(
      STYLE_VERBATIM,
      false,
      format!("page carries {} <style> blocks, expected exactly one", page_styles.len()),
    )]);
  }
  let template_rel = format!("prompts/{}", genre_template(context.genre));
  let template_path = context.repo_root.join(&template_rel);
  let template = fs::read_to_string(&template_path).with_context(|| format!("could not read {template_rel}"))?;
  let template_nodes = parse_dom(&template)?;
  let template_styles = find(&template_nodes, 0, "style", &[]);
  if template_styles.len() != 1 {
    bail!("genre template {template_rel} carries {} <style> blocks, expected one", template_styles.len());
  }
  let page_text = str_methods::collapse_whitespace(&element_text(&context.nodes, page_styles[0]));
  let template_text = str_methods::collapse_whitespace(&element_text(&template_nodes, template_styles[0]));
  if page_text == template_text {
    Ok(vec![outcome(STYLE_VERBATIM, true, "")])
  } else {
    Ok(vec![outcome(STYLE_VERBATIM, false, format!("<style> block differs from the genre template {template_rel}"))])
  }
}

/// `head-text`: non-whitespace text directly under the document root, `html` or `head` renders as page text, because
/// the browser moves it into `body`. CSS left after a head's `</style>` is such text.
fn check_head_text(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut failures = Vec::new();
  for (index, node) in context.nodes.iter().enumerate() {
    let container = match (index, node.tag.as_str()) {
      (0, _) => "the document root",
      (_, "html") => "<html>",
      (_, "head") => "<head>",
      _ => continue,
    };
    for child in &node.children {
      let Child::Text(text) = child else { continue };
      let collapsed = str_methods::collapse_whitespace(text);
      if collapsed.is_empty() {
        continue;
      }
      let snippet = collapsed.chars().take(60).collect::<String>();
      failures.push(outcome(
        HEAD_TEXT,
        false,
        format!(
          "text {} sits directly in {container}, so the browser shows it as page text; move CSS into the page's \
           <style> element and other head markup into its own element",
          text_repr::str_repr(&snippet)
        ),
      ));
    }
  }
  if failures.is_empty() {
    failures.push(outcome(HEAD_TEXT, true, ""));
  }
  Ok(failures)
}

fn check_sections_numbered(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut numbered = Vec::new();
  for heading in find(&context.nodes, 0, "h2", &[]) {
    let numbers = find(&context.nodes, heading, "span", &["n"]);
    if numbers.len() > 1 {
      let heading_text = str_methods::collapse_whitespace(&element_text(&context.nodes, heading));
      return Ok(vec![outcome(
        SECTIONS_NUMBERED,
        false,
        format!("h2 {} carries {} span.n numbers", text_repr::str_repr(&heading_text), numbers.len()),
      )]);
    }
    if let Some(&number) = numbers.first() {
      numbered.push((
        str_methods::collapse_whitespace(&element_text(&context.nodes, heading)),
        str_methods::collapse_whitespace(&element_text(&context.nodes, number)),
      ));
    }
  }
  let mut values = Vec::new();
  for (heading, number) in numbered {
    if !str_methods::isdigit(&number) {
      return Ok(vec![outcome(
        SECTIONS_NUMBERED,
        false,
        format!("h2 {} span.n reads {}, not an integer", text_repr::str_repr(&heading), text_repr::str_repr(&number)),
      )]);
    }
    values.push(decimal_integer(&number).map_err(|error| anyhow!("{error}"))?);
  }
  let read = if values.is_empty() { "(none)".to_owned() } else { values.join(", ") };
  match section_count_rule(context.genre) {
    SectionRule::Run(count) => {
      let expected = (1..=values.len()).map(|value| value.to_string()).collect::<Vec<_>>();
      if values != expected {
        return Ok(vec![outcome(
          SECTIONS_NUMBERED,
          false,
          format!("section numbers read {read}, expected 1..{} contiguously", values.len()),
        )]);
      }
      if !count.contains(&values.len()) {
        return Ok(vec![outcome(
          SECTIONS_NUMBERED,
          false,
          format!("{} requires {} numbered sections, found {}", context.genre, describe_count(&count), values.len()),
        )]);
      }
    }
    SectionRule::Subset(max) => {
      let numbers = values.iter().map(|value| value.parse::<usize>().ok()).collect::<Vec<_>>();
      let ascending_subset = numbers.iter().all(|number| number.is_some_and(|number| (1..=max).contains(&number)))
        && numbers.windows(2).all(|pair| pair[0] < pair[1]);
      if !ascending_subset {
        return Ok(vec![outcome(
          SECTIONS_NUMBERED,
          false,
          format!("section numbers read {read}, expected an ascending subset of 1..={max}"),
        )]);
      }
    }
  }
  Ok(vec![outcome(SECTIONS_NUMBERED, true, "")])
}

fn check_schema_dropped(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let headings = find(&context.nodes, 0, "h3", &[])
    .into_iter()
    .filter(|&heading| {
      find(&context.nodes, heading, "span", &["sn"])
        .into_iter()
        .any(|number| str_methods::collapse_whitespace(&element_text(&context.nodes, number)) == "4.1")
    })
    .collect::<Vec<_>>();
  let heading = match headings.as_slice() {
    [] => return Ok(vec![outcome(SCHEMA_DROPPED, false, "missing 4.1 Schema heading")]),
    [heading] => *heading,
    _ => return Ok(vec![outcome(SCHEMA_DROPPED, false, "more than one 4.1 Schema heading")]),
  };

  let mut ancestor = context.nodes[heading].parent;
  let section = loop {
    let Some(index) = ancestor else {
      break None;
    };
    if context.nodes[index].tag == "section" {
      break Some(index);
    }
    ancestor = context.nodes[index].parent;
  };
  let Some(section) = section else {
    return Ok(vec![outcome(SCHEMA_DROPPED, false, "missing containing section for 4.1 Schema heading")]);
  };

  let section_nodes = descendants(&context.nodes, section);
  let heading_position = section_nodes
    .iter()
    .position(|&element| element == heading)
    .expect("the 4.1 Schema heading is inside its containing section");
  let schema_nodes =
    section_nodes.iter().skip(heading_position + 1).take_while(|&&element| context.nodes[element].tag != "h3").copied();
  let dropped = schema_nodes
    .filter(|&element| context.nodes[element].tag == "p" && context.nodes[element].classes.contains("dropped"))
    .collect::<Vec<_>>();

  match dropped.as_slice() {
    [] => Ok(vec![outcome(SCHEMA_DROPPED, false, "missing p.dropped in 4.1 Schema")]),
    [dropped] => {
      let mut ancestor = context.nodes[*dropped].parent;
      while let Some(index) = ancestor {
        if context.nodes[index].tag == "details" {
          return Ok(vec![outcome(SCHEMA_DROPPED, false, "p.dropped is inside a fold")]);
        }
        ancestor = context.nodes[index].parent;
      }
      if element_text(&context.nodes, *dropped).trim().is_empty() {
        Ok(vec![outcome(SCHEMA_DROPPED, false, "p.dropped is empty")])
      } else {
        Ok(vec![outcome(SCHEMA_DROPPED, true, "")])
      }
    }
    _ => Ok(vec![outcome(
      SCHEMA_DROPPED,
      false,
      format!("more than one p.dropped in 4.1 Schema (found {})", dropped.len()),
    )]),
  }
}

/// The known prefixes of an exclusion paragraph: a paragraph outside `p.non-goals` that starts with one states a
/// non-goal where the one list does not live.
const NON_GOALS_PREFIXES: [&str; 5] = ["不做", "非目标", "范围外", "Non-goals", "Out of scope"];

/// `non-goals-line`: the plan's non-goals live on one closing `p.non-goals` paragraph of section 3, outside every
/// fold, non-empty, linking the understanding page the header's meta chips name when there is one, and no paragraph
/// outside it starts with a known exclusion prefix.
fn check_non_goals_line(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let Some(solution) = numbered_section(&context.nodes, "High Level Solution") else {
    return Ok(vec![outcome(NON_GOALS_LINE, false, "missing High Level Solution section")]);
  };
  let non_goals = find(&context.nodes, solution, "p", &["non-goals"]);
  let paragraph = match non_goals.as_slice() {
    [] => {
      return Ok(vec![outcome(NON_GOALS_LINE, false, "missing p.non-goals in High Level Solution")]);
    }
    [paragraph] => *paragraph,
    _ => {
      return Ok(vec![outcome(
        NON_GOALS_LINE,
        false,
        format!("more than one p.non-goals in High Level Solution (found {})", non_goals.len()),
      )]);
    }
  };
  let mut failures = Vec::new();
  let mut ancestor = context.nodes[paragraph].parent;
  while let Some(index) = ancestor {
    if context.nodes[index].tag == "details" {
      failures.push(outcome(NON_GOALS_LINE, false, "p.non-goals is inside a fold"));
      break;
    }
    ancestor = context.nodes[index].parent;
  }
  let last_element = context.nodes[solution]
    .children
    .iter()
    .filter_map(|child| match child {
      Child::Element(index) => Some(*index),
      Child::Text(_) => None,
    })
    .next_back();
  if last_element != Some(paragraph) {
    failures.push(outcome(NON_GOALS_LINE, false, "elements follow p.non-goals in High Level Solution"));
  }
  if element_text(&context.nodes, paragraph).trim().is_empty() {
    failures.push(outcome(NON_GOALS_LINE, false, "p.non-goals is empty"));
  }
  if header_names_understanding(context) && !paragraph_links_understanding(context, paragraph) {
    failures.push(outcome(
      NON_GOALS_LINE,
      false,
      "p.non-goals does not link the understanding page the header's meta chips name",
    ));
  }
  for element in descendants(&context.nodes, 0) {
    if context.nodes[element].tag != "p" || context.nodes[element].classes.contains("non-goals") {
      continue;
    }
    let text = str_methods::collapse_whitespace(&element_text(&context.nodes, element));
    let Some(prefix) = NON_GOALS_PREFIXES.iter().find(|prefix| text.starts_with(**prefix)) else {
      continue;
    };
    failures.push(outcome(
      NON_GOALS_LINE,
      false,
      format!(
        "paragraph in section {} starts with the exclusion prefix {} outside p.non-goals",
        text_repr::str_repr(&section_heading(&context.nodes, element)),
        text_repr::str_repr(prefix)
      ),
    ));
  }
  if failures.is_empty() {
    failures.push(outcome(NON_GOALS_LINE, true, ""));
  }
  Ok(failures)
}

/// Whether the page's header meta chips reference an `understanding_*.html` page: a chip's text or attribute names it.
fn header_names_understanding(context: &Context<'_>) -> bool {
  find(&context.nodes, 0, "span", &["mtag"]).iter().any(|&chip| {
    let text = element_text(&context.nodes, chip);
    let attributes = context.nodes[chip]
      .attrs
      .values()
      .flatten()
      .any(|value| value.contains("understanding_") && value.contains(".html"));
    (text.contains("understanding_") && text.contains(".html")) || attributes
  })
}

/// Whether the paragraph carries a link to an `understanding_*.html` page.
fn paragraph_links_understanding(context: &Context<'_>, paragraph: usize) -> bool {
  find(&context.nodes, paragraph, "a", &[]).iter().any(|&link| {
    context.nodes[link]
      .attrs
      .get("href")
      .and_then(Option::as_deref)
      .is_some_and(|href| href.contains("understanding_") && href.contains(".html"))
  })
}

/// Whether the page carries exactly one `p.non-goals` paragraph. An amend runs `non-goals-line` only when the
/// lineage's first version carries one.
pub(crate) fn page_has_non_goals(source: &str) -> anyhow::Result<bool> {
  Ok(find(&parse_dom(source)?, 0, "p", &["non-goals"]).len() == 1)
}

/// The collapsed text of the section whose first h2 ends with `title`, or `None` when no section carries one. The
/// text is HTML-escaped, so a diff of two sections compares their words and never parses them as markup.
pub(crate) fn section_text(source: &str, title: &str) -> anyhow::Result<Option<String>> {
  let nodes = parse_dom(source)?;
  let Some(section) = numbered_section(&nodes, title) else {
    return Ok(None);
  };
  Ok(Some(charliebot_html::escape(
    &str_methods::collapse_whitespace(&element_text(&nodes, section)),
    charliebot_html::EscapeContext::Text,
  )))
}

/// The named genre's assertion set without the named assertions, as the `only` list of `run_assertions`. `None` means
/// the genre has no set.
pub(crate) fn assertion_names_excluding(genre: &str, excluded: &[&str]) -> Option<Vec<String>> {
  assertion_set(genre)
    .map(|names| names.iter().copied().filter(|name| !excluded.contains(name)).map(String::from).collect())
}

fn check_foot_present(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  if find(&context.nodes, 0, "div", &["foot"]).is_empty() {
    Ok(vec![outcome(FOOT_PRESENT, false, "no div.foot on the page")])
  } else {
    Ok(vec![outcome(FOOT_PRESENT, true, "")])
  }
}

fn open_forks(nodes: &[Element]) -> Vec<(usize, usize)> {
  find(nodes, 0, "div", &["fork"])
    .into_iter()
    .enumerate()
    .filter_map(|(index, fork)| find(nodes, fork, "p", &["resolved"]).is_empty().then_some((index + 1, fork)))
    .collect()
}

fn check_fork_open_shape(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut failures = Vec::new();
  for (index, fork) in open_forks(&context.nodes) {
    let questions = find(&context.nodes, fork, "p", &["q"]);
    let question_has_fn = questions.iter().any(|&question| !find(&context.nodes, question, "span", &["fn"]).is_empty());
    let mut missing = Vec::new();
    if !question_has_fn {
      missing.push("p.q with span.fn");
    }
    if find(&context.nodes, fork, "p", &["rec"]).is_empty() {
      missing.push("p.rec");
    }
    if find(&context.nodes, fork, "p", &["trade"]).is_empty() {
      missing.push("p.trade");
    }
    if !missing.is_empty() {
      failures.push(outcome(
        FORK_OPEN_SHAPE,
        false,
        format!(
          "fork #{index} (section {}) is missing {}",
          text_repr::str_repr(&section_heading(&context.nodes, fork)),
          missing.join(", ")
        ),
      ));
    }
  }
  if failures.is_empty() {
    failures.push(outcome(FORK_OPEN_SHAPE, true, ""));
  }
  Ok(failures)
}

/// Whether a `details` that folds at open lies between `element` and its ancestor `up_to`, or among all of its
/// ancestors when `up_to` is `None`.
fn folded_ancestor(nodes: &[Element], element: usize, up_to: Option<usize>) -> bool {
  let mut ancestor = nodes[element].parent;
  while let Some(index) = ancestor {
    if Some(index) == up_to {
      return false;
    }
    let node = &nodes[index];
    if folds_at_open(&node.tag, node.attrs.keys().map(String::as_str)) {
      return true;
    }
    ancestor = node.parent;
  }
  false
}

/// Whether an element carrying `class` lies between `element` and its ancestor `up_to`, or among all of its
/// ancestors when `up_to` is `None`.
fn ancestor_with_class(nodes: &[Element], element: usize, up_to: Option<usize>, class: &str) -> bool {
  let mut ancestor = nodes[element].parent;
  while let Some(index) = ancestor {
    if Some(index) == up_to {
      return false;
    }
    if nodes[index].classes.contains(class) {
      return true;
    }
    ancestor = nodes[index].parent;
  }
  false
}

/// The `li` lines of one fork that no folded `details` holds and no `div.demo` holds. These are the options the page
/// shows at open, with the lists and menus a demo draws left out of the option count.
fn visible_option_lines(nodes: &[Element], fork: usize) -> Vec<usize> {
  find(nodes, fork, "li", &[])
    .into_iter()
    .filter(|&line| !folded_ancestor(nodes, line, Some(fork)) && !ancestor_with_class(nodes, line, Some(fork), "demo"))
    .collect()
}

/// The name an option line opens with, in 「」, when a non-empty line follows the closing bracket.
fn option_name(line: &str) -> Option<&str> {
  let rest = line.trim_start().strip_prefix('「')?;
  let (name, after) = rest.split_once('」')?;
  let name = name.trim();
  (!name.is_empty() && !after.trim().is_empty()).then_some(name)
}

/// The 「...」, “...” and "..." spans of a line, in order.
fn quoted_spans(text: &str) -> Vec<&str> {
  let mut spans = Vec::new();
  let mut rest = text;
  while let Some((start, opener)) = rest.char_indices().find(|(_, character)| matches!(character, '「' | '“' | '"'))
  {
    let closer = match opener {
      '「' => '」',
      '“' => '”',
      _ => '"',
    };
    let after = &rest[start + opener.len_utf8()..];
    let Some(end) = after.find(closer) else { break };
    spans.push(after[..end].trim());
    rest = &after[end + closer.len_utf8()..];
  }
  spans
}

/// One open fork of a page, read as a pick: its question number, its named options as they show at open, and the
/// name the recommendation quotes. The page script reads the same grammar in
/// `web/features/artifacts/static/artifact-comments.js`. The fixture `tests/data/plan_template_forks.json` pins both
/// readings to one output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg(test)]
struct PageFork {
  n: String,
  options: Vec<PageForkOption>,
  rec: Option<String>,
}

/// One named option: the name, which is the button text, and the option line as the page writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg(test)]
struct PageForkOption {
  name: String,
  text: String,
}

/// The forks of `html` a reader can pick: open, outside every closed fold, with a question number and at least two
/// named options. A fork without them has no pick.
#[cfg(test)]
fn page_forks(html: &str) -> anyhow::Result<Vec<PageFork>> {
  let nodes = parse_dom(html)?;
  let mut forks = Vec::new();
  for (_, fork) in open_forks(&nodes) {
    if folded_ancestor(&nodes, fork, None) {
      continue;
    }
    let Some(n) = fork_number(&nodes, fork) else { continue };
    let options: Vec<PageForkOption> = visible_option_lines(&nodes, fork)
      .into_iter()
      .filter_map(|line| {
        let text = str_methods::collapse_whitespace(&option_line_text(&nodes, line));
        let name = option_name(&text)?.to_owned();
        Some(PageForkOption { name, text })
      })
      .collect();
    if options.len() < 2 {
      continue;
    }
    let rec = find(&nodes, fork, "p", &["rec"]).first().and_then(|&rec| {
      let text = str_methods::collapse_whitespace(&element_text(&nodes, rec));
      quoted_spans(&text).into_iter().find(|span| options.iter().any(|option| option.name == *span)).map(str::to_owned)
    });
    forks.push(PageFork { n, options, rec });
  }
  Ok(forks)
}

/// The number span of the first question line of a fork that carries one.
#[cfg(test)]
fn fork_number(nodes: &[Element], fork: usize) -> Option<String> {
  find(nodes, fork, "p", &["q"]).into_iter().find_map(|question| {
    let span = *find(nodes, question, "span", &["fn"]).first()?;
    let number = str_methods::collapse_whitespace(&element_text(nodes, span));
    (!number.is_empty()).then_some(number)
  })
}

/// The text of an option line without its `div.demo` and `p.no-demo`, which draw or explain the option and name none.
#[cfg(test)]
fn option_line_text(nodes: &[Element], line: usize) -> String {
  fn append(nodes: &[Element], element: usize, output: &mut String) {
    for child in &nodes[element].children {
      match child {
        Child::Element(child)
          if nodes[*child].classes.contains("demo") || nodes[*child].classes.contains("no-demo") => {}
        Child::Element(child) => append(nodes, *child, output),
        Child::Text(text) => output.push_str(text),
      }
    }
  }
  let mut text = String::new();
  append(nodes, line, &mut text);
  text
}

fn check_fork_options(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut failures = Vec::new();
  for (index, fork) in open_forks(&context.nodes) {
    let section = text_repr::str_repr(&section_heading(&context.nodes, fork));
    if folded_ancestor(&context.nodes, fork, None) {
      failures.push(outcome(
        FORK_OPTIONS,
        false,
        format!("fork #{index} (section {section}) sits inside a closed fold"),
      ));
      continue;
    }
    let lines = visible_option_lines(&context.nodes, fork);
    let mut names = Vec::new();
    for (position, &line) in lines.iter().enumerate() {
      let text = str_methods::collapse_whitespace(&element_text(&context.nodes, line));
      match option_name(&text) {
        Some(name) => names.push(name.to_owned()),
        None => failures.push(outcome(
          FORK_OPTIONS,
          false,
          format!("fork #{index} (section {section}) option line {} does not open with its name in 「」", position + 1),
        )),
      }
    }
    if lines.len() < 2 {
      failures.push(outcome(
        FORK_OPTIONS,
        false,
        format!("fork #{index} (section {section}) has {} option line outside folds (2 required)", lines.len()),
      ));
    }
    if let Some(&recommendation) = find(&context.nodes, fork, "p", &["rec"]).first() {
      let text = str_methods::collapse_whitespace(&element_text(&context.nodes, recommendation));
      let quoted = quoted_spans(&text);
      if !quoted.iter().any(|span| names.iter().any(|name| name == span)) {
        let listed = if quoted.is_empty() {
          "none".to_owned()
        } else {
          quoted.iter().map(|span| format!("「{span}」")).collect::<Vec<_>>().join(", ")
        };
        failures.push(outcome(
          FORK_OPTIONS,
          false,
          format!("fork #{index} (section {section}) p.rec quotes no option name (quoted: {listed})"),
        ));
      }
    }
  }
  if failures.is_empty() {
    failures.push(outcome(FORK_OPTIONS, true, ""));
  }
  Ok(failures)
}

/// A `.demo` counts when it draws something: an element child or non-whitespace text. An SVG-only demo counts.
fn demo_has_content(nodes: &[Element], demo: usize) -> bool {
  nodes[demo].children.iter().any(|child| match child {
    Child::Element(_) => true,
    Child::Text(text) => !text.trim().is_empty(),
  })
}

/// The `.demo` and `.no-demo` descendants of one option line, in document order.
fn option_markers(nodes: &[Element], line: usize) -> (Vec<usize>, Vec<usize>) {
  let mut demos = Vec::new();
  let mut reasons = Vec::new();
  for element in descendants(nodes, line) {
    if nodes[element].classes.contains("demo") {
      demos.push(element);
    }
    if nodes[element].classes.contains("no-demo") {
      reasons.push(element);
    }
  }
  (demos, reasons)
}

/// `fork-demos`: every option of a fork whose option lines carry a demo or a reason carries one of the two. A demo
/// in the fork's folded explainer sits on no option line, so it starts nothing.
fn check_fork_demos(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut failures = Vec::new();
  for (index, fork) in open_forks(&context.nodes) {
    let lines = visible_option_lines(&context.nodes, fork);
    let markers = lines.iter().map(|&line| option_markers(&context.nodes, line)).collect::<Vec<_>>();
    if !markers.iter().any(|(demos, reasons)| !demos.is_empty() || !reasons.is_empty()) {
      continue;
    }
    let section = text_repr::str_repr(&section_heading(&context.nodes, fork));
    for (position, ((demos, reasons), &line)) in markers.iter().zip(&lines).enumerate() {
      let drawn = demos
        .iter()
        .any(|&demo| !folded_ancestor(&context.nodes, demo, Some(line)) && demo_has_content(&context.nodes, demo));
      let stated = reasons
        .iter()
        .any(|&reason| !str_methods::collapse_whitespace(&element_text(&context.nodes, reason)).is_empty());
      if drawn || stated {
        continue;
      }
      let gap = if demos.iter().any(|&demo| folded_ancestor(&context.nodes, demo, Some(line))) {
        "the demo sits in a closed fold"
      } else if !reasons.is_empty() {
        "the reason is empty"
      } else {
        "the demo is missing"
      };
      let name = match option_name(&str_methods::collapse_whitespace(&element_text(&context.nodes, line))) {
        Some(name) => format!("「{name}」"),
        None => format!("line {}", position + 1),
      };
      failures.push(outcome(FORK_DEMOS, false, format!("fork #{index} (section {section}) option {name}: {gap}")));
    }
  }
  if failures.is_empty() {
    failures.push(outcome(FORK_DEMOS, true, ""));
  }
  Ok(failures)
}

fn check_fork_explainer(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut failures = Vec::new();
  for (index, fork) in open_forks(&context.nodes) {
    let layers = find(&context.nodes, fork, "details", &["details-layer"]);
    if layers.is_empty() {
      failures.push(outcome(
        FORK_EXPLAINER,
        false,
        format!(
          "fork #{index} (section {}) has no details.details-layer",
          text_repr::str_repr(&section_heading(&context.nodes, fork))
        ),
      ));
    } else {
      let has_body = descendants(&context.nodes, layers[0]).into_iter().any(|element| {
        EXPLAINER_BODY_TAGS.contains(&context.nodes[element].tag.as_str())
          && !str_methods::split_whitespace(&element_text(&context.nodes, element)).is_empty()
      });
      if !has_body {
        failures.push(outcome(
          FORK_EXPLAINER,
          false,
          format!(
            "fork #{index} (section {}) explainer has no body",
            text_repr::str_repr(&section_heading(&context.nodes, fork))
          ),
        ));
      }
    }
  }
  if failures.is_empty() {
    failures.push(outcome(FORK_EXPLAINER, true, ""));
  }
  Ok(failures)
}

fn check_goal_section(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let source = fs::read_to_string(context.artifact)
    .with_context(|| format!("could not read artifact {}", context.artifact.display()))?;
  if !GOAL_SECTION_RE.is_match(&source)? {
    return Ok(vec![outcome(
      GOAL_SECTION,
      false,
      format!(
        "artifact {} has no 'Problem / Goal' section",
        text_repr::str_repr(&context.artifact.file_name().expect("an artifact path has a name").to_string_lossy())
      ),
    )]);
  }
  Ok(vec![outcome(GOAL_SECTION, true, "Problem / Goal section present")])
}

fn check_page_height(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let detail = if context.chrome_bin.is_empty() {
    "not measured: headless_chrome_bin is not configured".to_owned()
  } else {
    let chrome_bin = PathBuf::from(context.chrome_bin);
    if !chrome_bin.exists() {
      format!("not measured: configured headless_chrome_bin path does not exist: {}", chrome_bin.display())
    } else {
      match measure_page_height(&chrome_bin, context.artifact)? {
        Ok(height) if height > PAGE_HEIGHT_TARGET => {
          format!(
            "{height} px: {} px over the {PAGE_HEIGHT_TARGET} px target; fold secondary material",
            height - PAGE_HEIGHT_TARGET
          )
        }
        Ok(height) => format!("{height} px (target {PAGE_HEIGHT_TARGET})"),
        Err(message) => format!("not measured: {message}"),
      }
    }
  };
  Ok(vec![outcome(PAGE_HEIGHT, true, detail)])
}

/// The inner error is a renderer failure. The check reports it as an informational outcome. Any other error,
/// such as a probe that cannot be written, propagates and fails the check run.
fn measure_page_height(chrome_bin: &Path, artifact: &Path) -> anyhow::Result<Result<i32, String>> {
  let probe = artifact
    .parent()
    .expect("an artifact path has a parent")
    .join(format!(".page-height-probe-{}.html", Uuid::new_v4().simple()));
  let artifact_uri = Url::from_file_path(fs::canonicalize(artifact)?)
    .map_err(|()| anyhow!("could not build a file URL for artifact {}", artifact.display()))?;
  let page =
    PAGE_PROBE_TEMPLATE.replace("{width}", &PAGE_PROBE_WIDTH_PX.to_string()).replace("{src}", artifact_uri.as_str());
  fs::write(&probe, page.as_bytes())?;
  let result = render_probe(chrome_bin, &probe);
  fs::remove_file(&probe)?;
  result
}

fn render_probe(chrome_bin: &Path, probe: &Path) -> anyhow::Result<Result<i32, String>> {
  let probe_uri = Url::from_file_path(fs::canonicalize(probe)?)
    .map_err(|()| anyhow!("could not build a file URL for probe {}", probe.display()))?;
  Ok(headless_render::render_height(chrome_bin, probe_uri.as_str()))
}

fn decimal_integer(text: &str) -> Result<String, String> {
  let mut digits = String::with_capacity(text.len());
  for character in text.chars() {
    let Some(digit) = str_methods::decimal_value(character) else {
      return Err(format!("invalid literal for int() with base 10: {}", text_repr::str_repr(text)));
    };
    digits.push(char::from(b'0' + digit));
  }
  let normalized = digits.trim_start_matches('0');
  Ok(if normalized.is_empty() { "0" } else { normalized }.to_owned())
}

fn check_ordinal_named(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let (named, flagged) = ordinal_named_scan(context)?;
  if flagged.is_empty() {
    return Ok(vec![outcome(ORDINAL_NAMED, true, format!("{} external labels named in reach", named.len()))]);
  }
  Ok(flagged
    .into_iter()
    .map(|(block, labels, sentence)| {
      let joined = if labels.len() == 1 { labels[0].clone() } else { labels.join(", ") };
      let snippet = sentence.chars().take(160).collect::<String>();
      outcome(
        ORDINAL_NAMED,
        false,
        format!(
          "(section {}) {} names something outside this page and no content name is in reach; name it at first use (path, title, ticket, or a parenthetical): {snippet}",
          text_repr::str_repr(&section_heading(&context.nodes, block)),
          text_repr::str_repr(&joined)
        ),
      )
    })
    .collect())
}

fn ordinal_named_scan(context: &Context<'_>) -> anyhow::Result<(HashSet<String>, Vec<OrdinalFailure>)> {
  let h2_count = find(&context.nodes, 0, "h2", &[]).len();
  let fork_count = find(&context.nodes, 0, "div", &["fork"]).len();
  let section_numbers = find(&context.nodes, 0, "span", &["sn"])
    .into_iter()
    .map(|element| str_methods::collapse_whitespace(&element_text(&context.nodes, element)))
    .collect::<HashSet<_>>();
  let mut named = HashSet::new();
  let mut flagged = Vec::new();
  for block in ordinal_blocks(&context.nodes) {
    let block_text = block_scan_text(&context.nodes, block);
    let collapsed = str_methods::collapse_whitespace(&block_text);
    let text = unescape(&collapsed);
    for sentence in SENTENCE_SPLIT.split(&text) {
      let sentence = str_methods::strip(sentence?);
      if sentence.is_empty() {
        continue;
      }
      let sentence = QUOTE.replace_all(sentence, "「…」").into_owned();
      let code_text = split_code_spans(&sentence);
      let matches = ORD.captures_iter(&code_text.text).collect::<Result<Vec<_>, _>>()?;
      let mut hits = Vec::new();
      for captures in matches {
        let found = captures.get(0).expect("an ordinal capture has a whole match");
        if code_text.code_ranges.iter().any(|&(start, end)| start <= found.start() && found.start() < end) {
          continue;
        }
        let kind = found.as_str();
        let number = (1..=3).find_map(|index| captures.get(index).map(|value| value.as_str()));
        let before = &code_text.text[..found.start()];
        let external = QUAL.is_match(before)?;
        let mut own = false;
        if !external {
          if INTERNAL_UNLESS_QUALIFIED.is_match(kind)? {
            continue;
          }
          let dotted = match (captures.get(4), captures.get(5), captures.get(6), captures.get(7)) {
            (Some(first), Some(second), _, _) => Some((first.as_str(), second.as_str())),
            (_, _, Some(first), Some(second)) => Some((first.as_str(), second.as_str())),
            _ => None,
          };
          if let Some((first, second)) = dotted {
            own = section_numbers.contains(&format!("{first}.{second}"))
              || (1..=h2_count).contains(&first.parse::<usize>().expect("the dotted section has one ASCII digit"));
          } else if contains_any(kind, &["节", "章", "section", "Section"]) {
            own = if let Some(number) = number {
              ordinal_number(number)?.is_some_and(|value| value <= h2_count)
            } else {
              false
            };
          } else if contains_any(
            kind,
            &[
              "项",
              "条",
              "题",
              "问",
              "Trade",
              "fork",
              "Fork",
              "divergence",
              "Divergence",
              "分歧",
              "item",
              "Item",
              "question",
              "Question",
            ],
          ) {
            own = if let Some(number) = number {
              ordinal_number(number)?.is_some_and(|value| value >= 1 && value <= fork_count)
            } else {
              false
            };
          }
        }
        if own {
          continue;
        }
        let opening = before.rfind(['（', '(']);
        let closing = before.rfind(['）', ')']);
        if opening.is_some_and(|opening| Some(opening) > closing)
          && str_methods::strip(&before[..opening.unwrap()]).chars().count() >= 2
        {
          named.insert(ordinal_label(kind));
          continue;
        }
        let rest = &code_text.text[found.end()..];
        if GLOSS.is_match(rest)? || (trim_chars(before, " -—·1234567890.").is_empty() && COLON_GLOSS.is_match(rest)?)
        {
          named.insert(ordinal_label(kind));
          continue;
        }
        if context.nodes[block].tag == "span"
          && context.nodes[block].classes.contains("mtag")
          && trim_chars(rest, " ·").chars().count() >= 2
        {
          named.insert(ordinal_label(kind));
          continue;
        }
        hits.push(kind.to_owned());
      }
      if hits.is_empty() {
        continue;
      }
      if ANCHOR.is_match(&code_text.text)? || TITLE_ANCHOR.is_match(&code_text.text)? {
        for hit in hits {
          named.insert(ordinal_label(&hit));
        }
        continue;
      }
      let rest = hits.into_iter().filter(|hit| !named.contains(&ordinal_label(hit))).collect::<Vec<_>>();
      if !rest.is_empty() {
        flagged.push((block, rest, code_text.text));
      }
    }
  }
  Ok((named, flagged))
}

fn ordinal_blocks(nodes: &[Element]) -> Vec<usize> {
  fn walk(nodes: &[Element], element: usize, output: &mut Vec<usize>) {
    for child in &nodes[element].children {
      let Child::Element(child) = child else { continue };
      let node = &nodes[*child];
      if ORDINAL_SKIP_TAGS.contains(&node.tag.as_str()) {
        continue;
      }
      if ORDINAL_BLOCK_TAGS.contains(&node.tag.as_str()) || (node.tag == "span" && node.classes.contains("mtag")) {
        output.push(*child);
      }
      walk(nodes, *child, output);
    }
  }
  let mut blocks = Vec::new();
  walk(nodes, 0, &mut blocks);
  blocks
}

fn block_scan_text(nodes: &[Element], block: usize) -> String {
  fn walk(nodes: &[Element], element: usize, output: &mut String) {
    for child in &nodes[element].children {
      match child {
        Child::Text(text) => output.push_str(text),
        Child::Element(child) => {
          let node = &nodes[*child];
          if ORDINAL_SKIP_TAGS.contains(&node.tag.as_str())
            || ORDINAL_BLOCK_TAGS.contains(&node.tag.as_str())
            || (node.tag == "span" && node.classes.contains("mtag"))
          {
            continue;
          }
          if node.tag == "code" {
            output.push(CODE_START);
            walk(nodes, *child, output);
            output.push(CODE_END);
          } else {
            walk(nodes, *child, output);
          }
        }
      }
    }
  }
  let mut output = String::new();
  walk(nodes, block, &mut output);
  output
}

fn split_code_spans(source: &str) -> CodeText {
  let mut text = String::new();
  let mut code_ranges = Vec::new();
  let mut start = None;
  for character in source.chars() {
    if character == CODE_START {
      start = Some(text.len());
    } else if character == CODE_END {
      if let Some(start) = start.take() {
        code_ranges.push((start, text.len()));
      }
    } else {
      text.push(character);
    }
  }
  if let Some(start) = start {
    code_ranges.push((start, text.len()));
  }
  CodeText { text, code_ranges }
}

fn ordinal_label(kind: &str) -> String {
  str_methods::collapse_whitespace(kind)
}

fn ordinal_number(number: &str) -> anyhow::Result<Option<usize>> {
  if str_methods::isdigit(number) {
    return Ok(decimal_integer(number).ok().and_then(|value| value.parse().ok()));
  }
  if number == "十" {
    return Ok(Some(10));
  }
  if number.contains('十') {
    let (tens, ones) = number.split_once('十').expect("the numeral contains ten");
    if ones.contains('十') {
      bail!("too many values to unpack (expected 2)");
    }
    let tens = if tens.is_empty() { 1 } else { cjk_digit(tens).unwrap_or(1) };
    let ones = if ones.is_empty() { 0 } else { cjk_digit(ones).unwrap_or(0) };
    return Ok(Some(tens * 10 + ones));
  }
  Ok(Some(if number.chars().count() == 1 { cjk_digit(number).unwrap_or(99) } else { 99 }))
}

fn cjk_digit(text: &str) -> Option<usize> {
  match text {
    "零" => Some(0),
    "一" => Some(1),
    "二" => Some(2),
    "三" => Some(3),
    "四" => Some(4),
    "五" => Some(5),
    "六" => Some(6),
    "七" => Some(7),
    "八" => Some(8),
    "九" => Some(9),
    _ => None,
  }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
  needles.iter().any(|needle| text.contains(needle))
}

fn trim_chars<'a>(text: &'a str, characters: &str) -> &'a str {
  text.trim_matches(|character| characters.contains(character))
}

/// The page carries a `tag` with these classes.
fn check_explain_triad(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  if find(&context.nodes, 0, "div", &["triad"]).is_empty() {
    Ok(vec![outcome(EXPLAIN_TRIAD, false, "no div.triad on the page")])
  } else {
    Ok(vec![outcome(EXPLAIN_TRIAD, true, "")])
  }
}

/// `explain-figures`: a page without a `div.step` passes. Each `div.step` holds an svg of at least
/// `FIGURE_MIN_ELEMENTS` shape or text elements, or an img with a src. In a section that holds steps, every p sits in a
/// step, apart from one opening p before the first step.
fn check_explain_figures(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let steps = find(&context.nodes, 0, "div", &["step"]);
  if steps.is_empty() {
    return Ok(vec![outcome(EXPLAIN_FIGURES, true, "")]);
  }
  let mut failures = Vec::new();
  for (index, &step) in steps.iter().enumerate() {
    let index = index + 1;
    let heading = text_repr::str_repr(&section_heading(&context.nodes, step));
    let svgs = find(&context.nodes, step, "svg", &[]);
    let imgs = find(&context.nodes, step, "img", &[]);
    if svgs.is_empty() && imgs.is_empty() {
      failures.push(outcome(
        EXPLAIN_FIGURES,
        false,
        format!("step #{index} (section {heading}) has no svg or img figure"),
      ));
    }
    for &svg in &svgs {
      let elements = descendants(&context.nodes, svg)
        .into_iter()
        .filter(|&element| FIGURE_ELEMENT_TAGS.contains(&context.nodes[element].tag.as_str()))
        .count();
      if elements < FIGURE_MIN_ELEMENTS {
        failures.push(outcome(
          EXPLAIN_FIGURES,
          false,
          format!(
            "step #{index} (section {heading}) has an svg with fewer than {FIGURE_MIN_ELEMENTS} shape or text elements"
          ),
        ));
      }
    }
    for &img in &imgs {
      let has_src =
        context.nodes[img].attrs.get("src").and_then(Option::as_deref).is_some_and(|src| !src.trim().is_empty());
      if !has_src {
        failures.push(outcome(
          EXPLAIN_FIGURES,
          false,
          format!("step #{index} (section {heading}) has an img without src"),
        ));
      }
    }
  }
  for section in find(&context.nodes, 0, "section", &[]) {
    let section_steps = find(&context.nodes, section, "div", &["step"]);
    let Some(&first_step) = section_steps.first() else {
      continue;
    };
    let inside_steps = section_steps.iter().flat_map(|&step| descendants(&context.nodes, step)).collect::<HashSet<_>>();
    let outside = find(&context.nodes, section, "p", &[])
      .into_iter()
      .filter(|paragraph| !inside_steps.contains(paragraph))
      .collect::<Vec<_>>();
    // The one p before the first step is the opening paragraph. Every other p outside the steps is a failure.
    let opening = outside.first().is_some_and(|&paragraph| paragraph < first_step);
    let extra = outside.len() - usize::from(opening);
    if extra > 0 {
      let heading = text_repr::str_repr(&named_section(&context.nodes, section));
      let after_opening = if opening { " after its opening p" } else { "" };
      failures.push(outcome(
        EXPLAIN_FIGURES,
        false,
        format!("section {heading} has {extra} p blocks outside div.step{after_opening}"),
      ));
    }
  }
  if failures.is_empty() { Ok(vec![outcome(EXPLAIN_FIGURES, true, "")]) } else { Ok(failures) }
}

/// `explain-examples`: a page with a `div.step` holds a `div.example.e2e` and a `div.example.contrast`. Each
/// `div.example` names its source in a `span.src`.
fn check_explain_examples(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut failures = Vec::new();
  let has_steps = !find(&context.nodes, 0, "div", &["step"]).is_empty();
  for (kind, label) in [("e2e", "div.example.e2e"), ("contrast", "div.example.contrast")] {
    if has_steps && find(&context.nodes, 0, "div", &["example", kind]).is_empty() {
      failures.push(outcome(EXPLAIN_EXAMPLES, false, format!("no {label} on the page")));
    }
  }
  for (index, example) in find(&context.nodes, 0, "div", &["example"]).into_iter().enumerate() {
    if find(&context.nodes, example, "span", &["src"]).is_empty() {
      let index = index + 1;
      let heading = text_repr::str_repr(&section_heading(&context.nodes, example));
      failures.push(outcome(EXPLAIN_EXAMPLES, false, format!("example #{index} (section {heading}) has no span.src")));
    }
  }
  if failures.is_empty() { Ok(vec![outcome(EXPLAIN_EXAMPLES, true, "")]) } else { Ok(failures) }
}

/// Every `span.tag.fact` label sits in a block that also carries a `span.src`.
fn check_fact_anchored(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut failures = Vec::new();
  for (index, fact) in find(&context.nodes, 0, "span", &["tag", "fact"]).into_iter().enumerate() {
    let index = index + 1;
    let mut block = context.nodes[fact].parent;
    while let Some(current) = block {
      if FACT_ANCESTOR_TAGS.contains(&context.nodes[current].tag.as_str()) {
        break;
      }
      block = context.nodes[current].parent;
    }
    let heading = text_repr::str_repr(&section_heading(&context.nodes, fact));
    match block {
      None => failures.push(outcome(
        FACT_ANCHORED,
        false,
        format!("fact label #{index} (section {heading}) has no p/li/td/blockquote/h3 ancestor"),
      )),
      Some(block) => {
        if find(&context.nodes, block, "span", &["src"]).is_empty() {
          let tag = &context.nodes[block].tag;
          failures.push(outcome(
            FACT_ANCHORED,
            false,
            format!("fact label #{index} (section {heading}) sits in a {tag} with no span.src"),
          ));
        }
      }
    }
  }
  if failures.is_empty() { Ok(vec![outcome(FACT_ANCHORED, true, "")]) } else { Ok(failures) }
}

/// The text carries a Unicode letter or digit. Punctuation alone is not a gloss.
fn has_readable_text(text: &str) -> bool {
  text.chars().any(str_methods::is_alphanumeric)
}

/// The block's text with the target element fenced by the requirement sentinels, so the text
/// after the chip is unambiguous.
fn marked_element_text(nodes: &[Element], element: usize, target: usize) -> String {
  fn walk(nodes: &[Element], element: usize, target: usize, output: &mut String) {
    for child in &nodes[element].children {
      match child {
        Child::Element(child) if *child == target => {
          output.push(REQ_START);
          output.push_str(&element_text(nodes, *child));
          output.push(REQ_END);
        }
        Child::Element(child) => walk(nodes, *child, target, output),
        Child::Text(text) => output.push_str(text),
      }
    }
  }
  let mut output = String::new();
  walk(nodes, element, target, &mut output);
  output
}

/// Each `span.req` chip carries a gloss, readable text after the chip in the sentence that holds
/// it, before that sentence's terminator. A page with no chip passes.
fn check_req_chips(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let chips = find(&context.nodes, 0, "span", &["req"]);
  if chips.is_empty() {
    return Ok(vec![outcome(REQ_CHIPS, true, "no span.req requirement chips on the page")]);
  }
  let mut failures = Vec::new();
  for (index, chip) in chips.iter().enumerate() {
    let index = index + 1;
    let mut block = context.nodes[*chip].parent;
    while let Some(current) = block {
      if REQ_BLOCK_TAGS.contains(&context.nodes[current].tag.as_str()) {
        break;
      }
      block = context.nodes[current].parent;
    }
    let heading = text_repr::str_repr(&section_heading(&context.nodes, *chip));
    let chip_text = text_repr::str_repr(&str_methods::collapse_whitespace(&element_text(&context.nodes, *chip)));
    let Some(block) = block else {
      failures.push(outcome(
        REQ_CHIPS,
        false,
        format!(
          "requirement chip #{index} {chip_text} (section {heading}) has no p/li/td/blockquote ancestor to hold a gloss"
        ),
      ));
      continue;
    };
    let marked = marked_element_text(&context.nodes, block, *chip);
    let after = marked.split_once(REQ_END).map(|(_, after)| after).unwrap_or("");
    let tail = after.split(SENTENCE_END).next().unwrap_or("");
    if !has_readable_text(tail) {
      failures.push(outcome(
        REQ_CHIPS,
        false,
        format!("requirement chip #{index} {chip_text} (section {heading}) carries no gloss after it in its sentence"),
      ));
    }
  }
  if failures.is_empty() {
    Ok(vec![outcome(REQ_CHIPS, true, format!("{} requirement chips carry a gloss", chips.len()))])
  } else {
    Ok(failures)
  }
}

/// Pre-rendered `class="katex"` markup, or the full KaTeX script injection.
fn check_render_path(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let all = descendants(&context.nodes, 0);
  if all.iter().any(|&element| context.nodes[element].classes.contains("katex")) {
    return Ok(vec![outcome(RENDER_PATH, true, "pre-rendered class=\"katex\" markup")]);
  }
  let carries = |attribute: &str, hint: &str| {
    all.iter().any(|&element| {
      context.nodes[element].attrs.get(attribute).and_then(Option::as_deref).is_some_and(|value| value.contains(hint))
    })
  };
  let has_css = carries("href", KATEX_CSS_HINT);
  let has_katex_js = carries("src", KATEX_JS_HINT);
  let has_auto_render = carries("src", KATEX_AUTO_RENDER_HINT);
  if has_css && has_katex_js && has_auto_render {
    return Ok(vec![outcome(RENDER_PATH, true, "KaTeX script injection")]);
  }
  Ok(vec![outcome(
    RENDER_PATH,
    false,
    format!(
      "no math render path: neither pre-rendered class=\"katex\" markup nor the KaTeX script injection ({KATEX_CSS_HINT} link + {KATEX_JS_HINT} + {KATEX_AUTO_RENDER_HINT})"
    ),
  )])
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::io::Read;
  use std::path::{Path, PathBuf};

  use charliebot_shell_harness::write_executable;
  use charliebot_tasks::spawner_prompt::PLAN_TEMPLATE_PATH;
  use serde_json::{Value, json};

  use super::*;
  use crate::test_support::{chrome_program, plan_page, repo_root};

  fn write_page(directory: &Path, name: &str, source: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, source).unwrap();
    path
  }

  fn run_assertions_sync(
    genre: &str,
    artifact: &Path,
    chrome_bin: &str,
    repo_root: &Path,
  ) -> anyhow::Result<Vec<AssertionOutcome>> {
    let home = tempfile::tempdir()?;
    fs::create_dir(home.path().join("sessions"))?;
    run_assertions_sync_with_user_text(
      genre,
      artifact,
      chrome_bin,
      repo_root,
      &UserTextChecksConfig::default(),
      home.path(),
      None,
    )
  }

  fn run_plan(path: &Path, chrome: &str) -> Vec<AssertionOutcome> {
    run_plan_with_height(path, chrome, 800)
  }

  fn run_plan_with_height(path: &Path, chrome: &str, height: i32) -> Vec<AssertionOutcome> {
    headless_render::set_test_height(Path::new(chrome), height);
    run_assertions_sync("plan", path, chrome, &repo_root()).unwrap()
  }

  fn by_name<'a>(outcomes: &'a [AssertionOutcome], name: &str) -> Vec<&'a AssertionOutcome> {
    outcomes.iter().filter(|outcome| outcome.name == name).collect()
  }

  fn schema_dropped_outcome(source: &str) -> AssertionOutcome {
    let nodes = parse_dom(source).unwrap();
    let context = Context { genre: "plan", artifact: Path::new(""), chrome_bin: "", repo_root: Path::new(""), nodes };
    check_schema_dropped(&context).unwrap().into_iter().next().unwrap()
  }

  fn schema_dropped_page(content: &str) -> String {
    format!(
      "<section><h3><span class=\"sn\">4.1</span> Schema</h3>{content}\
       <h3><span class=\"sn\">4.2</span> Design Details</h3><p class=\"dropped\">Outside 4.1.</p></section>"
    )
  }

  fn assert_schema_dropped_failure(source: &str, detail: &str) {
    let result = schema_dropped_outcome(source);
    assert_eq!(result.name, SCHEMA_DROPPED);
    assert!(!result.passed);
    assert_eq!(result.detail, detail);
  }

  const STRAY_HEAD_CSS_PAGE: &str = include_str!("../tests/data/understanding_stray_head_css.html");

  fn head_text_outcomes(source: &str) -> Vec<AssertionOutcome> {
    let nodes = parse_dom(source).unwrap();
    let context =
      Context { genre: "understanding", artifact: Path::new(""), chrome_bin: "", repo_root: Path::new(""), nodes };
    check_head_text(&context).unwrap()
  }

  /// The stray-CSS page with its block moved above the style element's close, and the orphan `</style>` dropped.
  fn with_stray_css_moved_into_style(page: &str) -> String {
    let style_close = page.find("</style>").unwrap();
    let head_close = page.find("</head>").unwrap();
    let orphan_close = page[..head_close].rfind("</style>").unwrap();
    let stray = &page[style_close + "</style>\n".len()..orphan_close];
    format!("{}{}</style>\n{}", &page[..style_close], stray, &page[head_close..])
  }

  #[test]
  fn head_text_fails_on_css_after_the_style_element() {
    let outcomes = head_text_outcomes(STRAY_HEAD_CSS_PAGE);
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.contains("<head>"), "{}", outcomes[0].detail);
  }

  #[test]
  fn head_text_passes_when_the_css_sits_inside_the_style_element() {
    let fixed = with_stray_css_moved_into_style(STRAY_HEAD_CSS_PAGE);
    assert_eq!(head_text_outcomes(&fixed), vec![outcome(HEAD_TEXT, true, "")]);
  }

  #[test]
  fn every_genre_runs_head_text() {
    for genre in ARTIFACT_GENRES {
      assert!(assertion_set(genre).unwrap().contains(&HEAD_TEXT), "{genre}");
    }
  }

  #[test]
  fn schema_dropped_fails_when_missing() {
    assert_schema_dropped_failure(&schema_dropped_page("<p>Schema body.</p>"), "missing p.dropped in 4.1 Schema");
  }

  #[test]
  fn schema_dropped_fails_when_more_than_one() {
    assert_schema_dropped_failure(
      &schema_dropped_page("<p class=\"dropped\">First.</p><p class=\"dropped\">Second.</p>"),
      "more than one p.dropped in 4.1 Schema (found 2)",
    );
  }

  #[test]
  fn schema_dropped_fails_when_inside_a_fold() {
    assert_schema_dropped_failure(
      &schema_dropped_page("<details><p class=\"dropped\">Dropped features: none.</p></details>"),
      "p.dropped is inside a fold",
    );
  }

  #[test]
  fn schema_dropped_fails_when_empty() {
    assert_schema_dropped_failure(&schema_dropped_page("<p class=\"dropped\">  </p>"), "p.dropped is empty");
  }

  #[test]
  fn schema_dropped_accepts_one_nonempty_line_before_the_next_subheading() {
    let result = schema_dropped_outcome(&schema_dropped_page("<p class=\"dropped\">Dropped features: none.</p>"));
    assert_eq!(result.name, SCHEMA_DROPPED);
    assert!(result.passed, "{result:#?}");
  }

  /// A minimal plan body whose section 3 holds `solution` and whose other sections hold `elsewhere`. The style block
  /// is empty: `non-goals-line` reads the DOM alone.
  fn non_goals_page(solution: &str, elsewhere: &str) -> String {
    format!(
      "<html><head><style></style></head><body>\
       <section><h2><span class=\"n\">1</span> Problem / Goal</h2><p>The goal.</p></section>\
       <section><h2><span class=\"n\">2</span> Context</h2>{elsewhere}</section>\
       <section><h2><span class=\"n nb\">3</span> High Level Solution</h2><p>The mechanism.</p>{solution}</section>\
       <section><h2><span class=\"n\">4</span> Detailed Design</h2><p>The design.</p></section>\
       <section><h2><span class=\"n\">5</span> Trade-offs</h2><p>The trade-offs.</p></section>\
       <section><h2><span class=\"n\">6</span> Other Details</h2><p>The details.</p></section>\
       </body></html>"
    )
  }

  fn non_goals_line(source: &str) -> AssertionOutcome {
    let nodes = parse_dom(source).unwrap();
    let context = Context { genre: "plan", artifact: Path::new(""), chrome_bin: "", repo_root: Path::new(""), nodes };
    check_non_goals_line(&context).unwrap().into_iter().next().unwrap()
  }

  #[test]
  fn non_goals_line_fails_when_section_3_has_no_non_goals_paragraph() {
    let result = non_goals_line(&non_goals_page("<p>The mechanism.</p>", "<p>Context.</p>"));
    assert_eq!(result.name, NON_GOALS_LINE);
    assert!(!result.passed);
    assert_eq!(result.detail, "missing p.non-goals in High Level Solution");
  }

  #[test]
  fn non_goals_line_fails_when_the_paragraph_sits_in_a_fold() {
    let solution =
      "<details class=\"details-layer\"><summary>Fold</summary><p class=\"non-goals\">Non-goals: none.</p></details>";
    let result = non_goals_line(&non_goals_page(solution, ""));
    assert!(!result.passed);
    assert_eq!(result.detail, "p.non-goals is inside a fold");
  }

  #[test]
  fn non_goals_line_fails_on_a_second_non_goals_paragraph() {
    let solution = "<p class=\"non-goals\">Non-goals: none.</p><p class=\"non-goals\">Non-goals: more.</p>";
    let result = non_goals_line(&non_goals_page(solution, ""));
    assert!(!result.passed);
    assert_eq!(result.detail, "more than one p.non-goals in High Level Solution (found 2)");
  }

  #[test]
  fn non_goals_line_fails_when_elements_follow_the_paragraph() {
    let solution = "<p class=\"non-goals\">Non-goals: none.</p><p>After the list.</p>";
    let result = non_goals_line(&non_goals_page(solution, ""));
    assert!(!result.passed);
    assert_eq!(result.detail, "elements follow p.non-goals in High Level Solution");
  }

  #[test]
  fn non_goals_line_fails_when_the_paragraph_is_empty() {
    let result = non_goals_line(&non_goals_page("<p class=\"non-goals\">  </p>", ""));
    assert!(!result.passed);
    assert_eq!(result.detail, "p.non-goals is empty");
  }

  #[test]
  fn non_goals_line_fails_when_a_header_chip_names_an_understanding_page_the_paragraph_does_not_link() {
    let page = format!(
      "<html><head><style></style></head><body><header><div class=\"meta\">\
       <span class=\"mtag\">understanding · <a href=\"https://host/understanding_home_v2.html\">understanding_home_v2.html</a></span>\
       </div></header>{}</body></html>",
      non_goals_page("<p class=\"non-goals\">Non-goals: none.</p>", "")
    );
    let result = non_goals_line(&page);
    assert!(!result.passed);
    assert_eq!(result.detail, "p.non-goals does not link the understanding page the header's meta chips name");
  }

  #[test]
  fn non_goals_line_passes_when_the_paragraph_links_the_understanding_page_the_chips_name() {
    let page = format!(
      "<html><head><style></style></head><body><header><div class=\"meta\">\
       <span class=\"mtag\">understanding · understanding_home_v2.html</span>\
       </div></header>{}</body></html>",
      non_goals_page(
        "<p class=\"non-goals\">Non-goals: see <a href=\"understanding_home_v2.html\">the understanding page</a>.</p>",
        ""
      )
    );
    assert!(non_goals_line(&page).passed);
  }

  #[test]
  fn non_goals_line_fails_on_an_exclusion_paragraph_outside_the_one_list() {
    for prefix in ["不做", "非目标", "范围外", "Non-goals", "Out of scope"] {
      let page = non_goals_page("<p class=\"non-goals\">Non-goals: none.</p>", &format!("<p>{prefix}：x</p>"));
      let result = non_goals_line(&page);
      assert!(!result.passed, "{prefix}");
      assert_eq!(
        result.detail,
        format!("paragraph in section '2 Context' starts with the exclusion prefix '{prefix}' outside p.non-goals")
      );
    }
  }

  #[test]
  fn non_goals_line_passes_on_one_closing_paragraph() {
    let page = non_goals_page("<p class=\"non-goals\">Non-goals: none.</p>", "<p>Context.</p>");
    let result = non_goals_line(&page);
    assert_eq!(result.name, NON_GOALS_LINE);
    assert!(result.passed, "{result:#?}");
  }

  fn stub_chrome(directory: &Path, height: i32) -> PathBuf {
    chrome_program(
      directory,
      json!([{"stdout": format!("probe output <pre id=\"page-height\">{height}</pre>\n")}, {"exit": 0}]),
    )
  }

  fn outcome_value(outcome: &AssertionOutcome) -> Value {
    serde_json::to_value(outcome).unwrap()
  }

  #[test]
  #[ignore = "integration"]
  fn recorded_cases_match_every_plan_assertion_and_control_difference() {
    let mut decoder =
      flate2::read::GzDecoder::new(include_bytes!("../tests/data/artifact_check_golden.json.gz").as_slice());
    let mut json = String::new();
    decoder.read_to_string(&mut json).expect("the recorded fixture decompresses");
    let fixture: Value = serde_json::from_str(&json).expect("the recorded fixture is valid JSON");
    assert_eq!(fixture["source"], "rust/crates/features/artifacts/tests/check_differential.py");
    assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
    let cases = fixture["cases"].as_array().expect("the fixture holds the plan cases");
    assert_eq!(cases.len(), 23);
    let root = tempfile::tempdir().unwrap();
    let mut recorded_outputs = Vec::with_capacity(cases.len());
    let mut rust_outputs = Vec::with_capacity(cases.len());

    for case in cases {
      let name = case["name"].as_str().expect("each case has a name");
      let directory = root.path().join(name);
      fs::create_dir(&directory).unwrap();
      let page = write_page(&directory, &format!("{name}.html"), case["page"].as_str().expect("each case has a page"));
      let chrome_mode = case["chrome_mode"].as_str().expect("each case names its Chrome mode");
      let chrome = match chrome_mode {
        "system" => {
          let expected: Vec<Value> = serde_json::from_value(case["outcomes"].clone()).unwrap();
          let height = expected
            .iter()
            .find(|outcome| outcome["name"] == PAGE_HEIGHT)
            .and_then(|outcome| outcome["detail"].as_str())
            .and_then(|detail| detail.split_once(" px").map(|(value, _)| value))
            .expect("the recorded page-height detail starts with a pixel count")
            .parse::<i32>()
            .expect("the recorded page height is an integer");
          let chrome = directory.join("recorded-chrome");
          fs::write(&chrome, "").unwrap();
          headless_render::set_test_height(&chrome, height);
          chrome
        }
        "missing" => directory.join("missing-chrome"),
        "failing" => {
          let path = directory.join("chrome-fails.sh");
          write_executable(&path, "#!/bin/sh\necho 'synthetic renderer failure' >&2\nexit 1\n");
          path
        }
        "empty" => PathBuf::from(""),
        other => panic!("unknown recorded Chrome mode {other:?}"),
      };
      let chrome_text = chrome.to_string_lossy().into_owned();
      let outcomes = run_assertions_sync("plan", &page, &chrome_text, &repo_root()).unwrap();
      // The fixture predates the fork, schema-dropped, non-goals-line and head-text assertions, so those outcomes
      // have no recorded counterpart.
      let actual = outcomes
        .iter()
        .filter(|outcome| {
          ![FORK_OPTIONS, FORK_DEMOS, SCHEMA_DROPPED, NON_GOALS_LINE, HEAD_TEXT].contains(&outcome.name.as_str())
        })
        .map(outcome_value)
        .collect::<Vec<_>>();
      let mut expected: Vec<Value> = serde_json::from_value(case["outcomes"].clone()).unwrap();
      for outcome in &mut expected {
        let detail = outcome["detail"].as_str().expect("each outcome has detail");
        outcome["detail"] =
          detail.replace("$TMPROOT", &directory.to_string_lossy()).replace("$CHROME", &chrome_text).into();
      }
      assert_eq!(actual, expected, "recorded Python plan case {name}");
      assert!(
        fs::read_dir(&directory).unwrap().all(|entry| !entry
          .unwrap()
          .file_name()
          .to_string_lossy()
          .starts_with(".page-height-probe-")),
        "page-height probes remain beside {name}"
      );
      if name == "warm-renderer-real-page-height" {
        let height = expected
          .iter()
          .find(|outcome| outcome["name"] == PAGE_HEIGHT)
          .and_then(|outcome| outcome["detail"].as_str())
          .and_then(|detail| detail.split_once(" px").map(|(value, _)| value))
          .expect("the recorded real Chrome height is present")
          .parse::<u32>()
          .expect("the recorded real Chrome height is positive");
        assert!(height > 0);
        assert!(expected.iter().any(|outcome| outcome["name"] == PAGE_HEIGHT && outcome["passed"] == true));
      }
      recorded_outputs.push(expected);
      rust_outputs.push(actual);
    }

    let differences = rust_outputs
      .iter()
      .zip(&recorded_outputs)
      .map(|(actual, expected)| actual.iter().zip(expected).filter(|(left, right)| left != right).count())
      .sum::<usize>();
    assert_eq!(differences, 0);
    let mut control = rust_outputs[2].clone();
    control[0]["detail"] = "intentional control change".into();
    let control_differences = control.iter().zip(&recorded_outputs[2]).filter(|(left, right)| left != right).count();
    assert_eq!(control_differences, 1);
  }

  #[test]
  #[ignore = "integration"]
  fn test_shipped_template_passes_its_own_genre_plan() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = repo_root().join(PLAN_TEMPLATE_PATH);
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert_eq!(outcomes.len(), 17);
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn test_style_verbatim_fails_on_tampered_style() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = write_page(
      directory.path(),
      "plan.html",
      &plan_page("Ship the fix.").replacen("</style>", "body{color:red}\n</style>", 1),
    );
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    let style = by_name(&outcomes, STYLE_VERBATIM);
    assert_eq!(style.len(), 1);
    let style = style[0];
    assert!(!style.passed);
    assert!(style.detail.contains("differs from the genre template prompts/plan_template.html"));
  }

  #[test]
  #[ignore = "integration"]
  fn test_page_height_over_the_target_passes_and_names_the_overage() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let chrome = stub_chrome(directory.path(), 2600);
    let outcomes = run_plan_with_height(&page, chrome.to_str().unwrap(), 2600);
    assert_eq!(
      by_name(&outcomes, PAGE_HEIGHT)[0].detail,
      "2600 px: 600 px over the 2000 px target; fold secondary material"
    );
    assert!(by_name(&outcomes, PAGE_HEIGHT)[0].passed);
  }

  #[test]
  #[ignore = "integration"]
  fn test_page_height_at_or_under_the_target_reports_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let chrome = stub_chrome(directory.path(), 1967);
    let outcomes = run_plan_with_height(&page, chrome.to_str().unwrap(), 1967);
    assert_eq!(by_name(&outcomes, PAGE_HEIGHT)[0].detail, "1967 px (target 2000)");
    assert!(by_name(&outcomes, PAGE_HEIGHT)[0].passed);
  }

  #[test]
  #[ignore = "integration"]
  fn test_page_height_without_a_renderer_reports_without_failing() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let outcomes = run_plan(&page, "");
    assert_eq!(by_name(&outcomes, PAGE_HEIGHT)[0].detail, "not measured: headless_chrome_bin is not configured");
    assert!(by_name(&outcomes, PAGE_HEIGHT)[0].passed);
  }

  #[test]
  #[ignore = "integration"]
  fn test_page_height_with_missing_configured_path_reports_without_failing() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let chrome = directory.path().join("missing-chrome");
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert_eq!(
      by_name(&outcomes, PAGE_HEIGHT)[0].detail,
      format!("not measured: configured headless_chrome_bin path does not exist: {}", chrome.display())
    );
    assert!(by_name(&outcomes, PAGE_HEIGHT)[0].passed);
  }

  #[test]
  #[ignore = "integration"]
  fn test_warm_renderer_measures_real_page_height() {
    let chrome = PathBuf::from(std::env::var_os("CHARLIEBOT_TEST_CHROME_BIN").expect("a headless Chrome path is set"));
    let directory = tempfile::tempdir().unwrap();
    let lines = (0..200).map(|index| format!("<p>line {index}</p>")).collect::<String>();
    let page = write_page(directory.path(), "page.html", &format!("<html><body>{lines}</body></html>"));
    let first = measure_page_height(&chrome, &page);
    let second = measure_page_height(&chrome, &page);
    headless_render::close_renderer().unwrap();
    let first = first.unwrap().unwrap();
    let second = second.unwrap().unwrap();
    assert!(first > 0 && first == second, "the warm render must be deterministic for identical bytes");
  }

  #[test]
  #[ignore = "integration"]
  fn test_plan_present_and_artifact_check_reject_the_same_assertions_on_one_fixture() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let broken = plan_page("Ship the fix.")
      .replace("<span class=\"n\">4</span>", "<span class=\"n\">9</span>")
      .replace("<div class=\"foot\"><p>How to respond.</p></div>", "");
    let page = write_page(directory.path(), "plan.html", &broken);
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    let failures =
      outcomes.iter().filter(|outcome| !outcome.passed).map(|outcome| outcome.name.as_str()).collect::<HashSet<_>>();
    assert_eq!(failures, [SECTIONS_NUMBERED, FOOT_PRESENT].into_iter().collect());
  }

  #[test]
  #[ignore = "integration"]
  fn test_plan_present_accepts_a_page_over_the_height_target() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 2600);
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let outcomes = run_plan_with_height(&page, chrome.to_str().unwrap(), 2600);
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn plan_page_height_probe_is_removed_after_a_renderer_launch_failure() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let chrome = chrome_program(directory.path(), json!([{"stderr": "failure\n"}, {"exit": 1}]));
    let outcomes = run_assertions_sync("plan", &page, chrome.to_str().unwrap(), &repo_root()).unwrap();
    assert!(by_name(&outcomes, PAGE_HEIGHT)[0].passed);
    assert!(by_name(&outcomes, PAGE_HEIGHT)[0].detail.starts_with("not measured:"));
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
  }

  #[test]
  #[ignore = "integration"]
  fn plan_section_numbers_accept_unicode_decimal_digits() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix.").replace(">1</span>", ">١</span>"));
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert!(by_name(&outcomes, SECTIONS_NUMBERED)[0].passed);
  }

  #[test]
  #[ignore = "integration"]
  fn plan_section_number_span_must_hold_digits() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix.").replace(">1</span>", "></span>"));
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert_eq!(by_name(&outcomes, SECTIONS_NUMBERED)[0].detail, "h2 'Problem / Goal' span.n reads '', not an integer");
  }

  #[test]
  #[ignore = "integration"]
  fn ordinal_label_after_a_list_marker_takes_the_colon_gloss() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = write_page(directory.path(), "plan.html", &plan_page("2. Plan 3: ship the fix."));
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert!(by_name(&outcomes, ORDINAL_NAMED)[0].passed);
  }

  #[test]
  #[ignore = "integration"]
  fn test_byte_integrity_fails_on_non_lf_control_byte() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    for (damaged, hex_name) in [('\t', "0x09"), ('\u{000c}', "0x0c"), ('\r', "0x0d")] {
      let source = plan_page(&format!("Before{damaged}After"));
      let page = write_page(directory.path(), "plan.html", &source);
      let outcomes = run_plan(&page, chrome.to_str().unwrap());
      let byte = by_name(&outcomes, BYTE_INTEGRITY)[0];
      assert!(!byte.passed);
      assert!(byte.detail.contains(hex_name));
      assert!(byte.detail.contains("at offset"));
    }
  }

  #[test]
  #[ignore = "integration"]
  fn goal_over_the_former_length_limit_has_no_length_assertion() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = write_page(directory.path(), "plan.html", &plan_page(&"x".repeat(241)));
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert!(by_name(&outcomes, GOAL_SECTION)[0].passed);
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  fn failure_outcomes_serialize_as_one_json_object_each() {
    let value = outcome_value(&outcome("plan", false, "detail"));
    assert_eq!(value["name"], "plan");
    assert_eq!(value["passed"], false);
    assert_eq!(value["detail"], "detail");
  }

  /// The assertion outcomes of only `names` over `source` as a page file, so a fork test needs no Chrome and no
  /// template.
  fn run_only(genre: &str, source: &str, names: &[&str]) -> Vec<AssertionOutcome> {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "page.html", source);
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("sessions")).unwrap();
    run_assertions_sync_with_user_text(
      genre,
      &page,
      "",
      &repo_root(),
      &UserTextChecksConfig::default(),
      home.path(),
      Some(names.iter().map(|name| (*name).to_owned()).collect()),
    )
    .unwrap()
  }

  /// A plan page whose open fork holds `body` between its question and the end of the block.
  fn fork_page(goal: &str, body: &str) -> String {
    plan_page(goal).replacen(
      "<p>Trade-offs</p>",
      &format!("<div class=\"fork\"><p class=\"q\"><span class=\"fn\">1</span> Which one?</p>{body}</div>"),
      1,
    )
  }

  #[test]
  #[ignore = "integration"]
  fn two_named_option_lines_with_a_matching_recommendation_pass() {
    let page = fork_page(
      "Ship the fix.",
      "<ul><li>「keep」: keep the file.</li><li>「delete」: delete the file.</li></ul>\
       <p class=\"rec\">Recommendation: 「keep」.</p><p class=\"trade\">Tradeoff.</p>",
    );
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn one_visible_option_line_fails() {
    let page = fork_page(
      "Ship the fix.",
      "<ul><li>「keep」: keep the file.</li></ul><p class=\"rec\">Recommendation: 「keep」.</p>",
    );
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.contains("has 1 option line outside folds (2 required)"), "{}", outcomes[0].detail);
  }

  #[test]
  #[ignore = "integration"]
  fn option_lines_inside_a_fold_do_not_count() {
    let page = fork_page(
      "Ship the fix.",
      "<details class=\"details-layer\"><summary>More</summary>\
       <ul><li>「keep」: keep the file.</li><li>「delete」: delete the file.</li></ul></details>\
       <p class=\"rec\">Recommendation: 「keep」.</p>",
    );
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.contains("has 0 option line outside folds (2 required)"), "{}", outcomes[0].detail);
  }

  const ONE_VISIBLE_OPTION_AND_ONE_IN_A_PLAIN_DETAILS: &str = "<ul><li>「keep」: keep the file.</li></ul>\
     <details><summary>More</summary><ul><li>「delete」: delete the file.</li></ul></details>\
     <p class=\"rec\">Recommendation: 「keep」.</p>";

  #[test]
  #[ignore = "integration"]
  fn a_plain_closed_details_hides_its_option_from_the_check() {
    let page = fork_page("Ship the fix.", ONE_VISIBLE_OPTION_AND_ONE_IN_A_PLAIN_DETAILS);
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.contains("has 1 option line outside folds (2 required)"), "{}", outcomes[0].detail);
  }

  #[test]
  #[ignore = "integration"]
  fn an_open_details_shows_its_option_to_the_check() {
    let body = ONE_VISIBLE_OPTION_AND_ONE_IN_A_PLAIN_DETAILS.replacen("<details>", "<details open>", 1);
    let page = fork_page("Ship the fix.", &body);
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  /// A plan page whose open fork holds `body` and sits inside a `details` opened with the `details_start` tag.
  fn fork_page_in_details(details_start: &str, body: &str) -> String {
    let fork = format!("<div class=\"fork\"><p class=\"q\"><span class=\"fn\">1</span> Which one?</p>{body}</div>");
    plan_page("Ship the fix.").replacen("<p>Trade-offs</p>", &format!("{details_start}{fork}</details>"), 1)
  }

  const TWO_OPTIONS_AND_RECOMMENDATION: &str = "<ul><li>「keep」: keep the file.</li>\
     <li>「delete」: delete the file.</li></ul><p class=\"rec\">Recommendation: 「keep」.</p>";

  #[test]
  #[ignore = "integration"]
  fn an_open_fork_inside_a_closed_details_fails_with_one_line() {
    let page = fork_page_in_details("<details>", TWO_OPTIONS_AND_RECOMMENDATION);
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert_eq!(outcomes.len(), 1, "{outcomes:#?}");
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.starts_with("fork #1 (section"), "{}", outcomes[0].detail);
    assert!(outcomes[0].detail.ends_with("sits inside a closed fold"), "{}", outcomes[0].detail);
  }

  #[test]
  #[ignore = "integration"]
  fn the_same_fork_inside_an_open_details_passes() {
    let page = fork_page_in_details("<details open>", TWO_OPTIONS_AND_RECOMMENDATION);
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn a_resolved_fork_inside_a_closed_details_is_skipped() {
    let page = fork_page_in_details("<details>", "<p class=\"resolved\">Resolved: keep the file.</p>");
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  /// The plan template's forks are the expected output the browser's parser reads too.
  #[test]
  fn the_plan_template_forks_match_the_shared_fixture() {
    let forks = page_forks(include_str!("../../../../../prompts/plan_template.html")).expect("the template parses");
    let expected: serde_json::Value =
      serde_json::from_str(include_str!("../tests/data/plan_template_forks.json")).expect("the fixture is JSON");
    assert_eq!(serde_json::to_value(&forks).expect("the forks serialize"), expected);
  }

  #[test]
  fn an_option_inside_a_closed_details_is_not_a_pick() {
    let page = "<div class=\"fork\"><p class=\"q\"><span class=\"fn\">1</span> Which one?</p><ul>\
      <li>「keep」: keep the file.</li><li>「drop」: drop the file.</li></ul>\
      <details><ul><li>「hidden」: a folded choice.</li></ul></details></div>";
    let forks = page_forks(page).expect("the page parses");
    let names: Vec<&str> = forks[0].options.iter().map(|option| option.name.as_str()).collect();
    assert_eq!(names, ["keep", "drop"]);
  }

  #[test]
  fn a_pick_option_text_leaves_out_its_demo() {
    let page = "<div class=\"fork\"><p class=\"q\"><span class=\"fn\">1</span> Which one?</p><ul>\
      <li>「keep」: keep the file.<div class=\"demo\"><span class=\"who\">Ann</span></div></li>\
      <li>「drop」: drop the file.</li></ul></div>";
    let forks = page_forks(page).expect("the page parses");
    let texts: Vec<&str> = forks[0].options.iter().map(|option| option.text.as_str()).collect();
    assert_eq!(texts, ["「keep」: keep the file.", "「drop」: drop the file."]);
  }

  #[test]
  #[ignore = "integration"]
  fn an_option_line_that_does_not_open_with_its_name_fails() {
    let page = fork_page(
      "Ship the fix.",
      "<ul><li>keep: keep the file.</li><li>「delete」: delete the file.</li></ul>\
       <p class=\"rec\">Recommendation: 「delete」.</p>",
    );
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.contains("option line 1 does not open with its name in 「」"), "{}", outcomes[0].detail);
  }

  #[test]
  #[ignore = "integration"]
  fn a_recommendation_that_quotes_no_option_name_fails() {
    let page = fork_page(
      "Ship the fix.",
      "<ul><li>「keep」: keep the file.</li><li>「delete」: delete the file.</li></ul>\
       <p class=\"rec\">Recommendation: 「archive」.</p>",
    );
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.contains("p.rec quotes no option name (quoted: 「archive」)"), "{}", outcomes[0].detail);
  }

  #[test]
  #[ignore = "integration"]
  fn yes_or_no_options_pass() {
    let page = fork_page(
      "Ship the fix.",
      "<ul><li>「做」：现在就改。</li><li>「不做」：留着以后改。</li></ul>\
       <p class=\"rec\">推荐：「做」。</p>",
    );
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn prose_names_that_look_like_codes_do_not_break_the_name_match() {
    let page = fork_page(
      "Support 3D, 4K and 7B inputs.",
      "<ul><li>「GPU」: render on the GPU.</li><li>「CPU」: render on the CPU.</li></ul>\
       <p class=\"rec\">Recommendation: 「GPU」.</p>",
    );
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn a_resolved_fork_is_skipped() {
    let page = fork_page("Ship the fix.", "<p class=\"resolved\">Resolved: keep the file.</p>");
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  const DEMO_AND_REASON_OPTIONS: &str = "<ul><li>「keep」: keep the file.\
     <div class=\"demo\"><p>the file stays on the page</p></div></li>\
     <li>「delete」: delete the file.\
     <p class=\"no-demo\">The difference shows in the next page load, which a still picture cannot draw.</p></li></ul>\
     <p class=\"rec\">Recommendation: 「keep」.</p>";

  #[test]
  #[ignore = "integration"]
  fn an_option_with_a_demo_and_one_with_a_reason_pass() {
    let page = fork_page("Ship the fix.", DEMO_AND_REASON_OPTIONS);
    let outcomes = run_only("plan", &page, &[FORK_DEMOS]);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].passed, "{outcomes:#?}");
    assert_eq!(outcomes[0].detail, "");
  }

  #[test]
  #[ignore = "integration"]
  fn an_option_without_a_demo_or_a_reason_fails_and_names_the_option() {
    let body = DEMO_AND_REASON_OPTIONS.replace(
      "<p class=\"no-demo\">The difference shows in the next page load, which a still picture cannot draw.</p>",
      "",
    );
    let page = fork_page("Ship the fix.", &body);
    let outcomes = run_only("plan", &page, &[FORK_DEMOS]);
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].passed);
    assert!(
      outcomes[0].detail.contains("fork #1 (section '5 Trade-offs') option 「delete」: the demo is missing"),
      "{}",
      outcomes[0].detail
    );
  }

  #[test]
  #[ignore = "integration"]
  fn an_empty_no_demo_fails() {
    let body = DEMO_AND_REASON_OPTIONS
      .replace("The difference shows in the next page load, which a still picture cannot draw.", " ");
    let page = fork_page("Ship the fix.", &body);
    let outcomes = run_only("plan", &page, &[FORK_DEMOS]);
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.contains("option 「delete」: the reason is empty"), "{}", outcomes[0].detail);
  }

  #[test]
  #[ignore = "integration"]
  fn a_demo_inside_a_closed_fold_fails() {
    let body = DEMO_AND_REASON_OPTIONS.replace(
      "<div class=\"demo\"><p>the file stays on the page</p></div>",
      "<details><summary>Sample</summary><div class=\"demo\"><p>the file stays on the page</p></div></details>",
    );
    let page = fork_page("Ship the fix.", &body);
    let outcomes = run_only("plan", &page, &[FORK_DEMOS]);
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.contains("option 「keep」: the demo sits in a closed fold"), "{}", outcomes[0].detail);
  }

  #[test]
  #[ignore = "integration"]
  fn an_svg_only_demo_passes() {
    let body = DEMO_AND_REASON_OPTIONS.replace(
      "<div class=\"demo\"><p>the file stays on the page</p></div>",
      "<div class=\"demo\"><svg viewBox=\"0 0 10 10\"><rect x=\"0\" y=\"0\" width=\"5\" height=\"5\"/></svg></div>",
    );
    let page = fork_page("Ship the fix.", &body);
    let outcomes = run_only("plan", &page, &[FORK_DEMOS]);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn a_fork_without_any_marker_on_an_option_line_is_not_checked() {
    let page = fork_page("Ship the fix.", TWO_OPTIONS_AND_RECOMMENDATION);
    let outcomes = run_only("plan", &page, &[FORK_DEMOS]);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn a_demo_in_the_explainer_fold_does_not_start_the_check() {
    let body = format!(
      "{TWO_OPTIONS_AND_RECOMMENDATION}\
       <details class=\"details-layer\"><summary>Why</summary><div class=\"demo\"><p>drawn</p></div></details>"
    );
    let page = fork_page("Ship the fix.", &body);
    let outcomes = run_only("plan", &page, &[FORK_DEMOS]);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn an_li_inside_a_demo_is_not_an_option_line() {
    let page = fork_page(
      "Ship the fix.",
      "<ul><li>「keep」: keep the file.\
       <div class=\"demo\"><ul><li>the file list as it stays</li></ul></div></li>\
       <li>「delete」: delete the file.</li></ul>\
       <p class=\"rec\">Recommendation: 「keep」.</p>",
    );
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  #[test]
  fn a_file_name_without_a_genre_prefix_has_no_genre() {
    assert_eq!(genre_from_file_name("plan_export_interval_v2.html"), Some("plan"));
    assert_eq!(genre_from_file_name("waiting_release.html"), Some("waiting"));
    assert_eq!(genre_from_file_name("linked-page.html"), None);
    assert_eq!(genre_from_file_name("planning.html"), None);
  }

  #[test]
  #[ignore = "integration"]
  fn the_fork_assertion_entry_returns_a_verdict_without_the_user_text_input() {
    let directory = tempfile::tempdir().unwrap();
    let body = "<ul><li>「keep」: keep the file.</li><li>「delete」: delete the file.</li></ul>\
       <p class=\"rec\">Recommendation: 「keep」.</p><p class=\"trade\">Tradeoff.</p>\
       <details class=\"details-layer\"><summary>More</summary><p>More.</p></details>";
    let page = write_page(directory.path(), "plan_page.html", &fork_page("Ship the fix.", body));
    let home = tempfile::tempdir().unwrap();
    // A regular file where the sessions directory belongs makes the user-text checks fail to read their input.
    fs::write(home.path().join("sessions"), b"not a directory").unwrap();
    let outcomes = run_fork_assertions(&page, home.path()).unwrap().expect("the plan_ prefix names a genre");
    assert_eq!(
      outcomes.iter().map(|outcome| outcome.name.as_str()).collect::<Vec<_>>(),
      [FORK_OPEN_SHAPE, FORK_OPTIONS, FORK_DEMOS, FORK_EXPLAINER]
    );
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  /// A waiting page as `charliebot artifact wrap --genre waiting` assembles it: one context sentence and one fork.
  fn waiting_sample() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/waiting_page.html")
  }

  #[test]
  #[ignore = "integration"]
  fn the_waiting_sample_passes_every_waiting_assertion_in_order() {
    let outcomes = run_assertions_sync("waiting", &waiting_sample(), "", &repo_root()).unwrap();
    let names = outcomes.iter().map(|outcome| outcome.name.as_str()).collect::<Vec<_>>();
    assert_eq!(
      names,
      [
        BYTE_INTEGRITY,
        STYLE_VERBATIM,
        HEAD_TEXT,
        FORK_OPEN_SHAPE,
        FORK_OPTIONS,
        FORK_DEMOS,
        FORK_EXPLAINER,
        PAGE_HEIGHT,
        ORDINAL_NAMED,
        TRACKER_ID_NEARBY,
        SESSION_LINKED,
        LINK_RENDERABLE,
      ]
    );
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  fn every_genre_runs_shared_user_text_and_page_height_checks() {
    for genre in ARTIFACT_GENRES {
      let names = assertion_set(genre).expect("every artifact genre has an assertion set");
      assert!(user_text_checks::CHECK_NAMES.iter().all(|check| names.contains(check)), "{genre}: {names:?}");
      assert!(names.contains(&PAGE_HEIGHT), "{genre}: {names:?}");
    }
  }

  #[test]
  #[ignore = "integration"]
  fn each_user_text_hit_fails_with_its_snippet_block_and_fix() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("sessions")).unwrap();
    fs::write(home.path().join("config.yaml"), "user_text_checks:\n  internal_id_patterns: ['zz\\d{3}']\n").unwrap();
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("features.user_text_checks", charliebot_user_text_checks::register).unwrap();
    let config = registry.load_config(home.path()).unwrap();
    let id = Uuid::new_v4().to_string();
    fs::create_dir(home.path().join("sessions").join(&id)).unwrap();
    let chrome = stub_chrome(home.path(), 800);
    let cases = [
      ("tracker-id-nearby", "Internal id zz123.", "zz123", "write the Linear id right next to the internal id"),
      ("session-linked", id.as_str(), id.as_str(), "write the id as"),
      (
        "link-renderable",
        r#"<a href="https://example.invalid/files/export.tsv">the raw table</a>"#,
        "export.tsv",
        "link an HTML page, or the directory that holds the file",
      ),
    ];
    for (index, (check, source, snippet, fix)) in cases.into_iter().enumerate() {
      let page = write_page(home.path(), &format!("user-text-{index}.html"), &plan_page(source));
      let outcomes = run_assertions_sync_with_user_text(
        "plan",
        &page,
        chrome.to_str().unwrap(),
        &repo_root(),
        config.section::<UserTextChecksConfig>(user_text_checks::CONFIG_SECTION),
        home.path(),
        None,
      )
      .unwrap();
      let failures = by_name(&outcomes, check);
      assert_eq!(failures.len(), 1, "{check}: {outcomes:#?}");
      assert!(!failures[0].passed, "{check}: {outcomes:#?}");
      assert!(failures[0].detail.contains(snippet), "{check}: {}", failures[0].detail);
      assert!(failures[0].detail.contains("block"), "{check}: {}", failures[0].detail);
      assert!(failures[0].detail.contains(fix), "{check}: {}", failures[0].detail);
    }
  }

  #[test]
  #[ignore = "integration"]
  fn the_waiting_sample_fails_the_understanding_set_on_sections_and_foot() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    headless_render::set_test_height(&chrome, 800);
    let outcomes =
      run_assertions_sync("understanding", &waiting_sample(), chrome.to_str().unwrap(), &repo_root()).unwrap();
    let failures =
      outcomes.iter().filter(|outcome| !outcome.passed).map(|outcome| outcome.name.as_str()).collect::<HashSet<_>>();
    assert_eq!(failures, [SECTIONS_NUMBERED, FOOT_PRESENT].into_iter().collect());
  }

  /// A page in the genre's template: the `<style>` block verbatim plus `body`.
  fn genre_doc(genre: &str, body: &str) -> String {
    let template = fs::read_to_string(repo_root().join("prompts").join(genre_template(genre))).unwrap();
    let start = template.find("<style>").expect("the template carries a style block");
    let end = template.find("</style>").expect("the style block closes") + "</style>".len();
    format!("<html><head>{}</head><body>{body}</body></html>", &template[start..end])
  }

  /// A sitrep page that passes every sitrep assertion.
  fn sitrep_ok_doc() -> String {
    genre_doc(
      "sitrep",
      concat!(
        r#"<section><h2><span class="n">1</span> What waits on you?</h2><p>Nothing.</p></section>"#,
        r#"<section><h2><span class="n">2</span> What is this and why?</h2>"#,
        r#"<p>Why. <span class="req">r1</span> states the first requirement.</p></section>"#,
        r#"<section><h2><span class="n">3</span> What was verified?</h2>"#,
        r#"<p>Done. <span class="src">s</span></p></section>"#,
        r#"<section><h2><span class="n">4</span> Risks</h2>"#,
        r#"<p><span class="tag fact">Fact</span> The reading holds. <span class="src">s</span></p></section>"#,
        r#"<section><h2><span class="n">5</span> What happens next?</h2><p>Next.</p></section>"#,
      ),
    )
  }

  /// The outcomes of `req-chips` on a sitrep page, run alone so no Chrome render runs.
  fn run_sitrep(source: &str) -> Vec<AssertionOutcome> {
    run_only("sitrep", source, &[REQ_CHIPS])
  }

  #[test]
  fn test_sitrep_with_a_decimal_table_and_no_changed_factor_block_passes_every_sitrep_assertion() {
    let table = "<table><tr><td>10</td><td>0.002378</td></tr></table>";
    let doc = sitrep_ok_doc().replace(
      r#"<p>Done. <span class="src">s</span></p>"#,
      &format!("{table}<p>Done. <span class=\"src\">s</span></p>"),
    );
    let outcomes = run_only("sitrep", &doc, assertion_set("sitrep").unwrap());
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  fn test_req_chips_passes_when_the_chip_is_glossed_in_its_sentence() {
    let outcomes = run_sitrep(&sitrep_ok_doc());
    let chips = by_name(&outcomes, REQ_CHIPS);
    assert_eq!(chips.len(), 1, "{outcomes:#?}");
    assert!(chips[0].passed, "{:?}", chips[0].detail);
  }

  #[test]
  fn test_req_chips_fails_when_the_sentence_ends_after_the_chip() {
    let doc = sitrep_ok_doc().replace(
      r#"<span class="req">r1</span> states the first requirement."#,
      r#"<span class="req">r1</span>. A later sentence names the requirement."#,
    );
    let outcomes = run_sitrep(&doc);
    let chips = by_name(&outcomes, REQ_CHIPS);
    assert_eq!(chips.len(), 1, "{outcomes:#?}");
    assert!(!chips[0].passed);
    assert!(chips[0].detail.contains("'r1'"), "{}", chips[0].detail);
    assert!(chips[0].detail.contains("no gloss"), "{}", chips[0].detail);
  }

  #[test]
  fn test_req_chips_passes_a_sitrep_without_chips() {
    let doc = sitrep_ok_doc()
      .replace(r#"<span class="req">r1</span> states the first requirement."#, "States the first requirement.");
    let outcomes = run_sitrep(&doc);
    let chips = by_name(&outcomes, REQ_CHIPS);
    assert_eq!(chips.len(), 1, "{outcomes:#?}");
    assert!(chips[0].passed, "{:?}", chips[0].detail);
  }

  #[test]
  fn test_req_chips_accepts_a_gloss_in_another_unicode_script() {
    let doc = sitrep_ok_doc().replace(
      r#"<span class="req">r1</span> states the first requirement."#,
      r#"<span class="req">r1</span> 要件の説明です。"#,
    );
    let outcomes = run_sitrep(&doc);
    let chips = by_name(&outcomes, REQ_CHIPS);
    assert_eq!(chips.len(), 1, "{outcomes:#?}");
    assert!(chips[0].passed, "{:?}", chips[0].detail);
  }

  /// Sections numbered `numbers` in order, each with a heading and one paragraph.
  fn numbered_sections(numbers: &[usize]) -> String {
    numbers
      .iter()
      .map(|number| {
        format!(r#"<section><h2><span class="n">{number}</span> Section {number}</h2><p>Text.</p></section>"#)
      })
      .collect()
  }

  #[test]
  fn test_sections_numbered_accepts_a_plan_of_five_or_six_sections() {
    for count in [5, 6] {
      let numbers = (1..=count).collect::<Vec<_>>();
      let outcomes = run_only("plan", &genre_doc("plan", &numbered_sections(&numbers)), &[SECTIONS_NUMBERED]);
      assert!(outcomes[0].passed, "{count} sections: {outcomes:#?}");
    }
  }

  #[test]
  fn test_sections_numbered_fails_a_plan_of_four_or_seven_sections() {
    for count in [4, 7] {
      let numbers = (1..=count).collect::<Vec<_>>();
      let outcomes = run_only("plan", &genre_doc("plan", &numbered_sections(&numbers)), &[SECTIONS_NUMBERED]);
      let failures = failed(&outcomes, SECTIONS_NUMBERED);
      assert_eq!(failures.len(), 1, "{count} sections: {outcomes:#?}");
      assert_eq!(failures[0].detail, format!("plan requires 5 to 6 numbered sections, found {count}"));
    }
  }

  #[test]
  fn test_sections_numbered_accepts_any_subset_of_the_explain_sections_and_none() {
    for numbers in [vec![], vec![2, 5], vec![1, 3, 4], vec![1, 2, 3, 4, 5]] {
      let outcomes = run_only("explain", &explain_doc(&numbered_sections(&numbers)), &[SECTIONS_NUMBERED]);
      assert!(outcomes[0].passed, "{numbers:?}: {outcomes:#?}");
    }
  }

  #[test]
  fn test_sections_numbered_fails_explain_numbers_out_of_order_or_outside_one_to_five() {
    for numbers in [vec![3, 2], vec![2, 2], vec![1, 6], vec![0]] {
      let outcomes = run_only("explain", &explain_doc(&numbered_sections(&numbers)), &[SECTIONS_NUMBERED]);
      let failures = failed(&outcomes, SECTIONS_NUMBERED);
      assert_eq!(failures.len(), 1, "{numbers:?}: {outcomes:#?}");
      assert!(failures[0].detail.ends_with("expected an ascending subset of 1..=5"), "{}", failures[0].detail);
    }
  }

  /// The explain template with `body` as its `<main>` content. The head, the render path and the style block stay the
  /// template's.
  fn explain_doc(body: &str) -> String {
    let template = fs::read_to_string(repo_root().join("prompts/explain_template.html")).unwrap();
    let start = template.find("<main>").unwrap() + "<main>".len();
    let end = template.find("</main>").unwrap();
    format!("{}{body}{}", &template[..start], &template[end..])
  }

  fn explain_section(number: usize, title: &str, content: &str) -> String {
    format!(r#"<section class="plan-section"><h2><span class="n">{number}</span> {title}</h2>{content}</section>"#)
  }

  /// An explain body that passes every explain assertion, with `mechanism` as section 2 and `examples` as section 3.
  fn explain_body(mechanism: &str, examples: &str) -> String {
    [
      concat!(
        r#"<section class="plan-section"><div class="triad">"#,
        r#"<div class="row"><span class="k">You know</span><div>Sixteen bins cap the variants.</div></div>"#,
        r#"<div class="row"><span class="k">You observe</span><div>The ramp took 47 minutes.</div></div>"#,
        r#"<div class="row"><span class="k">Missing link</span><div>The frame guard keys on raw values.</div></div>"#,
        r#"</div></section>"#,
      )
      .to_owned(),
      explain_section(1, "Where the model is right", "<p>Binning caps the shader cache.</p>"),
      explain_section(2, "The mechanism", mechanism),
      explain_section(3, "Worked example", examples),
      explain_section(4, "Reconciling the count", "<p>Recount.</p>"),
      explain_section(5, "What stays open", "<p>Nothing.</p>"),
      r#"<div class="foot"><p>Closing.</p></div>"#.to_owned(),
    ]
    .concat()
  }

  /// An svg that passes `explain-figures`: three shape or text elements.
  const FIGURE: &str = concat!(
    r#"<svg viewBox="0 0 100 40"><rect x="0" y="0" width="30" height="30"/>"#,
    r#"<line x1="30" y1="15" x2="60" y2="15"/><text x="62" y="20">guard</text></svg>"#,
  );
  /// The one paragraph that may sit before the first step.
  const OPENING: &str = "<p>The frame cache keys on raw values.</p>";
  const E2E: &str = concat!(
    r#"<div class="example e2e"><h3>Frame 41 to 42</h3><p>Frame 42 recompiles.</p>"#,
    r#"<p><span class="src">ramp log</span></p></div>"#,
  );
  const CONTRAST: &str = concat!(
    r#"<div class="example contrast"><h3>Same bin, no recompile</h3><p>Frame 42 holds.</p>"#,
    r#"<p><span class="src">ramp log</span></p></div>"#,
  );

  /// A `div.step` with an h3, `figure` and one paragraph.
  fn step(figure: &str) -> String {
    format!(
      r#"<div class="step"><h3>The guard records a raw value</h3>{figure}<p>The next call re-checks it.</p></div>"#
    )
  }

  fn run_explain(body: &str) -> Vec<AssertionOutcome> {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "explain.html", &explain_doc(body));
    run_assertions_sync("explain", &page, "", &repo_root()).unwrap()
  }

  /// The failed outcomes of one assertion.
  fn failed<'a>(outcomes: &'a [AssertionOutcome], name: &str) -> Vec<&'a AssertionOutcome> {
    by_name(outcomes, name).into_iter().filter(|outcome| !outcome.passed).collect()
  }

  #[test]
  #[ignore = "integration"]
  fn test_shipped_explain_template_passes_its_explain_set() {
    let template = repo_root().join("prompts/explain_template.html");
    let outcomes = run_assertions_sync("explain", &template, "", &repo_root()).unwrap();
    assert_eq!(outcomes.len(), 16, "{outcomes:#?}");
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  #[ignore = "integration"]
  fn test_explain_page_with_figured_steps_and_both_examples_passes_every_assertion() {
    let mechanism = format!("{OPENING}{}{}", step(FIGURE), step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, &format!("{E2E}{CONTRAST}")));
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  fn test_explain_figures_passes_a_page_without_steps() {
    let outcomes = run_only("explain", &explain_doc(OPENING), &[EXPLAIN_FIGURES]);
    assert!(failed(&outcomes, EXPLAIN_FIGURES).is_empty(), "{outcomes:#?}");
  }

  #[test]
  fn test_explain_examples_passes_a_page_without_steps_or_examples() {
    let outcomes = run_only("explain", &explain_doc(OPENING), &[EXPLAIN_EXAMPLES]);
    assert!(failed(&outcomes, EXPLAIN_EXAMPLES).is_empty(), "{outcomes:#?}");
  }

  #[test]
  fn test_explain_examples_still_fails_a_page_with_steps_and_no_examples() {
    let mechanism = format!("{OPENING}{}", step(FIGURE));
    let outcomes = run_only("explain", &explain_doc(&explain_body(&mechanism, "")), &[EXPLAIN_EXAMPLES]);
    let failures = failed(&outcomes, EXPLAIN_EXAMPLES);
    assert_eq!(failures.len(), 2, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "no div.example.e2e on the page");
  }

  #[test]
  #[ignore = "integration"]
  fn test_explain_figures_fails_a_step_with_only_a_flow_strip() {
    let flow_only = concat!(
      r#"<div class="step"><h3>Flow only</h3>"#,
      r#"<div class="flow"><span class="b">packer</span><span class="ar">→</span><span class="b">frame</span></div>"#,
      r#"<p>Text.</p></div>"#,
    );
    let mechanism = format!("{OPENING}{flow_only}{}", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, &format!("{E2E}{CONTRAST}")));
    let failures = failed(&outcomes, EXPLAIN_FIGURES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "step #1 (section '2 The mechanism') has no svg or img figure");
  }

  #[test]
  #[ignore = "integration"]
  fn test_explain_figures_fails_an_svg_with_fewer_than_three_elements() {
    let flow = r#"<div class="flow"><span class="b">a</span><span class="b">b</span><span class="b">c</span></div>"#;
    let sparse_svgs = [
      r#"<svg viewBox="0 0 10 10"></svg>"#,
      r#"<svg viewBox="0 0 10 10"><rect x="0" y="0" width="5" height="5"/><text x="6" y="6">a</text></svg>"#,
    ];
    for svg in sparse_svgs {
      let sparse_step = format!(r#"<div class="step"><h3>Sparse</h3>{flow}{svg}<p>Text.</p></div>"#);
      let mechanism = format!("{OPENING}{sparse_step}{}", step(FIGURE));
      let outcomes = run_explain(&explain_body(&mechanism, &format!("{E2E}{CONTRAST}")));
      let failures = failed(&outcomes, EXPLAIN_FIGURES);
      assert_eq!(failures.len(), 1, "{svg}: {outcomes:#?}");
      assert_eq!(
        failures[0].detail,
        "step #1 (section '2 The mechanism') has an svg with fewer than 3 shape or text elements"
      );
    }
  }

  #[test]
  #[ignore = "integration"]
  fn test_explain_figures_fails_an_img_without_src() {
    for img in [r#"<img alt="frame"/>"#, r#"<img src="" alt="frame"/>"#] {
      let mechanism = format!("{OPENING}{}", step(img));
      let outcomes = run_explain(&explain_body(&mechanism, &format!("{E2E}{CONTRAST}")));
      let failures = failed(&outcomes, EXPLAIN_FIGURES);
      assert_eq!(failures.len(), 1, "{img}: {outcomes:#?}");
      assert_eq!(failures[0].detail, "step #1 (section '2 The mechanism') has an img without src");
    }
  }

  #[test]
  #[ignore = "integration"]
  fn test_explain_figures_fails_a_paragraph_after_the_steps() {
    let mechanism = format!("{OPENING}{}<p>A stray paragraph.</p>", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, &format!("{E2E}{CONTRAST}")));
    let failures = failed(&outcomes, EXPLAIN_FIGURES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "section '2 The mechanism' has 1 p blocks outside div.step after its opening p");
  }

  #[test]
  #[ignore = "integration"]
  fn test_explain_figures_fails_a_second_paragraph_before_the_first_step() {
    let mechanism = format!("{OPENING}<p>A second opening.</p>{}", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, &format!("{E2E}{CONTRAST}")));
    let failures = failed(&outcomes, EXPLAIN_FIGURES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "section '2 The mechanism' has 1 p blocks outside div.step after its opening p");
  }

  #[test]
  #[ignore = "integration"]
  fn test_explain_examples_fails_a_page_with_no_e2e_example() {
    let mechanism = format!("{OPENING}{}", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, CONTRAST));
    let failures = failed(&outcomes, EXPLAIN_EXAMPLES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "no div.example.e2e on the page");
  }

  #[test]
  #[ignore = "integration"]
  fn test_explain_examples_fails_a_page_with_no_contrast_example() {
    let mechanism = format!("{OPENING}{}", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, E2E));
    let failures = failed(&outcomes, EXPLAIN_EXAMPLES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "no div.example.contrast on the page");
  }

  #[test]
  #[ignore = "integration"]
  fn test_explain_examples_fails_an_example_without_a_source() {
    let unsourced = r#"<div class="example e2e"><h3>Frame 41 to 42</h3><p>Frame 42 recompiles.</p></div>"#;
    let mechanism = format!("{OPENING}{}", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, &format!("{unsourced}{CONTRAST}")));
    let failures = failed(&outcomes, EXPLAIN_EXAMPLES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "example #1 (section '3 Worked example') has no span.src");
  }
}

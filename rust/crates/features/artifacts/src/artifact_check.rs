use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context as AnyhowContext, anyhow, bail};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::pyfmt;
use charliebot_core::pyregex::compile_python_regex;
use charliebot_core::pystr;
use charliebot_pyhtml::{EventKind, folds_at_open, parse_with, unescape};
use charliebot_user_text_checks::{self as user_text_checks, UserTextChecksConfig};
use fancy_regex::Regex;
use serde::Serialize;
use url::Url;
use uuid::Uuid;

use crate::headless_render;

const STYLE_VERBATIM: &str = "style-verbatim";
const SECTIONS_NUMBERED: &str = "sections-numbered";
const FOOT_PRESENT: &str = "foot-present";
const EXPLAIN_TRIAD: &str = "explain-triad";
const EXPLAIN_FIGURES: &str = "explain-figures";
const EXPLAIN_EXAMPLES: &str = "explain-examples";
const FORK_OPEN_SHAPE: &str = "fork-open-shape";
const FORK_OPTIONS: &str = "fork-options";
const FORK_EXPLAINER: &str = "fork-explainer";
const FACT_ANCHORED: &str = "fact-anchored";
const REQ_CHIPS: &str = "req-chips";
const RUN_STATED: &str = "run-stated";
const RENDER_PATH: &str = "render-path";
const GOAL_BUDGET: &str = "goal-budget";
const PAGE_HEIGHT: &str = "page-height";
const ORDINAL_NAMED: &str = "ordinal-named";
const BYTE_INTEGRITY: &str = "byte-integrity";
const TRACKER_ID_NEARBY: &str = user_text_checks::TRACKER_ID_NEARBY;
const SESSION_LINKED: &str = user_text_checks::SESSION_LINKED;
const LINK_RENDERABLE: &str = user_text_checks::LINK_RENDERABLE;

/// The registered genres in the order the parser states them. Python's `constants.ARTIFACT_GENRES` lists the first
/// five; `waiting` exists only here.
pub const ARTIFACT_GENRES: [&str; 6] = ["plan", "understanding", "sitrep", "debug", "explain", "waiting"];

const GOAL_WEIGHTED_BUDGET: usize = 240;
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

/// `_FACT_ANCESTOR_TAGS`: a fact label's block, which must also carry a `span.src`.
const FACT_ANCESTOR_TAGS: &[&str] = &["p", "li", "td", "blockquote", "h3"];
/// `_REQ_BLOCK_TAGS`: the blocks that can hold a requirement chip's gloss.
const REQ_BLOCK_TAGS: &[&str] = &["p", "li", "td", "th", "dt", "dd", "blockquote", "h1", "h2", "h3", "h4"];
/// `_REQ_START` / `_REQ_END`: the sentinels that fence a requirement chip inside its block's text.
const REQ_START: char = '\u{0003}';
const REQ_END: char = '\u{0004}';
/// `_SENTENCE_END_RE`: the terminators that end the sentence holding a requirement chip.
const SENTENCE_END: [char; 5] = ['.', '。', '!', '?', '！'];
/// `_KATEX_*_HINT`: the math render path's resource names.
const KATEX_CSS_HINT: &str = "katex.min.css";
const KATEX_JS_HINT: &str = "katex.min.js";
const KATEX_AUTO_RENDER_HINT: &str = "auto-render.min.js";

static RUN_DECIMAL_RE: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(r"\d+\.\d+"));
static RUN_HELD_RE: LazyLock<Regex> = LazyLock::new(|| {
  compile_python_regex(
    r"(?i)\b(?:held|kept|stayed|stays|remained|remains)\s+(?:the\s+)?(?:fixed|constant|unchanged|same)\b|\b(?:fixed|constant|unchanged|retained|controlled|same)\b|保持不变|固定不变|保持一致|不变|未变|没变|未改动|没改动|未改|没改|恒定|固定|控制变量",
  )
});
static RUN_CHANGED_RE: LazyLock<Regex> = LazyLock::new(|| {
  compile_python_regex(
    r"(?i)\b(?:changed?|changing|changes?|varied|varying|modified|altered|variable|tweaked)\b|改(?:动|变|了|成|为)|更改|修改|变更|变量|变化",
  )
});

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

/// `_SECTION_COUNT_RULES`: the numbered-h2 count per genre, and whether it is exact.
fn section_count_rule(genre: &str) -> (usize, bool) {
  match genre {
    "plan" => (6, true),
    "understanding" => (5, false),
    "sitrep" | "debug" | "explain" => (5, true),
    "waiting" => unreachable!("the waiting set omits sections-numbered, so it has no section-count rule"),
    other => unreachable!("genre {other:?} is registered, so it has a section-count rule"),
  }
}

/// `_ASSERTION_SETS`: the assertions of one genre, in the order their outcomes print.
fn assertion_set(genre: &str) -> Option<&'static [&'static str]> {
  match genre {
    "plan" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      SECTIONS_NUMBERED,
      FOOT_PRESENT,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FORK_EXPLAINER,
      GOAL_BUDGET,
      PAGE_HEIGHT,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    "understanding" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      SECTIONS_NUMBERED,
      FOOT_PRESENT,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
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
      SECTIONS_NUMBERED,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FORK_EXPLAINER,
      FACT_ANCHORED,
      REQ_CHIPS,
      RUN_STATED,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    "debug" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      SECTIONS_NUMBERED,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FACT_ANCHORED,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    "explain" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      RENDER_PATH,
      SECTIONS_NUMBERED,
      EXPLAIN_TRIAD,
      EXPLAIN_FIGURES,
      EXPLAIN_EXAMPLES,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      ORDINAL_NAMED,
      TRACKER_ID_NEARBY,
      SESSION_LINKED,
      LINK_RENDERABLE,
    ]),
    "waiting" => Some(&[
      BYTE_INTEGRITY,
      STYLE_VERBATIM,
      FORK_OPEN_SHAPE,
      FORK_OPTIONS,
      FORK_EXPLAINER,
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
  const FORK: [&str; 3] = [FORK_OPEN_SHAPE, FORK_OPTIONS, FORK_EXPLAINER];
  assertion_set(genre)
    .map(|names| names.iter().copied().filter(|name| FORK.contains(name)).collect())
    .unwrap_or_default()
}

/// Run the fork assertions of the genre that `page`'s file name names, or `None` when the name carries no genre
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

static GOAL_SECTION_RE: LazyLock<Regex> =
  LazyLock::new(|| compile_python_regex(r"(?s)Problem / Goal</h2>(.*?)</section>"));
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(r"<[^>]+>"));
static ORD: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(ORDINAL_PATTERN));
static INTERNAL_UNLESS_QUALIFIED: LazyLock<Regex> =
  LazyLock::new(|| compile_python_regex(INTERNAL_UNLESS_QUALIFIED_PATTERN));
static QUAL: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(QUAL_PATTERN));
static ANCHOR: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(ANCHOR_PATTERN));
static SENTENCE_SPLIT: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(SENTENCE_SPLIT_PATTERN));
static QUOTE: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(QUOTE_PATTERN));
static GLOSS: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(GLOSS_PATTERN));
static COLON_GLOSS: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(COLON_GLOSS_PATTERN));
static TITLE_ANCHOR: LazyLock<Regex> = LazyLock::new(|| compile_python_regex(TITLE_ANCHOR_PATTERN));

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
  /// Every attribute's raw value; a bare attribute is `None`, as Python's parser reports it.
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

/// Run one genre's assertion set on a blocking thread. `only` narrows the run to the named assertions; `None` runs
/// the whole set.
pub async fn run_assertions(
  genre: &str,
  artifact: PathBuf,
  config: &CharlieBotConfig,
  repo_root: PathBuf,
  only: Option<&[&str]>,
) -> anyhow::Result<Vec<AssertionOutcome>> {
  run_assertions_with_chrome(
    genre,
    artifact,
    config.headless_chrome_bin.clone(),
    repo_root,
    config.section::<UserTextChecksConfig>("user_text_checks").clone(),
    config.charliebot_home.clone(),
    only,
  )
  .await
}

/// Run one genre's assertion set with an explicit Chrome path on a blocking thread.
pub async fn run_assertions_with_chrome(
  genre: &str,
  artifact: PathBuf,
  chrome_bin: String,
  repo_root: PathBuf,
  checks_config: UserTextChecksConfig,
  home: PathBuf,
  only: Option<&[&str]>,
) -> anyhow::Result<Vec<AssertionOutcome>> {
  let genre = genre.to_owned();
  let only = only.map(|names| names.iter().map(|name| (*name).to_owned()).collect::<Vec<String>>());
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
      pyfmt::str_repr(genre)
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
      SECTIONS_NUMBERED => check_sections_numbered,
      FOOT_PRESENT => check_foot_present,
      EXPLAIN_TRIAD => check_explain_triad,
      EXPLAIN_FIGURES => check_explain_figures,
      EXPLAIN_EXAMPLES => check_explain_examples,
      FORK_OPEN_SHAPE => check_fork_open_shape,
      FORK_OPTIONS => check_fork_options,
      FORK_EXPLAINER => check_fork_explainer,
      FACT_ANCHORED => check_fact_anchored,
      REQ_CHIPS => check_req_chips,
      RUN_STATED => check_run_stated,
      RENDER_PATH => check_render_path,
      GOAL_BUDGET => check_goal_budget,
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
      let classes = pystr::split_whitespace(class_value).into_iter().map(str::to_owned).collect();
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
    .map(|&heading| pystr::collapse_whitespace(&element_text(nodes, heading)))
    .unwrap_or_else(|| "<unnamed section>".to_owned())
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
  let page_text = pystr::collapse_whitespace(&element_text(&context.nodes, page_styles[0]));
  let template_text = pystr::collapse_whitespace(&element_text(&template_nodes, template_styles[0]));
  if page_text == template_text {
    Ok(vec![outcome(STYLE_VERBATIM, true, "")])
  } else {
    Ok(vec![outcome(STYLE_VERBATIM, false, format!("<style> block differs from the genre template {template_rel}"))])
  }
}

fn check_sections_numbered(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut numbered = Vec::new();
  for heading in find(&context.nodes, 0, "h2", &[]) {
    let numbers = find(&context.nodes, heading, "span", &["n"]);
    if numbers.len() > 1 {
      let heading_text = pystr::collapse_whitespace(&element_text(&context.nodes, heading));
      return Ok(vec![outcome(
        SECTIONS_NUMBERED,
        false,
        format!("h2 {} carries {} span.n numbers", pyfmt::str_repr(&heading_text), numbers.len()),
      )]);
    }
    if let Some(&number) = numbers.first() {
      numbered.push((
        pystr::collapse_whitespace(&element_text(&context.nodes, heading)),
        pystr::collapse_whitespace(&element_text(&context.nodes, number)),
      ));
    }
  }
  let mut values = Vec::new();
  for (heading, number) in numbered {
    if !pystr::isdigit(&number) {
      return Ok(vec![outcome(
        SECTIONS_NUMBERED,
        false,
        format!("h2 {} span.n reads {}, not an integer", pyfmt::str_repr(&heading), pyfmt::str_repr(&number)),
      )]);
    }
    values.push(python_decimal_integer(&number).map_err(|error| anyhow!("{error}"))?);
  }
  let expected = (1..=values.len()).map(|value| value.to_string()).collect::<Vec<_>>();
  if values != expected {
    let read = if values.is_empty() { "(none)".to_owned() } else { values.join(", ") };
    return Ok(vec![outcome(
      SECTIONS_NUMBERED,
      false,
      format!("section numbers read {read}, expected 1..{} contiguously", values.len()),
    )]);
  }
  let (required, exact) = section_count_rule(context.genre);
  if exact && values.len() != required {
    return Ok(vec![outcome(
      SECTIONS_NUMBERED,
      false,
      format!("{} requires exactly {required} numbered sections, found {}", context.genre, values.len()),
    )]);
  }
  if !exact && values.len() < required {
    return Ok(vec![outcome(
      SECTIONS_NUMBERED,
      false,
      format!("{} requires at least {required} numbered sections, found {}", context.genre, values.len()),
    )]);
  }
  Ok(vec![outcome(SECTIONS_NUMBERED, true, "")])
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
          pyfmt::str_repr(&section_heading(&context.nodes, fork)),
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

/// The `li` lines of one fork that no folded `details` holds: the options the page shows at open.
fn visible_option_lines(nodes: &[Element], fork: usize) -> Vec<usize> {
  find(nodes, fork, "li", &[])
    .into_iter()
    .filter(|&line| {
      let mut ancestor = nodes[line].parent;
      while let Some(index) = ancestor {
        if index == fork {
          return true;
        }
        let node = &nodes[index];
        if folds_at_open(&node.tag, node.attrs.keys().map(String::as_str)) {
          return false;
        }
        ancestor = node.parent;
      }
      true
    })
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

fn check_fork_options(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut failures = Vec::new();
  for (index, fork) in open_forks(&context.nodes) {
    let section = pyfmt::str_repr(&section_heading(&context.nodes, fork));
    let lines = visible_option_lines(&context.nodes, fork);
    let mut names = Vec::new();
    for (position, &line) in lines.iter().enumerate() {
      let text = pystr::collapse_whitespace(&element_text(&context.nodes, line));
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
      let text = pystr::collapse_whitespace(&element_text(&context.nodes, recommendation));
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
          pyfmt::str_repr(&section_heading(&context.nodes, fork))
        ),
      ));
    } else {
      let has_body = descendants(&context.nodes, layers[0]).into_iter().any(|element| {
        EXPLAINER_BODY_TAGS.contains(&context.nodes[element].tag.as_str())
          && !pystr::split_whitespace(&element_text(&context.nodes, element)).is_empty()
      });
      if !has_body {
        failures.push(outcome(
          FORK_EXPLAINER,
          false,
          format!(
            "fork #{index} (section {}) explainer has no body",
            pyfmt::str_repr(&section_heading(&context.nodes, fork))
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

fn check_goal_budget(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let source = fs::read_to_string(context.artifact)
    .with_context(|| format!("could not read artifact {}", context.artifact.display()))?;
  let Some(captures) = GOAL_SECTION_RE.captures(&source)? else {
    return Ok(vec![outcome(
      GOAL_BUDGET,
      false,
      format!(
        "artifact {} has no 'Problem / Goal' section",
        pyfmt::str_repr(&context.artifact.file_name().expect("an artifact path has a name").to_string_lossy())
      ),
    )]);
  };
  let body = captures.get(1).expect("the goal expression has one capture").as_str();
  let body = revision_mark_regex("div", "revnote").replace_all(body, "");
  let body = revision_mark_regex("span", "revbadge").replace_all(&body, "");
  let body = TAG_RE.replace_all(&body, "");
  let decoded = unescape(&body);
  let text = pystr::collapse_whitespace(&decoded);
  let weighted = text.chars().count()
    + text
      .chars()
      .filter(|character| matches!(*character as u32, 0x3000..=0x303f | 0x4e00..=0x9fff | 0xff00..=0xffef))
      .count();
  if weighted > GOAL_WEIGHTED_BUDGET {
    Ok(vec![outcome(
      GOAL_BUDGET,
      false,
      format!(
        "plan goal is {weighted} weighted chars (budget {GOAL_WEIGHTED_BUDGET}): keep the goal and non-goals; demote diagnosis, thresholds, paths, and justifications to Context or 4.1"
      ),
    )])
  } else {
    Ok(vec![outcome(GOAL_BUDGET, true, format!("{weighted} weighted chars (budget {GOAL_WEIGHTED_BUDGET})"))])
  }
}

fn revision_mark_regex(tag: &str, class: &str) -> Regex {
  compile_python_regex(&format!(
    r#"(?s)<{tag}\b(?=[^>]*(?<!\S)class\s*=\s*(?:(?:"(?:[^"]*\s)?{class}(?:\s[^"]*)?")|(?:'(?:[^']*\s)?{class}(?:\s[^']*)?')|{class}(?:\s|>)))[^>]*>.*?</{tag}>"#
  ))
}

fn check_page_height(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  if context.chrome_bin.is_empty() {
    return Ok(vec![outcome(
      PAGE_HEIGHT,
      false,
      "headless_chrome_bin is required for plan registration: set it in the host config.yaml to the absolute path of a headless-chromium-compatible binary",
    )]);
  }
  let chrome_bin = PathBuf::from(context.chrome_bin);
  if !chrome_bin.exists() {
    return Ok(vec![outcome(
      PAGE_HEIGHT,
      false,
      format!(
        "headless_chrome_bin is required for plan registration but the configured path does not exist: {}",
        chrome_bin.display()
      ),
    )]);
  }
  let height = match measure_page_height(&chrome_bin, context.artifact)? {
    Ok(height) => height,
    Err(message) => return Ok(vec![outcome(PAGE_HEIGHT, false, message)]),
  };
  if height > PAGE_HEIGHT_TARGET {
    Ok(vec![outcome(
      PAGE_HEIGHT,
      true,
      format!("{height} px: {} px over the {PAGE_HEIGHT_TARGET} px target", height - PAGE_HEIGHT_TARGET),
    )])
  } else {
    Ok(vec![outcome(PAGE_HEIGHT, true, format!("{height} px (target {PAGE_HEIGHT_TARGET})"))])
  }
}

/// The inner error is a renderer failure, which Python reports as a failed assertion. Any other error,
/// such as a probe that cannot be written, propagates as Python's `OSError` does.
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

fn python_decimal_integer(text: &str) -> Result<String, String> {
  let mut digits = String::with_capacity(text.len());
  for character in text.chars() {
    let Some(digit) = pystr::python_decimal_value(character) else {
      return Err(format!("invalid literal for int() with base 10: {}", pyfmt::str_repr(text)));
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
          pyfmt::str_repr(&section_heading(&context.nodes, block)),
          pyfmt::str_repr(&joined)
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
    .map(|element| pystr::collapse_whitespace(&element_text(&context.nodes, element)))
    .collect::<HashSet<_>>();
  let mut named = HashSet::new();
  let mut flagged = Vec::new();
  for block in ordinal_blocks(&context.nodes) {
    let block_text = block_scan_text(&context.nodes, block);
    let collapsed = pystr::collapse_whitespace(&block_text);
    let text = unescape(&collapsed);
    for sentence in SENTENCE_SPLIT.split(&text) {
      let sentence = pystr::strip(sentence?);
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
          && pystr::strip(&before[..opening.unwrap()]).chars().count() >= 2
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
  pystr::collapse_whitespace(kind)
}

fn ordinal_number(number: &str) -> anyhow::Result<Option<usize>> {
  if pystr::isdigit(number) {
    return Ok(python_decimal_integer(number).ok().and_then(|value| value.parse().ok()));
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

/// `_presence_check`: the page carries a `tag` with these classes.
fn check_explain_triad(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  if find(&context.nodes, 0, "div", &["triad"]).is_empty() {
    Ok(vec![outcome(EXPLAIN_TRIAD, false, "no div.triad on the page")])
  } else {
    Ok(vec![outcome(EXPLAIN_TRIAD, true, "")])
  }
}

/// `explain-figures`: each `div.step` holds an svg of at least `FIGURE_MIN_ELEMENTS` shape or text elements, or an img
/// with a src. In a section that holds steps, every p sits in a step, apart from one opening p before the first step.
fn check_explain_figures(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let steps = find(&context.nodes, 0, "div", &["step"]);
  if steps.is_empty() {
    return Ok(vec![outcome(EXPLAIN_FIGURES, false, "no div.step on the page")]);
  }
  let mut failures = Vec::new();
  for (index, &step) in steps.iter().enumerate() {
    let index = index + 1;
    let heading = pyfmt::str_repr(&section_heading(&context.nodes, step));
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
    // The one p before the first step is the opening paragraph; every other p outside the steps is a failure.
    let opening = outside.first().is_some_and(|&paragraph| paragraph < first_step);
    let extra = outside.len() - usize::from(opening);
    if extra > 0 {
      let heading = pyfmt::str_repr(&named_section(&context.nodes, section));
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

/// `explain-examples`: the page holds a `div.example.e2e` and a `div.example.contrast`, and each `div.example` names its
/// source in a `span.src`.
fn check_explain_examples(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut failures = Vec::new();
  for (kind, label) in [("e2e", "div.example.e2e"), ("contrast", "div.example.contrast")] {
    if find(&context.nodes, 0, "div", &["example", kind]).is_empty() {
      failures.push(outcome(EXPLAIN_EXAMPLES, false, format!("no {label} on the page")));
    }
  }
  for (index, example) in find(&context.nodes, 0, "div", &["example"]).into_iter().enumerate() {
    if find(&context.nodes, example, "span", &["src"]).is_empty() {
      let index = index + 1;
      let heading = pyfmt::str_repr(&section_heading(&context.nodes, example));
      failures.push(outcome(EXPLAIN_EXAMPLES, false, format!("example #{index} (section {heading}) has no span.src")));
    }
  }
  if failures.is_empty() { Ok(vec![outcome(EXPLAIN_EXAMPLES, true, "")]) } else { Ok(failures) }
}

/// `_check_fact_anchored`: every `span.tag.fact` label sits in a block that also carries a `span.src`.
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
    let heading = pyfmt::str_repr(&section_heading(&context.nodes, fact));
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

/// `_has_readable_text`: the text carries a Unicode letter or digit; punctuation alone is not a gloss.
fn has_readable_text(text: &str) -> bool {
  text.chars().any(pystr::is_python_alphanumeric)
}

/// `_marked_element_text`: the block's text with the target element fenced by the requirement sentinels, so the text
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

/// `_check_req_chips`: each `span.req` chip carries a gloss, readable text after the chip in the sentence that holds
/// it, before that sentence's terminator.
fn check_req_chips(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let chips = find(&context.nodes, 0, "span", &["req"]);
  if chips.is_empty() {
    return Ok(vec![outcome(REQ_CHIPS, false, "no span.req requirement chips on the page")]);
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
    let heading = pyfmt::str_repr(&section_heading(&context.nodes, *chip));
    let chip_text = pyfmt::str_repr(&pystr::collapse_whitespace(&element_text(&context.nodes, *chip)));
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

/// `_table_rows`: one text chunk per td/th cell of each row descendant.
fn table_rows(nodes: &[Element], block: usize) -> Vec<Vec<String>> {
  let mut rows = Vec::new();
  for row in find(nodes, block, "tr", &[]) {
    let cells: Vec<String> = nodes[row]
      .children
      .iter()
      .filter_map(|child| match child {
        Child::Element(child) if nodes[*child].tag == "td" || nodes[*child].tag == "th" => {
          Some(element_text(nodes, *child))
        }
        _ => None,
      })
      .collect();
    if !cells.is_empty() {
      rows.push(cells);
    }
  }
  rows
}

/// `_dl_rows`: one chunk list per dl entry, the dt text followed by the dd texts that belong to it.
fn dl_rows(nodes: &[Element], block: usize) -> Vec<Vec<String>> {
  let mut rows = Vec::new();
  let mut chunks: Option<Vec<String>> = None;
  for element in descendants(nodes, block) {
    if nodes[element].tag == "dt" {
      if let Some(finished) = chunks.take() {
        rows.push(finished);
      }
      chunks = Some(vec![element_text(nodes, element)]);
    } else if nodes[element].tag == "dd"
      && let Some(current) = chunks.as_mut()
    {
      current.push(element_text(nodes, element));
    }
  }
  if let Some(finished) = chunks {
    rows.push(finished);
  }
  rows
}

/// `_run_row_role`: `held` or `changed` when a row names that role and carries readable text besides it.
fn run_row_role(chunks: &[String]) -> Option<&'static str> {
  for (role, pattern) in [("held", &*RUN_HELD_RE), ("changed", &*RUN_CHANGED_RE)] {
    for (index, chunk) in chunks.iter().enumerate() {
      let Ok(Some(found)) = pattern.find(chunk) else { continue };
      let mut value = String::new();
      value.push_str(&chunk[..found.start()]);
      value.push_str(&chunk[found.end()..]);
      for (other_index, other) in chunks.iter().enumerate() {
        if other_index != index {
          value.push(' ');
          value.push_str(other);
        }
      }
      if has_readable_text(&value) {
        return Some(role);
      }
    }
  }
  None
}

/// `_check_run_stated`: a page that shows a decimal in a table cell states its run's changed and held-fixed factors,
/// in another table or in a dl.
fn check_run_stated(context: &Context<'_>) -> anyhow::Result<Vec<AssertionOutcome>> {
  let mut cells = find(&context.nodes, 0, "td", &[]);
  cells.extend(find(&context.nodes, 0, "th", &[]));
  let numeric_cells: Vec<usize> = cells
    .into_iter()
    .filter(|&cell| RUN_DECIMAL_RE.is_match(&element_text(&context.nodes, cell)).unwrap_or(false))
    .collect();
  if numeric_cells.is_empty() {
    return Ok(vec![outcome(RUN_STATED, true, "no numeric table cell")]);
  }

  let mut numeric_tables = HashSet::new();
  for cell in numeric_cells {
    let mut table = context.nodes[cell].parent;
    while let Some(current) = table {
      if context.nodes[current].tag == "table" {
        numeric_tables.insert(current);
        break;
      }
      table = context.nodes[current].parent;
    }
  }

  let mut observed_roles = HashSet::new();
  for block in descendants(&context.nodes, 0) {
    let tag = context.nodes[block].tag.as_str();
    if tag != "table" && tag != "dl" {
      continue;
    }
    if numeric_tables.contains(&block) && numeric_tables.len() == 1 {
      continue;
    }
    let rows = if tag == "table" { table_rows(&context.nodes, block) } else { dl_rows(&context.nodes, block) };
    let roles: HashSet<&str> = rows.iter().filter_map(|chunks| run_row_role(chunks)).collect();
    if roles.contains("changed") && roles.contains("held") {
      return Ok(vec![outcome(RUN_STATED, true, "another table or dl states the changed and held-fixed factors")]);
    }
    observed_roles.extend(roles);
  }

  let mut missing: Vec<&str> = Vec::new();
  for (role, label) in [("changed", "changed-factor row"), ("held", "held-fixed-factor row")] {
    if !observed_roles.contains(role) {
      missing.push(label);
    }
  }
  if missing.is_empty() {
    missing.push("changed-factor row and held-fixed-factor row in the same block");
  }
  Ok(vec![outcome(
    RUN_STATED,
    false,
    format!(
      "a table cell holds a decimal number, but no other table or dl states the {} with a value",
      missing.join(" and the ")
    ),
  )])
}

/// `_check_render_path`: pre-rendered `class="katex"` markup, or the full KaTeX script injection.
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
  fn recorded_python_cases_match_every_plan_assertion_and_control_difference() {
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
          fs::write(&path, "#!/bin/sh\necho 'synthetic renderer failure' >&2\nexit 1\n").unwrap();
          let mut permissions = fs::metadata(&path).unwrap().permissions();
          use std::os::unix::fs::PermissionsExt;
          permissions.set_mode(0o755);
          fs::set_permissions(&path, permissions).unwrap();
          path
        }
        "empty" => PathBuf::from(""),
        other => panic!("unknown recorded Chrome mode {other:?}"),
      };
      let chrome_text = chrome.to_string_lossy().into_owned();
      let outcomes = run_assertions_sync("plan", &page, &chrome_text, &repo_root()).unwrap();
      // The fixture predates `fork-options`, so that outcome has no recorded Python counterpart to compare with.
      let actual =
        outcomes.iter().filter(|outcome| outcome.name != FORK_OPTIONS).map(outcome_value).collect::<Vec<_>>();
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
  fn test_shipped_template_passes_its_own_genre_plan() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = repo_root().join("prompts/plan_template.html");
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert_eq!(outcomes.len(), 13);
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
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
  fn test_page_height_over_the_target_passes_and_names_the_overage() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let chrome = stub_chrome(directory.path(), 2600);
    let outcomes = run_plan_with_height(&page, chrome.to_str().unwrap(), 2600);
    assert_eq!(by_name(&outcomes, PAGE_HEIGHT)[0].detail, "2600 px: 600 px over the 2000 px target");
    assert!(by_name(&outcomes, PAGE_HEIGHT)[0].passed);
  }

  #[test]
  fn test_page_height_at_or_under_the_target_reports_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let chrome = stub_chrome(directory.path(), 1967);
    let outcomes = run_plan_with_height(&page, chrome.to_str().unwrap(), 1967);
    assert_eq!(by_name(&outcomes, PAGE_HEIGHT)[0].detail, "1967 px (target 2000)");
    assert!(by_name(&outcomes, PAGE_HEIGHT)[0].passed);
  }

  #[test]
  fn test_page_height_without_a_renderer_fails() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let outcomes = run_plan(&page, "");
    assert_eq!(
      by_name(&outcomes, PAGE_HEIGHT)[0].detail,
      "headless_chrome_bin is required for plan registration: set it in the host config.yaml to the absolute path of a headless-chromium-compatible binary"
    );
    assert!(!by_name(&outcomes, PAGE_HEIGHT)[0].passed);
  }

  #[test]
  fn test_page_height_with_missing_configured_path_uses_the_python_detail() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let chrome = directory.path().join("missing-chrome");
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert_eq!(
      by_name(&outcomes, PAGE_HEIGHT)[0].detail,
      format!(
        "headless_chrome_bin is required for plan registration but the configured path does not exist: {}",
        chrome.display()
      )
    );
    assert!(!by_name(&outcomes, PAGE_HEIGHT)[0].passed);
  }

  #[test]
  #[ignore = "requires CHARLIEBOT_TEST_CHROME_BIN to point at headless Chrome"]
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
  fn test_plan_present_accepts_a_page_over_the_height_target() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 2600);
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let outcomes = run_plan_with_height(&page, chrome.to_str().unwrap(), 2600);
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  fn plan_page_height_probe_is_removed_after_a_renderer_launch_failure() {
    let directory = tempfile::tempdir().unwrap();
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix."));
    let chrome = chrome_program(directory.path(), json!([{"stderr": "failure\n"}, {"exit": 1}]));
    let outcomes = run_assertions_sync("plan", &page, chrome.to_str().unwrap(), &repo_root()).unwrap();
    assert!(!by_name(&outcomes, PAGE_HEIGHT)[0].passed);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
  }

  #[test]
  fn plan_section_numbers_accept_unicode_decimal_digits() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix.").replace(">1</span>", ">١</span>"));
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert!(by_name(&outcomes, SECTIONS_NUMBERED)[0].passed);
  }

  #[test]
  fn plan_section_number_span_must_hold_digits() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = write_page(directory.path(), "plan.html", &plan_page("Ship the fix.").replace(">1</span>", "></span>"));
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert_eq!(by_name(&outcomes, SECTIONS_NUMBERED)[0].detail, "h2 'Problem / Goal' span.n reads '', not an integer");
  }

  #[test]
  fn ordinal_label_after_a_list_marker_takes_the_colon_gloss() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = write_page(directory.path(), "plan.html", &plan_page("2. Plan 3: ship the fix."));
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert!(by_name(&outcomes, ORDINAL_NAMED)[0].passed);
  }

  #[test]
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
  fn weighted_goal_counts_code_points_and_python_whitespace() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = write_page(directory.path(), "plan.html", &plan_page("你\u{001c}好"));
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert_eq!(by_name(&outcomes, GOAL_BUDGET)[0].detail, "5 weighted chars (budget 240)");
  }

  #[test]
  fn failure_outcomes_serialize_as_one_json_object_each() {
    let value = outcome_value(&outcome("plan", false, "detail"));
    assert_eq!(value["name"], "plan");
    assert_eq!(value["passed"], false);
    assert_eq!(value["detail"], "detail");
  }

  /// Runs only `names` over `source` as a page file, so a fork test needs no Chrome and no template.
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
  fn a_plain_closed_details_hides_its_option_from_the_check() {
    let page = fork_page("Ship the fix.", ONE_VISIBLE_OPTION_AND_ONE_IN_A_PLAIN_DETAILS);
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.contains("has 1 option line outside folds (2 required)"), "{}", outcomes[0].detail);
  }

  #[test]
  fn an_open_details_shows_its_option_to_the_check() {
    let body = ONE_VISIBLE_OPTION_AND_ONE_IN_A_PLAIN_DETAILS.replacen("<details>", "<details open>", 1);
    let page = fork_page("Ship the fix.", &body);
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
    assert!(outcomes[0].passed, "{outcomes:#?}");
  }

  #[test]
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
  fn a_resolved_fork_is_skipped() {
    let page = fork_page("Ship the fix.", "<p class=\"resolved\">Resolved: keep the file.</p>");
    let outcomes = run_only("plan", &page, &[FORK_OPTIONS]);
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
      [FORK_OPEN_SHAPE, FORK_OPTIONS, FORK_EXPLAINER]
    );
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  /// A waiting page as `charliebot artifact wrap --genre waiting` assembles it: one context sentence and one fork.
  fn waiting_sample() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/waiting_page.html")
  }

  #[test]
  fn the_waiting_sample_passes_every_waiting_assertion_in_order() {
    let outcomes = run_assertions_sync("waiting", &waiting_sample(), "", &repo_root()).unwrap();
    let names = outcomes.iter().map(|outcome| outcome.name.as_str()).collect::<Vec<_>>();
    assert_eq!(
      names,
      [
        BYTE_INTEGRITY,
        STYLE_VERBATIM,
        FORK_OPEN_SHAPE,
        FORK_OPTIONS,
        FORK_EXPLAINER,
        ORDINAL_NAMED,
        TRACKER_ID_NEARBY,
        SESSION_LINKED,
        LINK_RENDERABLE,
      ]
    );
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  fn every_genre_runs_the_three_shared_user_text_checks() {
    for genre in ARTIFACT_GENRES {
      let names = assertion_set(genre).expect("every artifact genre has an assertion set");
      assert!(user_text_checks::CHECK_NAMES.iter().all(|check| names.contains(check)), "{genre}: {names:?}");
    }
  }

  #[test]
  fn each_user_text_hit_fails_with_its_snippet_block_and_fix() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("sessions")).unwrap();
    fs::write(home.path().join("config.yaml"), "user_text_checks:\n  internal_id_patterns: ['zz\\d{3}']\n").unwrap();
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("src.features.user_text_checks", charliebot_user_text_checks::register).unwrap();
    let config = registry.load_config(home.path()).unwrap();
    let id = Uuid::new_v4().to_string();
    fs::create_dir(home.path().join("sessions").join(&id)).unwrap();
    let chrome = stub_chrome(home.path(), 800);
    let cases = [
      ("tracker-id-nearby", "Internal id zz123.", "zz123", "write the Linear id right next to the internal id"),
      ("session-linked", id.as_str(), id.as_str(), "write the id as"),
      (
        "link-renderable",
        r#"<a href="https://example.invalid/export.tsv">the raw table</a>"#,
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
        config.section::<UserTextChecksConfig>("user_text_checks"),
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

  /// `tests/core/test_artifact_check.py::_genre_doc`: the genre template's `<style>` block verbatim plus `body`.
  fn genre_doc(genre: &str, body: &str) -> String {
    let template = fs::read_to_string(repo_root().join("prompts").join(genre_template(genre))).unwrap();
    let start = template.find("<style>").expect("the template carries a style block");
    let end = template.find("</style>").expect("the style block closes") + "</style>".len();
    format!("<html><head>{}</head><body>{body}</body></html>", &template[start..end])
  }

  /// `tests/core/test_artifact_check.py::_sitrep_ok_doc`: a sitrep page that passes every sitrep assertion.
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

  const NUMERIC_TABLE: &str = concat!(
    "<table><thead><tr><th>checkpoint</th><th>score</th></tr></thead>",
    "<tbody><tr><td>10</td><td>0.002378</td></tr></tbody></table>",
  );

  /// `tests/core/test_artifact_check.py::_sitrep_run_doc`: the sitrep page with a measured table and `variable_block`.
  fn sitrep_run_doc(variable_block: &str) -> String {
    sitrep_ok_doc().replace(
      r#"<p>Done. <span class="src">s</span></p>"#,
      &format!("{NUMERIC_TABLE}{variable_block}{}", r#"<p>Done. <span class="src">s</span></p>"#),
    )
  }

  fn run_sitrep(directory: &Path, source: &str) -> Vec<AssertionOutcome> {
    // The sitrep set holds no page-height assertion, so it needs no renderer.
    let page = write_page(directory, "page.html", source);
    run_assertions_sync("sitrep", &page, "", &repo_root()).unwrap()
  }

  #[test]
  fn test_req_chips_passes_when_the_chip_is_glossed_in_its_sentence() {
    let directory = tempfile::tempdir().unwrap();
    let outcomes = run_sitrep(directory.path(), &sitrep_ok_doc());
    let chips = by_name(&outcomes, REQ_CHIPS);
    assert_eq!(chips.len(), 1, "{outcomes:#?}");
    assert!(chips[0].passed, "{:?}", chips[0].detail);
  }

  #[test]
  fn test_req_chips_fails_when_the_sentence_ends_after_the_chip() {
    let directory = tempfile::tempdir().unwrap();
    let doc = sitrep_ok_doc().replace(
      r#"<span class="req">r1</span> states the first requirement."#,
      r#"<span class="req">r1</span>. A later sentence names the requirement."#,
    );
    let outcomes = run_sitrep(directory.path(), &doc);
    let chips = by_name(&outcomes, REQ_CHIPS);
    assert_eq!(chips.len(), 1, "{outcomes:#?}");
    assert!(!chips[0].passed);
    assert!(chips[0].detail.contains("'r1'"), "{}", chips[0].detail);
    assert!(chips[0].detail.contains("no gloss"), "{}", chips[0].detail);
  }

  #[test]
  fn test_req_chips_accepts_a_gloss_in_another_unicode_script() {
    let directory = tempfile::tempdir().unwrap();
    let doc = sitrep_ok_doc().replace(
      r#"<span class="req">r1</span> states the first requirement."#,
      r#"<span class="req">r1</span> 要件の説明です。"#,
    );
    let outcomes = run_sitrep(directory.path(), &doc);
    let chips = by_name(&outcomes, REQ_CHIPS);
    assert_eq!(chips.len(), 1, "{outcomes:#?}");
    assert!(chips[0].passed, "{:?}", chips[0].detail);
  }

  #[test]
  fn test_run_stated_passes_when_a_table_or_dl_states_both_roles() {
    let directory = tempfile::tempdir().unwrap();
    let variable_blocks = [
      concat!(
        "<table><tr><th>role</th><th>value</th></tr>",
        "<tr><td>Changed</td><td>weights randomly initialized; 3,506 columns dropped</td></tr>",
        "<tr><td>Held fixed</td><td>commit e0852cbb; seed 3</td></tr></table>",
      ),
      concat!(
        "<table><tr><td>Changed</td><td>learning rate 0.0001</td></tr>",
        "<tr><td>Held fixed</td><td>seed 3</td></tr></table>",
      ),
      concat!(
        "<dl><dt>改了什么</dt><dd>权重随机初始化，删掉 3,506 列</dd>",
        "<dt>保持不变</dt><dd>提交 e0852cbb，种子 3</dd></dl>",
      ),
    ];
    for variable_block in variable_blocks {
      let outcomes = run_sitrep(directory.path(), &sitrep_run_doc(variable_block));
      let stated = by_name(&outcomes, RUN_STATED);
      assert_eq!(stated.len(), 1, "{variable_block}: {outcomes:#?}");
      assert!(stated[0].passed, "{variable_block}: {:?}", stated[0].detail);
    }
  }

  #[test]
  fn test_run_stated_fails_when_the_numeric_table_has_no_variable_block() {
    let directory = tempfile::tempdir().unwrap();
    let outcomes = run_sitrep(directory.path(), &sitrep_run_doc(""));
    let stated = by_name(&outcomes, RUN_STATED);
    assert_eq!(stated.len(), 1, "{outcomes:#?}");
    assert!(!stated[0].passed);
    assert!(stated[0].detail.contains("changed-factor row"), "{}", stated[0].detail);
    assert!(stated[0].detail.contains("held-fixed-factor row"), "{}", stated[0].detail);
  }

  #[test]
  fn test_run_stated_requires_a_block_separate_from_the_numeric_table() {
    let directory = tempfile::tempdir().unwrap();
    let table_with_inline_factors = NUMERIC_TABLE.replace(
      "</tbody></table>",
      concat!(
        "<tr><td>Changed</td><td>3,506 columns dropped</td></tr>",
        "<tr><td>Held fixed</td><td>seed 3</td></tr></tbody></table>",
      ),
    );
    let doc = sitrep_ok_doc().replace(
      r#"<p>Done. <span class="src">s</span></p>"#,
      &format!("{table_with_inline_factors}{}", r#"<p>Done. <span class="src">s</span></p>"#),
    );
    let outcomes = run_sitrep(directory.path(), &doc);
    let stated = by_name(&outcomes, RUN_STATED);
    assert_eq!(stated.len(), 1, "{outcomes:#?}");
    assert!(!stated[0].passed);
    assert!(stated[0].detail.contains("changed-factor row"), "{}", stated[0].detail);
    assert!(stated[0].detail.contains("held-fixed-factor row"), "{}", stated[0].detail);
  }

  #[test]
  fn test_run_stated_requires_both_role_rows_in_one_block() {
    let directory = tempfile::tempdir().unwrap();
    let variable_blocks = concat!(
      "<table><tr><td>Changed</td><td>3,506 columns dropped</td></tr></table>",
      "<dl><dt>Held fixed</dt><dd>seed 3</dd></dl>",
    );
    let outcomes = run_sitrep(directory.path(), &sitrep_run_doc(variable_blocks));
    let stated = by_name(&outcomes, RUN_STATED);
    assert_eq!(stated.len(), 1, "{outcomes:#?}");
    assert!(!stated[0].passed);
    assert!(
      stated[0].detail.contains("changed-factor row and held-fixed-factor row in the same block"),
      "{}",
      stated[0].detail
    );
  }

  #[test]
  fn test_run_stated_names_the_one_missing_role_row() {
    let directory = tempfile::tempdir().unwrap();
    let block = "<table><tr><td>Changed</td><td>3,506 columns dropped</td></tr></table>";
    let outcomes = run_sitrep(directory.path(), &sitrep_run_doc(block));
    let stated = by_name(&outcomes, RUN_STATED);
    assert_eq!(stated.len(), 1, "{outcomes:#?}");
    assert!(!stated[0].passed);
    assert!(stated[0].detail.contains("held-fixed-factor row"), "{}", stated[0].detail);
    assert!(!stated[0].detail.contains("changed-factor row"), "{}", stated[0].detail);
  }

  #[test]
  fn test_run_stated_is_inert_without_a_decimal_table_cell() {
    let directory = tempfile::tempdir().unwrap();
    let doc = sitrep_ok_doc().replace(
      r#"<p>Done. <span class="src">s</span></p>"#,
      concat!(
        r#"<table><tr><td>checkpoint</td><td>10</td></tr></table>"#,
        r#"<p>Done. <span class="src">s</span></p>"#,
      ),
    );
    let outcomes = run_sitrep(directory.path(), &doc);
    let stated = by_name(&outcomes, RUN_STATED);
    assert_eq!(stated.len(), 1, "{outcomes:#?}");
    assert!(stated[0].passed, "{:?}", stated[0].detail);
    assert_eq!(stated[0].detail, "no numeric table cell");
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
  fn test_shipped_explain_template_passes_its_explain_set() {
    let template = repo_root().join("prompts/explain_template.html");
    let outcomes = run_assertions_sync("explain", &template, "", &repo_root()).unwrap();
    assert_eq!(outcomes.len(), 13, "{outcomes:#?}");
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  fn test_explain_page_with_figured_steps_and_both_examples_passes_every_assertion() {
    let mechanism = format!("{OPENING}{}{}", step(FIGURE), step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, &format!("{E2E}{CONTRAST}")));
    assert!(outcomes.iter().all(|outcome| outcome.passed), "{outcomes:#?}");
  }

  #[test]
  fn test_explain_figures_fails_a_page_with_no_step() {
    let outcomes = run_explain(&explain_body(OPENING, &format!("{E2E}{CONTRAST}")));
    let failures = failed(&outcomes, EXPLAIN_FIGURES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "no div.step on the page");
  }

  #[test]
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
  fn test_explain_figures_fails_a_paragraph_after_the_steps() {
    let mechanism = format!("{OPENING}{}<p>A stray paragraph.</p>", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, &format!("{E2E}{CONTRAST}")));
    let failures = failed(&outcomes, EXPLAIN_FIGURES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "section '2 The mechanism' has 1 p blocks outside div.step after its opening p");
  }

  #[test]
  fn test_explain_figures_fails_a_second_paragraph_before_the_first_step() {
    let mechanism = format!("{OPENING}<p>A second opening.</p>{}", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, &format!("{E2E}{CONTRAST}")));
    let failures = failed(&outcomes, EXPLAIN_FIGURES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "section '2 The mechanism' has 1 p blocks outside div.step after its opening p");
  }

  #[test]
  fn test_explain_examples_fails_a_page_with_no_e2e_example() {
    let mechanism = format!("{OPENING}{}", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, CONTRAST));
    let failures = failed(&outcomes, EXPLAIN_EXAMPLES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "no div.example.e2e on the page");
  }

  #[test]
  fn test_explain_examples_fails_a_page_with_no_contrast_example() {
    let mechanism = format!("{OPENING}{}", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, E2E));
    let failures = failed(&outcomes, EXPLAIN_EXAMPLES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "no div.example.contrast on the page");
  }

  #[test]
  fn test_explain_examples_fails_an_example_without_a_source() {
    let unsourced = r#"<div class="example e2e"><h3>Frame 41 to 42</h3><p>Frame 42 recompiles.</p></div>"#;
    let mechanism = format!("{OPENING}{}", step(FIGURE));
    let outcomes = run_explain(&explain_body(&mechanism, &format!("{unsourced}{CONTRAST}")));
    let failures = failed(&outcomes, EXPLAIN_EXAMPLES);
    assert_eq!(failures.len(), 1, "{outcomes:#?}");
    assert_eq!(failures[0].detail, "example #1 (section '3 Worked example') has no span.src");
  }
}

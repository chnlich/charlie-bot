use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context as AnyhowContext, anyhow, bail};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::pyfmt;
use charliebot_core::pyregex::compile_python_regex;
use charliebot_core::pystr;
use charliebot_pyhtml::{EventKind, parse_with, unescape};
use fancy_regex::Regex;
use serde::Serialize;
use url::Url;
use uuid::Uuid;

use crate::headless_render;

const STYLE_VERBATIM: &str = "style-verbatim";
const SECTIONS_NUMBERED: &str = "sections-numbered";
const FOOT_PRESENT: &str = "foot-present";
const FORK_OPEN_SHAPE: &str = "fork-open-shape";
const FORK_EXPLAINER: &str = "fork-explainer";
const GOAL_BUDGET: &str = "goal-budget";
const PAGE_HEIGHT: &str = "page-height";
const ORDINAL_NAMED: &str = "ordinal-named";
const BYTE_INTEGRITY: &str = "byte-integrity";

const GOAL_WEIGHTED_BUDGET: usize = 240;
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
  parent: Option<usize>,
  children: Vec<Child>,
}

#[derive(Clone, Debug)]
enum Child {
  Element(usize),
  Text(String),
}

struct Context<'a> {
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

/// Run the plan assertion set on a blocking thread.
pub async fn run_assertions(
  genre: &str,
  artifact: PathBuf,
  config: &CharlieBotConfig,
  repo_root: PathBuf,
) -> anyhow::Result<Vec<AssertionOutcome>> {
  run_assertions_with_chrome(genre, artifact, config.headless_chrome_bin.clone(), repo_root).await
}

/// Run the plan assertion set with an explicit Chrome path on a blocking thread.
pub async fn run_assertions_with_chrome(
  genre: &str,
  artifact: PathBuf,
  chrome_bin: String,
  repo_root: PathBuf,
) -> anyhow::Result<Vec<AssertionOutcome>> {
  let genre = genre.to_owned();
  tokio::task::spawn_blocking(move || run_assertions_sync(&genre, &artifact, &chrome_bin, &repo_root))
    .await
    .context("plan assertion worker failed")?
}

pub(crate) fn run_assertions_sync(
  genre: &str,
  artifact: &Path,
  chrome_bin: &str,
  repo_root: &Path,
) -> anyhow::Result<Vec<AssertionOutcome>> {
  if genre != "plan" {
    bail!(
      "no assertion set registered for genre {}; genres with a set: plan, understanding, sitrep, debug, explain",
      pyfmt::str_repr(genre)
    );
  }
  let source =
    fs::read_to_string(artifact).with_context(|| format!("could not read artifact {}", artifact.display()))?;
  let nodes = parse_dom(&source)?;
  let context = Context { artifact, chrome_bin, repo_root, nodes };
  let checks: [AssertionCheck; 9] = [
    check_byte_integrity,
    check_style_verbatim,
    check_sections_numbered,
    check_foot_present,
    check_fork_open_shape,
    check_fork_explainer,
    check_goal_budget,
    check_page_height,
    check_ordinal_named,
  ];
  let mut outcomes = Vec::new();
  for check in checks {
    outcomes.extend(check(&context)?);
  }
  Ok(outcomes)
}

fn parse_dom(source: &str) -> anyhow::Result<Vec<Element>> {
  let mut nodes =
    vec![Element { tag: "#root".to_owned(), classes: HashSet::new(), parent: None, children: Vec::new() }];
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
      nodes.push(Element { tag: tag.clone(), classes, parent: Some(parent), children: Vec::new() });
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
      return find(nodes, index, "h2", &[])
        .first()
        .map(|&heading| pystr::collapse_whitespace(&element_text(nodes, heading)))
        .unwrap_or_else(|| "<unnamed section>".to_owned());
    }
    ancestor = nodes[index].parent;
  }
  "<no section>".to_owned()
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
  let template_rel = "prompts/plan_template.html";
  let template_path = context.repo_root.join(template_rel);
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
  if values.len() != 6 {
    return Ok(vec![outcome(
      SECTIONS_NUMBERED,
      false,
      format!("plan requires exactly 6 numbered sections, found {}", values.len()),
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

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::{Path, PathBuf};

  use serde_json::{Value, json};

  use super::*;
  use crate::test_support::{chrome_program, plan_page, repo_root};

  fn write_page(directory: &Path, name: &str, source: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, source).unwrap();
    path
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
  fn test_shipped_template_passes_its_own_genre_plan() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = stub_chrome(directory.path(), 800);
    let page = repo_root().join("prompts/plan_template.html");
    let outcomes = run_plan(&page, chrome.to_str().unwrap());
    assert_eq!(outcomes.len(), 9);
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
}

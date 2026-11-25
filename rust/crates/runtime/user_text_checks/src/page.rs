//! The HTML page scan: the text blocks, text runs, code runs and links that the checks read.

use anyhow::anyhow;
use charliebot_html::{EventKind, VOID_TAGS, parse_with};

const PAGE_BLOCK_TAGS: &[&str] =
  &["p", "li", "tr", "dd", "dt", "h1", "h2", "h3", "h4", "blockquote", "figcaption", "summary", "pre"];

pub(crate) struct PageScan {
  pub(crate) blocks: Vec<String>,
  pub(crate) text: Vec<PageText>,
  pub(crate) code: Vec<PageCode>,
  pub(crate) links: Vec<PageLink>,
}

pub(crate) struct PageText {
  pub(crate) text: String,
  pub(crate) block: Option<usize>,
  pub(crate) anchor_href: Option<String>,
  pub(crate) in_pre: bool,
}

pub(crate) struct PageCode {
  pub(crate) text: String,
  pub(crate) block: Option<usize>,
  pub(crate) anchor_href: Option<String>,
  pub(crate) in_pre: bool,
}

pub(crate) struct PageLink {
  pub(crate) href: String,
  pub(crate) block: Option<usize>,
  pub(crate) label: String,
}

struct ElementState {
  tag: String,
  block: Option<usize>,
  anchor: Option<usize>,
  code: Option<usize>,
  in_pre: bool,
  hidden: bool,
}

pub(crate) fn parse_page(html: &str) -> anyhow::Result<PageScan> {
  let mut page = PageScan { blocks: Vec::new(), text: Vec::new(), code: Vec::new(), links: Vec::new() };
  let mut stack = vec![ElementState {
    tag: "#root".to_string(),
    block: None,
    anchor: None,
    code: None,
    in_pre: false,
    hidden: false,
  }];
  parse_with(html, true, |event| match event.kind {
    EventKind::StartTag | EventKind::StartEndTag => {
      let arguments = event.args.as_array().expect("a start-tag event has arguments");
      let tag = arguments[0].as_str().expect("a start-tag event has a tag").to_string();
      let parent = stack.last().expect("the root remains on the parser stack");
      let block = if PAGE_BLOCK_TAGS.contains(&tag.as_str()) {
        let index = page.blocks.len();
        page.blocks.push(String::new());
        Some(index)
      } else {
        parent.block
      };
      let attributes = arguments[1].as_array().expect("a start-tag event has attributes");
      let href = attributes.iter().find_map(|attribute| {
        let pair = attribute.as_array().expect("an attribute has a pair");
        (pair[0].as_str() == Some("href")).then(|| pair[1].as_str().map(str::to_string)).flatten()
      });
      let anchor = if tag == "a" {
        if let Some(href) = href {
          let index = page.links.len();
          page.links.push(PageLink { href, block, label: String::new() });
          Some(index)
        } else {
          parent.anchor
        }
      } else {
        parent.anchor
      };
      let code = if tag == "code" {
        let index = page.code.len();
        page.code.push(PageCode {
          text: String::new(),
          block,
          anchor_href: parent.anchor.map(|anchor| page.links[anchor].href.clone()),
          in_pre: parent.in_pre || tag == "pre",
        });
        Some(index)
      } else {
        parent.code
      };
      let in_pre = parent.in_pre || tag == "pre";
      let hidden = parent.hidden || matches!(tag.as_str(), "script" | "style");
      if event.kind == EventKind::StartTag && !VOID_TAGS.contains(&tag.as_str()) {
        stack.push(ElementState { tag, block, anchor, code, in_pre, hidden });
      }
    }
    EventKind::EndTag => {
      let tag = event.args[0].as_str().expect("an end-tag event has a tag");
      if let Some(position) = (1..stack.len()).rev().find(|&position| stack[position].tag == tag) {
        stack.truncate(position);
      }
    }
    EventKind::Data => {
      let text = event.args[0].as_str().expect("a data event has text");
      let current = stack.last().expect("the root remains on the parser stack");
      if current.hidden {
        return;
      }
      if let Some(block) = current.block {
        page.blocks[block].push_str(text);
      }
      if let Some(anchor) = current.anchor {
        page.links[anchor].label.push_str(text);
      }
      if let Some(code) = current.code {
        page.code[code].text.push_str(text);
      } else {
        page.text.push(PageText {
          text: text.to_string(),
          block: current.block,
          anchor_href: current.anchor.map(|anchor| page.links[anchor].href.clone()),
          in_pre: current.in_pre,
        });
      }
    }
    EventKind::EntityReference
    | EventKind::CharacterReference
    | EventKind::Comment
    | EventKind::Declaration
    | EventKind::ProcessingInstruction
    | EventKind::UnknownDeclaration => {}
  })
  .map_err(|error| anyhow!("{}: {}", error.exception, error.message))?;
  Ok(page)
}

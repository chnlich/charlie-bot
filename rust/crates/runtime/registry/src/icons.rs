//! Lucide icons used by CharlieBot pages.
//!
//! `icons.json` maps each meaning to one source SVG and an optional filled state. The SVG files keep their upstream
//! children; this module supplies the shared frame and the browser script embeds those same children for `cbIcon`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt::{self, Write as FmtWrite};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A malformed or incomplete icon table, or a request for a meaning the table does not define.
#[derive(Debug)]
pub struct IconError(String);

impl IconError {
  fn new(message: impl Into<String>) -> IconError {
    IconError(message.into())
  }
}

impl fmt::Display for IconError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl Error for IconError {}

#[derive(Deserialize)]
struct Definition {
  lucide: String,
  #[serde(default)]
  fill: bool,
}

#[derive(Debug)]
struct Icon {
  lucide: String,
  fill: bool,
  shapes: String,
}

/// The validated icon meanings and their source SVG children.
#[derive(Debug)]
pub struct Icons {
  entries: BTreeMap<String, Icon>,
}

#[derive(Serialize)]
struct ScriptIcon<'a> {
  lucide: &'a str,
  #[serde(skip_serializing_if = "is_false")]
  fill: bool,
  shapes: &'a str,
}

fn is_false(value: &bool) -> bool {
  !value
}

impl Icons {
  /// Reads and validates the icon table, all named SVGs, the SVG directory, and the package license.
  pub fn load(repo_root: &Path) -> Result<Icons, IconError> {
    let icon_root = repo_root.join("web/static/icons");
    let license_path = icon_root.join("LICENSE");
    fs::read(&license_path)
      .map_err(|error| IconError::new(format!("read icon license {}: {error}", license_path.display())))?;

    let table_path = icon_root.join("icons.json");
    let table = fs::read_to_string(&table_path)
      .map_err(|error| IconError::new(format!("read icon table {}: {error}", table_path.display())))?;
    let definitions: BTreeMap<String, Definition> = serde_json::from_str(&table)
      .map_err(|error| IconError::new(format!("parse icon table {}: {error}", table_path.display())))?;

    let mut combinations = HashMap::new();
    for (key, definition) in &definitions {
      if let Some(previous) = combinations.insert((definition.lucide.as_str(), definition.fill), key.as_str()) {
        return Err(IconError::new(format!(
          "icon meanings '{previous}' and '{key}' share the same (lucide, fill) combination ({}, {})",
          definition.lucide, definition.fill
        )));
      }
    }

    let lucide_dir = icon_root.join("lucide");
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(&lucide_dir)
      .map_err(|error| IconError::new(format!("read Lucide directory {}: {error}", lucide_dir.display())))?
    {
      let entry = entry.map_err(|error| IconError::new(format!("read Lucide directory entry: {error}")))?;
      let path = entry.path();
      if path.extension().is_none_or(|extension| extension != "svg") {
        continue;
      }
      let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| IconError::new(format!("Lucide SVG name is not UTF-8: {}", path.display())))?
        .to_string();
      let source = fs::read_to_string(&path)
        .map_err(|error| IconError::new(format!("read Lucide SVG {}: {error}", path.display())))?;
      if !source
        .lines()
        .next()
        .is_some_and(|line| line.starts_with("<!-- @license lucide-static ") && line.ends_with(" - ISC -->"))
      {
        return Err(IconError::new(format!("Lucide SVG {} is missing its ISC license header", path.display())));
      }
      let shapes = svg_children(&source, &path)?;
      if files.insert(name.clone(), shapes).is_some() {
        return Err(IconError::new(format!("duplicate Lucide SVG name '{name}'")));
      }
    }

    let mut used = HashSet::new();
    let mut entries = BTreeMap::new();
    for (key, definition) in definitions {
      let shapes = files.get(&definition.lucide).ok_or_else(|| {
        IconError::new(format!("icon meaning '{key}' names missing Lucide SVG '{}.svg'", definition.lucide))
      })?;
      used.insert(definition.lucide.clone());
      entries.insert(key, Icon { lucide: definition.lucide, fill: definition.fill, shapes: shapes.clone() });
    }
    if let Some(name) = files.keys().find(|name| !used.contains(*name)) {
      return Err(IconError::new(format!("Lucide SVG '{name}.svg' is not used by icons.json")));
    }

    Ok(Icons { entries })
  }

  /// The meanings in key order.
  pub fn keys(&self) -> impl Iterator<Item = &str> {
    self.entries.keys().map(String::as_str)
  }

  /// The rendered SVG for `key`, at `size` pixels, with caller-owned class and escaped attributes.
  pub fn svg(&self, key: &str, size: u32, cls: &str, attrs: &str) -> Result<String, IconError> {
    let icon = self.entry(key)?;
    if size == 0 {
      return Err(IconError::new(format!("icon '{key}' needs a positive pixel size, got 0")));
    }
    let class_name = if cls.is_empty() { "cb-icon".to_string() } else { format!("cb-icon {cls}") };
    let fill = if icon.fill { "currentColor" } else { "none" };
    let stroke_width = std::cmp::max(2, 24 / size);
    let extra_attributes = if attrs.is_empty() { String::new() } else { format!(" {attrs}") };
    Ok(format!(
      "<svg class=\"{class_name}\" width=\"{size}\" height=\"{size}\" viewBox=\"0 0 24 24\" fill=\"{fill}\" stroke=\"currentColor\" stroke-width=\"{stroke_width}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"{extra_attributes}>{}</svg>",
      icon.shapes
    ))
  }

  /// The inline JavaScript table for all meanings or the requested subset.
  pub fn script(&self, keys: Option<&[&str]>) -> Result<String, IconError> {
    let selected = match keys {
      Some(keys) => keys.to_vec(),
      None => self.entries.keys().map(String::as_str).collect(),
    };
    let mut script = String::from("window.CB_ICON_TABLE = {");
    for (index, key) in selected.iter().enumerate() {
      let icon = self.entry(key)?;
      if index > 0 {
        script.push(',');
      }
      let key = serde_json::to_string(key).map_err(|error| IconError::new(format!("encode icon key: {error}")))?;
      let value = serde_json::to_string(&ScriptIcon { lucide: &icon.lucide, fill: icon.fill, shapes: &icon.shapes })
        .map_err(|error| IconError::new(format!("encode icon script entry: {error}")))?;
      write!(&mut script, "{key}:{value}").expect("writing to a String succeeds");
    }
    script.push_str("};\n");
    Ok(script)
  }

  fn entry(&self, key: &str) -> Result<&Icon, IconError> {
    self
      .entries
      .get(key)
      .ok_or_else(|| IconError::new(format!("unknown icon key '{key}' in web/static/icons/icons.json")))
  }
}

fn svg_children(source: &str, path: &Path) -> Result<String, IconError> {
  let open =
    source.find("<svg").ok_or_else(|| IconError::new(format!("Lucide SVG has no <svg> root: {}", path.display())))?;
  let content_start = source[open..]
    .find('>')
    .map(|offset| open + offset + 1)
    .ok_or_else(|| IconError::new(format!("Lucide SVG has an unfinished <svg> root: {}", path.display())))?;
  let content_end = source
    .rfind("</svg>")
    .ok_or_else(|| IconError::new(format!("Lucide SVG has no </svg> root: {}", path.display())))?;
  if content_end < content_start || !source[content_end + "</svg>".len()..].trim().is_empty() {
    return Err(IconError::new(format!("Lucide SVG has malformed root content: {}", path.display())));
  }
  Ok(source[content_start..content_end].to_string())
}

/// Writes the smallest icon tree that loads under `repo_root`, for tests that render from a temporary root.
#[cfg(test)]
pub(crate) fn write_minimal_icon_tree(repo_root: &Path) {
  let icon_root = repo_root.join("web/static/icons");
  fs::create_dir_all(icon_root.join("lucide")).expect("create the test icon directory");
  fs::write(icon_root.join("LICENSE"), "ISC\n").expect("write the test icon license");
  fs::write(icon_root.join("icons.json"), r#"{"close":{"lucide":"x"}}"#).expect("write the test icon table");
  fs::write(
    icon_root.join("lucide/x.svg"),
    "<!-- @license lucide-static v1.76.0 - ISC -->\n<svg viewBox=\"0 0 24 24\"><path d=\"M18 6 6 18\"/></svg>\n",
  )
  .expect("write the test icon");
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::{Path, PathBuf};

  use roxmltree::Document;
  use serde::Deserialize;

  use super::Icons;

  #[derive(Deserialize)]
  struct ParityFixture {
    script: String,
    cases: Vec<ParityCase>,
  }

  #[derive(Deserialize)]
  struct ParityCase {
    key: String,
    size: u32,
    cls: String,
    attrs: String,
    svg: String,
  }

  fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../").canonicalize().unwrap()
  }

  fn fixture_root() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
  }

  fn write_table(root: &Path, table: &str, icons: &[(&str, &str)]) {
    let icon_root = root.join("web/static/icons");
    let lucide_dir = icon_root.join("lucide");
    fs::create_dir_all(&lucide_dir).unwrap();
    fs::write(icon_root.join("LICENSE"), "ISC\n").unwrap();
    fs::write(icon_root.join("icons.json"), table).unwrap();
    for (name, source) in icons {
      fs::write(lucide_dir.join(format!("{name}.svg")), source).unwrap();
    }
  }

  const CIRCLE_SVG: &str = "<!-- @license lucide-static v1.76.0 - ISC -->\n<svg viewBox=\"0 0 24 24\"><circle cx=\"12\" cy=\"12\" r=\"10\"/></svg>\n";

  #[test]
  fn every_meaning_renders_a_parseable_svg() {
    let icons = Icons::load(&repository_root()).unwrap();
    let mut count = 0;
    for key in icons.keys() {
      let svg = icons.svg(key, 16, "", "").unwrap();
      let document = Document::parse(&svg).unwrap_or_else(|error| panic!("icon '{key}' is not valid XML: {error}"));
      assert_eq!(document.root_element().tag_name().name(), "svg", "icon '{key}' has an SVG root");
      count += 1;
    }
    assert_eq!(count, 49);
  }

  #[test]
  fn unknown_meanings_name_the_key() {
    let icons = Icons::load(&repository_root()).unwrap();
    let error = icons.svg("not-in-the-table", 16, "", "").unwrap_err();
    assert!(error.to_string().contains("not-in-the-table"));
  }

  #[test]
  fn duplicate_meaning_and_fill_pairs_fail_validation() {
    let root = fixture_root();
    write_table(
      root.path(),
      r#"{"first":{"lucide":"circle"},"second":{"lucide":"circle"}}"#,
      &[("circle", CIRCLE_SVG)],
    );
    let error = Icons::load(root.path()).unwrap_err();
    assert!(error.to_string().contains("first"));
    assert!(error.to_string().contains("second"));
  }

  #[test]
  fn a_missing_svg_fails_validation_with_its_meaning() {
    let root = fixture_root();
    write_table(root.path(), r#"{"close":{"lucide":"x"}}"#, &[]);
    let error = Icons::load(root.path()).unwrap_err();
    assert!(error.to_string().contains("close"));
    assert!(error.to_string().contains("x.svg"));
  }

  #[test]
  fn an_unused_svg_fails_validation_with_its_name() {
    let root = fixture_root();
    write_table(root.path(), r#"{"dot":{"lucide":"circle"}}"#, &[("circle", CIRCLE_SVG), ("unused", CIRCLE_SVG)]);
    let error = Icons::load(root.path()).unwrap_err();
    assert!(error.to_string().contains("unused.svg"));
  }

  #[test]
  fn a_missing_license_fails_validation() {
    let root = fixture_root();
    write_table(root.path(), r#"{"dot":{"lucide":"circle"}}"#, &[("circle", CIRCLE_SVG)]);
    fs::remove_file(root.path().join("web/static/icons/LICENSE")).unwrap();
    let error = Icons::load(root.path()).unwrap_err();
    assert!(error.to_string().contains("LICENSE"));
  }

  #[test]
  fn stroke_width_uses_the_shared_integer_rule_and_rejects_zero_size() {
    let icons = Icons::load(&repository_root()).unwrap();
    assert!(icons.svg("close", 8, "", "").unwrap().contains("stroke-width=\"3\""));
    assert!(icons.svg("close", 12, "", "").unwrap().contains("stroke-width=\"2\""));
    let error = icons.svg("close", 0, "", "").unwrap_err();
    assert!(error.to_string().contains("close"));
  }

  #[test]
  fn script_and_svg_match_the_shared_browser_fixtures() {
    let fixture: ParityFixture =
      serde_json::from_str(include_str!("../../../../../web/tests/icons_parity.json")).unwrap();
    let icons = Icons::load(&repository_root()).unwrap();
    assert_eq!(icons.script(Some(&["close", "dot", "starred"])).unwrap(), fixture.script);
    for case in fixture.cases {
      assert_eq!(
        icons.svg(&case.key, case.size, &case.cls, &case.attrs).unwrap(),
        case.svg,
        "Rust output for '{}' differs from the parity fixture",
        case.key
      );
    }
  }
}

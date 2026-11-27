//! The Rust sources and HTML assets that build a page or a page fragment a browser receives.
//!
//! `PAGE_SOURCES` is the classifier's list: `web_screenshots::is_web_page_path` counts each path in it as a web page
//! change, so a change to one of these files needs the screenshots that a change to a template needs. The harness
//! check, in the test module at the end of this file, keeps the list complete. It scans the Rust tree, and each
//! `.html` file under `rust/`, outside test code, for the page markers it names, and it fails on each marked file
//! that neither list names. A new Rust page therefore fails the check until it is listed.
//!
//! The markers are in the harness below: an HTML content type or the HTML response helper, a render through the
//! templating engine, markup written in a string literal, the engine's autoescape setting (`AutoEscape`), and the
//! served-form strip of a page's markup, script or style (`without_comments(`). A helper that builds no markup of its
//! own is not a page source: the HTML escape table, the HTML parser and the quoted-string scan that the inline strips
//! share (`registry/src/inlined_literal.rs`) are such helpers.

/// The Rust sources and HTML assets that build a page or a page fragment a browser receives, by path from the
/// repository root.
pub const PAGE_SOURCES: &[&str] = &[
  "rust/crates/app/server/src/auth.rs", // the login page of a refused browser GET
  "rust/crates/app/server/src/login_page.html", // the login page that auth.rs sends
  "rust/crates/app/server/src/routes/pages.rs", // /, /home, /sessions/{session_id}/events
  "rust/crates/features/artifacts/src/artifact_view.rs", // artifact pages under /absolute_filepath/
  "rust/crates/features/artifacts/src/cli/wrap.rs", // the page the artifact wrap verb writes
  "rust/crates/features/artifacts/src/plan_diff.rs", // revision marks of an artifact page (?diff=)
  "rust/crates/features/diff_view/src/page.rs", // /diff
  "rust/crates/features/files/src/document.rs", // the shell of every /absolute_filepath/ page
  "rust/crates/features/files/src/error_page.rs", // the 403 page under /absolute_filepath/
  "rust/crates/features/files/src/internals.rs", // the directory listing under /absolute_filepath/
  "rust/crates/features/files/src/listing.rs", // the listing rows under /absolute_filepath/
  "rust/crates/features/host_auth/src/api.rs", // the 409 wait page of POST /api/host-auth/logins/{name}/start
  "rust/crates/features/host_auth/src/page.rs", // /host-auth
  "rust/crates/features/trace/src/api.rs", // /perfetto and /perfetto/merged
  "rust/crates/features/usage/src/page.rs", // /token-usage
  "rust/crates/features/voice/src/api.rs", // /voice-recordings
  "rust/crates/infra/compat/src/jinja.rs", // the template environment every page renders through
  "rust/crates/infra/compat/src/responses.rs", // the HTML response helper
  "rust/crates/runtime/registry/src/icons.rs", // icon markup of pages and the directory listing
  "rust/crates/runtime/registry/src/inlined_markup.rs", // the served form of every page's markup
  "rust/crates/runtime/registry/src/inlined_script.rs", // the served form of inline scripts
  "rust/crates/runtime/registry/src/inlined_style.rs", // the served form of inline stylesheets
  "rust/crates/runtime/registry/src/templating.rs", // every page render: render_page, page_response
];

/// Whether `path`, a repository-relative path with `/` separators, is a page source the classifier counts.
pub fn is_page_source(path: &str) -> bool {
  PAGE_SOURCES.contains(&path)
}

#[cfg(test)]
mod harness {
  use std::collections::BTreeSet;
  use std::fs;
  use std::path::Path;
  use std::sync::LazyLock;

  use anyhow::{Context, bail};
  use regex::Regex;

  use super::PAGE_SOURCES;

  /// The files that hold a page marker and build no page, each with the reason the harness accepts it. Each entry must
  /// still hold a marker, so the list shrinks when its marker goes.
  const NOT_PAGE_SOURCES: &[(&str, &str)] = &[
    (
      "rust/crates/features/artifacts/src/artifact_check.rs",
      "its probe page is a temporary file that headless Chrome loads from disk, so no server sends it",
    ),
    (
      "rust/crates/features/artifacts/src/plan_verbs.rs",
      "a goal section's text, wrapped as a paragraph, is the input of the text diff that a refusal prints",
    ),
    (
      "rust/crates/features/taste/src/candidates.rs",
      "reads the paragraph and list-item blocks of a taste section and writes no markup",
    ),
  ];

  /// The HTML element names whose tag in a string literal marks markup.
  const ELEMENTS: &[&str] = &[
    "a", "article", "aside", "body", "button", "code", "details", "div", "em", "footer", "form", "h1", "h2", "h3",
    "h4", "h5", "h6", "head", "header", "html", "iframe", "img", "input", "li", "link", "main", "meta", "nav", "ol",
    "p", "pre", "script", "section", "select", "small", "span", "strong", "style", "summary", "svg", "table", "tbody",
    "td", "textarea", "th", "thead", "tr", "ul",
  ];

  /// The marker alternatives other than the element tags, each a regular expression. They name an HTML content type or
  /// the HTML response helper, a render call of the templating engine, a document opening, a class attribute, the
  /// engine's autoescape setting and the served-form strip of page markup.
  const MARKER_ALTERNATIVES: &[&str] = &[
    r"with_utf8_charset\([^\n]*TEXT_HTML",
    r"text/html;",
    r"responses::html\(",
    r"\.(?:render_page|render_page_html|render_page_template|page_response)\(",
    r"(?i:<!doctype html[ >]|<html[ >])",
    r#"class=\\?""#,
    r"AutoEscape",
    r"without_comments\(",
  ];

  /// Every marker in one pattern: the alternatives above and an element tag from `ELEMENTS`, followed by a tag's end.
  static MARKER: LazyLock<Regex> = LazyLock::new(|| {
    let elements = format!("<(?:{})[ />]", ELEMENTS.join("|"));
    let alternatives: Vec<&str> = MARKER_ALTERNATIVES.iter().copied().chain([elements.as_str()]).collect();
    Regex::new(&alternatives.join("|")).expect("the marker alternatives are valid regular expressions")
  });

  /// The inline test module: a column-0 `#[cfg(test)]` line directly before a `mod name {` line.
  static INLINE_TEST_MODULE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^#\[cfg\(test\)\]\nmod [A-Za-z0-9_]+ *\{").expect("the test module pattern is valid")
  });

  /// The directories the scan does not enter: build output and test code.
  const SKIPPED_DIRS: &[&str] = &["target", "tests", "test_support"];

  /// Refuses a tree in which a Rust source or an HTML asset under `rust/` holds a page marker that neither
  /// `page_sources` nor `not_page_sources` names, a listed path that is not a file, and an exempt path that holds no
  /// marker. The lists are `PAGE_SOURCES` and `NOT_PAGE_SOURCES`, or a fixture's own lists.
  pub fn check_page_sources(
    root: &Path,
    page_sources: &[&str],
    not_page_sources: &[(&str, &str)],
  ) -> anyhow::Result<()> {
    let marked = marked_sources(root)?;
    let mut problems = Vec::new();
    for path in &marked {
      let exempt = not_page_sources.iter().any(|(listed, _)| *listed == path.as_str());
      if !exempt && !page_sources.contains(&path.as_str()) {
        problems.push(format!(
          "{path} builds HTML but is in neither PAGE_SOURCES nor NOT_PAGE_SOURCES; list it in PAGE_SOURCES if it \
           renders a page or a fragment a browser receives"
        ));
      }
    }
    for path in page_sources {
      if !root.join(path).is_file() {
        problems.push(format!("{path} is listed in PAGE_SOURCES but is not a file"));
      }
    }
    for (path, _) in not_page_sources {
      if !marked.contains(*path) {
        problems.push(format!("{path} is in NOT_PAGE_SOURCES but holds no page marker"));
      }
    }
    if problems.is_empty() {
      return Ok(());
    }
    bail!("the page source list is out of date:\n  {}", problems.join("\n  "))
  }

  /// The repository-relative paths under `root/rust` that hold a page marker, outside the skipped directories.
  fn marked_sources(root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut marked = BTreeSet::new();
    let mut pending = vec![root.join("rust")];
    while let Some(dir) = pending.pop() {
      let entries = fs::read_dir(&dir).with_context(|| format!("cannot read {}", dir.display()))?;
      for entry in entries {
        let path = entry.with_context(|| format!("cannot read {}", dir.display()))?.path();
        let name = path.file_name().expect("a directory entry has a name").to_string_lossy().into_owned();
        if path.is_dir() {
          if !SKIPPED_DIRS.contains(&name.as_str()) {
            pending.push(path);
          }
          continue;
        }
        let holds_marker = match path.extension().and_then(|extension| extension.to_str()) {
          Some("rs") if !name.ends_with("tests.rs") && !name.ends_with("test_support.rs") => {
            let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
            marks_html(&text)
          }
          Some("html") => true,
          _ => false,
        };
        if holds_marker {
          marked.insert(relative_path(root, &path));
        }
      }
    }
    Ok(marked)
  }

  fn relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).expect("a scanned path lies under the root");
    relative.components().map(|part| part.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
  }

  /// Whether a file's code holds a page marker. The code ends where the inline test module starts.
  fn marks_html(text: &str) -> bool {
    let code = match INLINE_TEST_MODULE.find(text) {
      Some(module) => &text[..module.start()],
      None => text,
    };
    MARKER.find_iter(code).any(|marker| is_code(code, marker.start()))
  }

  /// Whether a marker at `start` is code: its line is not a comment, and it is not the `?P<` of a regex named group.
  fn is_code(code: &str, start: usize) -> bool {
    let line_start = code[..start].rfind('\n').map_or(0, |newline| newline + 1);
    !code[line_start..start].trim_start().starts_with("//") && !code[..start].ends_with("?P")
  }

  /// The repository root: the checkout that holds this crate, four levels above its crate directory.
  fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate lies in a repository")
  }

  /// Writes each `(path, text)` under a fresh repository root and returns the root.
  fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("a temporary directory");
    for (path, text) in files {
      let file = root.path().join(path);
      fs::create_dir_all(file.parent().expect("a file has a parent")).expect("the directory is created");
      fs::write(file, text).expect("the file is written");
    }
    root
  }

  const PAGE: &str = "pub fn answer() -> Response {\n  responses::html(String::from(\"<p>hello</p>\"))\n}\n";

  #[test]
  fn a_rust_source_that_answers_html_fails_until_it_is_listed() {
    let root = tree(&[("rust/crates/x/src/page.rs", PAGE)]);

    let refusal = check_page_sources(root.path(), &[], &[]).unwrap_err().to_string();

    assert!(refusal.contains("rust/crates/x/src/page.rs builds HTML"), "{refusal}");
    check_page_sources(root.path(), &["rust/crates/x/src/page.rs"], &[]).expect("a listed page passes");
  }

  #[test]
  fn plain_rust_test_code_and_comments_are_not_pages() {
    let test_only =
      "#[cfg(test)]\nmod tests {\n  fn page() -> Response {\n    responses::html(String::new())\n  }\n}\n";
    let root = tree(&[
      ("rust/crates/x/src/lib.rs", "pub fn plain() -> u8 {\n  1\n}\n"),
      ("rust/crates/x/src/inline.rs", test_only),
      ("rust/crates/x/src/page_tests.rs", PAGE),
      ("rust/crates/x/src/tests/fixtures.rs", PAGE),
      ("rust/crates/x/tests/data/page.html", "<html></html>\n"),
      ("rust/crates/x/src/commented.rs", "// responses::html(String::new())\n"),
    ]);

    check_page_sources(root.path(), &[], &[]).expect("no page marker outside test code");
  }

  #[test]
  fn an_exempt_source_must_still_hold_a_marker() {
    let root = tree(&[("rust/crates/x/src/plain.rs", "pub fn plain() -> u8 {\n  1\n}\n")]);

    let refusal = check_page_sources(root.path(), &[], &[("rust/crates/x/src/plain.rs", "no longer builds HTML")])
      .unwrap_err()
      .to_string();

    assert!(refusal.contains("rust/crates/x/src/plain.rs is in NOT_PAGE_SOURCES but holds no page marker"));
  }

  #[test]
  fn the_repository_lists_every_page_source() {
    check_page_sources(repository_root(), PAGE_SOURCES, NOT_PAGE_SOURCES).expect("the tree's page sources are listed");
  }
}

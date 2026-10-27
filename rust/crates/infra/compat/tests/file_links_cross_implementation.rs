//! The file-link scanner against Python's `src/runtime/file_urls.py` on one synthetic case matrix.
//!
//! The test builds a synthetic home, session directory and user home, writes the case matrix as JSON, and asks
//! `file_links_python.py` for Python's findings and detail text. `scan_file_links` and `dead_links_detail` answer the
//! same cases here. Every answer must match, and the matrix must reach each form, live and dead, with and without a
//! correct link.
//!
//! Run with `cargo test -p charliebot-compat --test file_links_cross_implementation`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_compat::file_urls::{
  FILE_SERVER_MOUNT, FileLink, FileLinkForm, ScanContext, dead_links_detail, scan_file_links,
};
use serde_json::{Value, json};

/// The finding shapes the matrix must reach: the form, its liveness, and whether it has a correct link.
const COVERED: [(FileLinkForm, Option<bool>, bool); 6] = [
  (FileLinkForm::Canonical, Some(true), false),
  (FileLinkForm::Canonical, Some(false), false),
  (FileLinkForm::Canonical, None, false),
  (FileLinkForm::RetiredPrefix, Some(false), true),
  (FileLinkForm::RetiredPrefix, Some(false), false),
  (FileLinkForm::LocalPath, Some(false), true),
];

/// One case of the matrix: a text and the context that resolves its links.
struct Case {
  text: String,
  context: Option<ScanContext>,
}

fn case(text: impl Into<String>, context: Option<&ScanContext>) -> Case {
  Case { text: text.into(), context: context.cloned() }
}

fn write_page(path: &Path) {
  fs::create_dir_all(path.parent().expect("a page has a parent directory")).unwrap();
  fs::write(path, "page").unwrap();
}

/// The synthetic tree under `root`: a retired page in the home, a session page, a live page in `live/`, and no file at
/// `missing/page.html`. Returns the context the cases share.
fn build_tree(root: &Path) -> ScanContext {
  let context = ScanContext {
    server_port: 18498,
    charliebot_home: root.join("charliebot-home"),
    session_dir: root.join("sessions").join("synthetic-session"),
    user_home: root.join("user-home"),
  };
  write_page(&context.charliebot_home.join("sessions/synthetic-session/plan.html"));
  write_page(&context.session_dir.join("artifacts/plan.html"));
  write_page(&root.join("live/page.html"));
  context
}

/// The case matrix. The live and missing pages sit under `root`; the retired URLs name the home's synthetic session.
fn cases(root: &Path, context: &ScanContext) -> Vec<Case> {
  let root = root.display();
  let live = format!("http://localhost:18498{FILE_SERVER_MOUNT}{root}/live/page.html");
  let live_dir = format!("http://localhost:18498{FILE_SERVER_MOUNT}{root}/live");
  let missing = format!("http://localhost:18498{FILE_SERVER_MOUNT}{root}/missing/page.html");
  let retired = "https://charliebot.example:18498/files/sessions/synthetic-session/plan.html";
  let retired_missing = "https://charliebot.example:18498/files/sessions/synthetic-session/missing.html";
  let cross_origin = format!("https://charliebot.example{FILE_SERVER_MOUNT}{root}/missing/page.html");
  let some = Some(context);
  vec![
    case(format!("see {live}"), some),
    case(format!("see {live_dir}"), some),
    case(format!("see {missing}"), some),
    case(format!("see {cross_origin}"), some),
    case(format!("see {missing}"), None),
    case(format!("{live}. {live}, {missing}; {live}。"), some),
    case(format!("[page]({live})"), some),
    case(format!("[page]({missing})"), None),
    case(format!("http://localhost:١٨٤٩٨{FILE_SERVER_MOUNT}{root}/missing/page.html"), some),
    case(format!("see {retired}"), some),
    case(format!("see {retired_missing}"), some),
    case(format!("see {retired}"), None),
    case(format!("[old]({retired})"), None),
    case("[old](/files/home/synthetic-user/source.rs)", some),
    case("[plan](artifacts/plan.html)", some),
    case("[script](~/scripts/x.sh)", some),
    case(format!("[page]({root}//missing/./page.md)"), some),
    case("[plan](artifacts/plan.html)", None),
    case("[x](/synthetic-missing-root/x.md)", some),
    case("[route](/diff/x) [api](/api/sessions) [query](/?session=synthetic-id)", some),
    case("https://code.example/o/r/pull/1/files", some),
    case("intro\n[plan]: artifacts/plan.html", some),
    case("[plan](<artifacts/plan.html>)", some),
    case("https://charliebot.example:18499/files/x", some),
    case("/files/sessions/synthetic-session/plan.html", some),
  ]
}

fn context_json(context: &ScanContext) -> Value {
  json!({
    "server_port": context.server_port,
    "charliebot_home": context.charliebot_home,
    "session_dir": context.session_dir,
    "user_home": context.user_home,
  })
}

fn case_json(case: &Case) -> Value {
  json!({"text": case.text, "context": case.context.as_ref().map(context_json)})
}

fn form_name(form: FileLinkForm) -> &'static str {
  match form {
    FileLinkForm::Canonical => "canonical",
    FileLinkForm::RetiredPrefix => "retired_prefix",
    FileLinkForm::LocalPath => "local_path",
  }
}

fn finding_json(finding: &FileLink) -> Value {
  json!({
    "link": finding.link,
    "form": form_name(finding.form),
    "path": finding.path,
    "live": finding.live,
    "correct_link": finding.correct_link,
  })
}

/// Runs `file_links_python.py` on the cases file; it writes the answers to `answers`.
fn run_python(cases: &Path, answers: &Path) {
  let repo = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate sits in the repository");
  let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/file_links_python.py");
  let output = Command::new("uv")
    .args(["run", "python"])
    .arg(&script)
    .arg(cases)
    .arg(answers)
    .current_dir(repo)
    .output()
    .expect("running uv for the Python scanner");
  assert!(output.status.success(), "file_links_python.py failed:\n{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn the_file_link_scanner_matches_python_on_the_case_matrix() {
  let root = tempfile::tempdir().unwrap();
  let context = build_tree(root.path());
  let cases = cases(root.path(), &context);
  let cases_path: PathBuf = root.path().join("cases.json");
  let answers_path: PathBuf = root.path().join("python-answers.json");
  let cases_json: Vec<Value> = cases.iter().map(case_json).collect();
  fs::write(&cases_path, serde_json::to_string_pretty(&cases_json).unwrap()).unwrap();
  run_python(&cases_path, &answers_path);
  let python: Vec<Value> = serde_json::from_slice(&fs::read(&answers_path).unwrap()).unwrap();
  assert_eq!(python.len(), cases.len(), "file_links_python.py answered a different number of cases");

  let mut rust = Vec::new();
  let mut reached = Vec::new();
  for case in &cases {
    let findings = scan_file_links(&case.text, case.context.as_ref()).unwrap();
    reached.extend(findings.iter().map(|finding| (finding.form, finding.live, finding.correct_link.is_some())));
    rust.push(json!({
      "findings": findings.iter().map(finding_json).collect::<Vec<_>>(),
      "detail": dead_links_detail(&findings),
    }));
  }

  let mut differences = Vec::new();
  for (index, (case, (python, rust))) in cases.iter().zip(python.iter().zip(&rust)).enumerate() {
    if python != rust {
      differences.push(format!("case {index} {:?}\n  python {python}\n  rust   {rust}", case.text));
    }
  }
  assert!(differences.is_empty(), "{} of {} cases differ:\n{}", differences.len(), cases.len(), differences.join("\n"));
  for shape in COVERED {
    assert!(reached.contains(&shape), "the matrix never reaches {shape:?}");
  }
}

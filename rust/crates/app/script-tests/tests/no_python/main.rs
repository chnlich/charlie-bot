//! The no-Python tree guard: `dev/check-no-python.sh` fails on a tracked .py file or a Python shebang. It also fails
//! on a line that names python in a shell script, git hook, CI configuration, prompt, skill, doc or Markdown file.
//! The guard passes a clean tree, prose that names the language with a capital P, and the exempted paths.

use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};

const PGREP_EXAMPLE: &str = r#"Anchor the pattern, for example `pgrep -fc "^python3 -c while True"`."#;

fn guard() -> PathBuf {
  harness::repo_root().join("dev/check-no-python.sh")
}

fn git(env: &Env, cwd: &Path, args: &[&str]) {
  let mut command = harness::command("git", env);
  command.args(args);
  let run = run_in(&mut command, cwd);
  assert_eq!(run.code, 0, "git {args:?}: {}", run.describe());
}

/// Git identity and config pinned, as the other script tests pin them for the commits they make.
fn env_for(sandbox: &Sandbox) -> Env {
  let mut env = sandbox.env();
  env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
  env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
  for name in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
    env.insert(name.to_string(), "Example".to_string());
  }
  for name in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
    env.insert(name.to_string(), "123+example@users.noreply.github.com".to_string());
  }
  env
}

fn write(repo: &Path, path: &str, body: &str) {
  let file = repo.join(path);
  std::fs::create_dir_all(file.parent().unwrap()).unwrap();
  std::fs::write(file, body).unwrap();
}

/// A committed tree whose only Python text is prose with a capital P: the clean case the guard must pass.
fn repo(sandbox: &Sandbox, env: &Env) -> PathBuf {
  let repo = sandbox.dir("repo");
  git(env, sandbox.root(), &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
  write(&repo, "README.md", "A clean checkout.\n");
  write(&repo, "dev/note.sh", "#!/usr/bin/env bash\n# The Python program is gone.\ntrue\n");
  git(env, &repo, &["add", "."]);
  git(env, &repo, &["commit", "-q", "-m", "seed commit"]);
  repo
}

fn run_in(command: &mut Command, cwd: &Path) -> Run {
  command.current_dir(cwd);
  harness::run(command)
}

fn run_guard(repo: &Path) -> Run {
  run_in(&mut Command::new(guard()), repo)
}

fn stage(repo: &Path, env: &Env, path: &str, body: &str) {
  write(repo, path, body);
  git(env, repo, &["add", path]);
}

fn fails_naming(run: &Run, path: &str) {
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains(path), "{}: the output does not name {path}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_clean_tree_passes() {
  let sandbox = Sandbox::new();
  let env = env_for(&sandbox);
  let repo = repo(&sandbox, &env);
  let run = run_guard(&repo);
  assert_eq!(run.code, 0, "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_tracked_python_file_fails() {
  let sandbox = Sandbox::new();
  let env = env_for(&sandbox);
  let repo = repo(&sandbox, &env);
  stage(&repo, &env, "dev/improve/evil.py", "x = 1\n");
  fails_naming(&run_guard(&repo), "dev/improve/evil.py");
}

#[test]
#[ignore = "integration"]
fn a_python_shebang_fails() {
  let sandbox = Sandbox::new();
  let env = env_for(&sandbox);
  let repo = repo(&sandbox, &env);
  stage(&repo, &env, "scripts/run", "#!/usr/bin/env python3\ntrue\n");
  fails_naming(&run_guard(&repo), "scripts/run");
}

#[test]
#[ignore = "integration"]
fn a_script_or_ci_line_that_runs_python_fails() {
  let sandbox = Sandbox::new();
  let env = env_for(&sandbox);
  let repo = repo(&sandbox, &env);
  stage(&repo, &env, "dev/gen.sh", "#!/usr/bin/env bash\npython3 -m compileall .\n");
  stage(&repo, &env, ".github/workflows/ci.yml", "steps:\n  - run: python3 dev/gen.sh\n");
  let run = run_guard(&repo);
  fails_naming(&run, "dev/gen.sh");
  fails_naming(&run, ".github/workflows/ci.yml");
}

#[test]
#[ignore = "integration"]
fn a_prompts_skills_or_docs_line_that_runs_python_fails() {
  let sandbox = Sandbox::new();
  let env = env_for(&sandbox);
  let repo = repo(&sandbox, &env);
  stage(&repo, &env, "prompts/example.md", "Run `python3 -c 'print(1)'` to test.\n");
  stage(&repo, &env, "docs/guide.md", "Generate the table:\n\n    python3 dev/gen.sh\n");
  let run = run_guard(&repo);
  fails_naming(&run, "prompts/example.md");
  fails_naming(&run, "docs/guide.md");
}

#[test]
#[ignore = "integration"]
fn an_exempt_path_passes() {
  let sandbox = Sandbox::new();
  let env = env_for(&sandbox);
  let repo = repo(&sandbox, &env);
  stage(&repo, &env, "skills/cli-tool-traps/SKILL.md", &format!("{PGREP_EXAMPLE}\n"));
  let run = run_guard(&repo);
  assert_eq!(run.code, 0, "{}", run.describe());
}

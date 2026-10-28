//! The release's test world: one throwaway repository, release root, log root, stub directory and artifact fixture
//! under a sandbox of stub commands. Each test builds its own, so nothing touches ~/charliebot-releases or the live
//! home. A stub `gh` answers the authentication, repository, run and artifact queries from files beside it, and
//! CHARLIEBOT_RELEASE_TRIAL and CHARLIEBOT_RELEASE_REHEARSAL name stub stage commands, so the preflight refusals and
//! the stage verdicts run without downloading a binary or starting a server.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};
use serde_json::json;
use sha2::{Digest as _, Sha256};

/// The repository slug the stub gh reports for `repo view`.
pub const SLUG: &str = "example/charlie-bot";

const GIT_IDENTITY: [(&str, &str); 4] = [
  ("GIT_AUTHOR_NAME", "release-test"),
  ("GIT_AUTHOR_EMAIL", "release-test@example.invalid"),
  ("GIT_COMMITTER_NAME", "release-test"),
  ("GIT_COMMITTER_EMAIL", "release-test@example.invalid"),
];

/// One test's throwaway repository, release root, log root, artifact fixture and stub CI behavior. The repository has
/// one commit on `origin/main` and one commit off it.
pub struct World {
  pub sandbox: Sandbox,
  pub repo: PathBuf,
  pub releases: PathBuf,
  /// The stage logs' root. A preflight refusal leaves it absent.
  pub logs: PathBuf,
  pub artifact: PathBuf,
  /// The commit that `origin/main` names.
  pub on_main: String,
  /// One commit on top of `on_main`.
  pub off_main: String,
  /// The variables the release script sees.
  pub env: Env,
}

/// The stub gh's answers. `runs` is the JSON array `run list` prints, and each `jobs` entry is the JSON array that
/// `run view <id>` wraps in `{"jobs": ...}`.
pub struct GhBehavior {
  pub auth: bool,
  pub runs: String,
  pub jobs: BTreeMap<String, String>,
  pub download_ok: bool,
  pub artifact_dir: PathBuf,
}

impl World {
  pub fn new() -> Self {
    let sandbox = Sandbox::new();
    let repo = sandbox.dir("repo");
    let scripts = sandbox.dir("repo/scripts");
    let releases = sandbox.dir("releases");
    let logs = sandbox.root().join("logs");
    let artifact = sandbox.dir("artifact");
    let home = sandbox.dir("home");
    harness::copy_file(&harness::repo_root().join("scripts/release.sh"), &scripts.join("release.sh"));
    fs::write(repo.join("README.md"), "release test\n").expect("the README");

    let mut env = sandbox.env();
    for (name, value) in GIT_IDENTITY {
      env.insert(name.to_string(), value.to_string());
    }
    // Unset ~/.gitconfig and any user identity: the commits are the fixture's own.
    env.insert("HOME".to_string(), home.display().to_string());
    git(&env, &repo, &["init", "-q", "-b", "main"]);
    git(&env, &repo, &["add", "."]);
    git(&env, &repo, &["commit", "-q", "-m", "the release commit"]);
    let on_main = git(&env, &repo, &["rev-parse", "HEAD"]);
    git(&env, &repo, &["update-ref", "refs/remotes/origin/main", &on_main]);
    git(&env, &repo, &["commit", "-q", "--allow-empty", "-m", "a commit off origin/main"]);
    let off_main = git(&env, &repo, &["rev-parse", "HEAD"]);

    install_artifacts(&artifact);
    let gh = sandbox.bin().join("gh");
    harness::copy_file(&stub_file("gh"), &gh);
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("make the gh stub executable");
    for (name, code) in [("stage-ok", 0), ("stage-fail", 3)] {
      sandbox.stub(name, &format!("exit {code}\n"));
    }
    fs::create_dir(releases.join("prior")).expect("the prior release");
    symlink("prior", releases.join("current")).expect("the current link");

    env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), releases.display().to_string());
    env.insert("CHARLIEBOT_RELEASE_LOG_DIR".to_string(), logs.display().to_string());
    env.insert("CHARLIEBOT_RELEASE_TRIAL".to_string(), sandbox.bin().join("stage-ok").display().to_string());
    env.insert("CHARLIEBOT_RELEASE_REHEARSAL".to_string(), sandbox.bin().join("stage-ok").display().to_string());

    let world = Self { sandbox, repo, releases, logs, artifact, on_main, off_main, env };
    world.write_gh(&world.gh_defaults());
    world
  }

  /// The stub directory, first on PATH.
  pub fn bin(&self) -> &Path {
    self.sandbox.bin()
  }

  /// The defaults the stub gh answers: authenticated, the test slug, one successful push run of commit 7 with both
  /// Rust jobs green, and a passing artifact download.
  pub fn gh_defaults(&self) -> GhBehavior {
    let mut jobs = BTreeMap::new();
    jobs.insert(
      "7".to_string(),
      json!([
        {"name": "release-build", "conclusion": "success"},
        {"name": "rust-test", "conclusion": "success"},
      ])
      .to_string(),
    );
    GhBehavior {
      auth: true,
      runs: json!([{
        "databaseId": 7,
        "event": "push",
        "headSha": self.on_main,
        "conclusion": "success",
      }])
      .to_string(),
      jobs,
      download_ok: true,
      artifact_dir: self.artifact.clone(),
    }
  }

  /// Writes `behavior` as the files the stub gh reads beside itself.
  pub fn write_gh(&self, behavior: &GhBehavior) {
    let dir = self.bin().join("gh_behavior");
    fs::create_dir_all(&dir).expect("the gh behavior directory");
    fs::write(dir.join("auth"), if behavior.auth { "1\n" } else { "0\n" }).expect("the auth answer");
    fs::write(dir.join("slug"), format!("{SLUG}\n")).expect("the slug answer");
    fs::write(dir.join("runs"), format!("{}\n", behavior.runs)).expect("the runs answer");
    for (id, jobs) in &behavior.jobs {
      fs::write(dir.join(format!("jobs-{id}")), format!("{{\"jobs\": {jobs}}}\n")).expect("a jobs answer");
    }
    fs::write(dir.join("download_ok"), if behavior.download_ok { "1\n" } else { "0\n" }).expect("the download answer");
    fs::write(dir.join("artifact_dir"), format!("{}\n", behavior.artifact_dir.display()))
      .expect("the artifact directory answer");
  }

  /// Runs the repository's release script for `commit`.
  pub fn release(&self, commit: &str) -> Run {
    let mut command = harness::command("bash", &self.env);
    command.arg(self.repo.join("scripts/release.sh")).arg(commit);
    harness::run(&mut command)
  }

  /// The refusal contract shared by the preflight refusals: exit 2, no release directory, and no log directory.
  pub fn assert_refused_and_wrote_nothing(&self, run: &Run) {
    assert_eq!(run.code, 2, "{}", run.describe());
    assert!(!self.releases.join(&self.on_main).exists(), "{}", run.describe());
    assert!(!self.logs.exists(), "the refusal wrote {}", tree(&self.logs));
  }

  /// The one stage log named `name` under the log root. A run that names its stage log wrote it there.
  pub fn stage_log(&self, name: &str) -> PathBuf {
    for entry in fs::read_dir(&self.logs).expect("the log root") {
      let log = entry.expect("a log entry").path().join(name);
      if log.is_file() {
        return log;
      }
    }
    panic!("no {name} under {}", self.logs.display());
  }

  /// The target that the `current` link names.
  pub fn current_link(&self) -> PathBuf {
    fs::read_link(self.releases.join("current")).expect("the current link")
  }

  /// Whether an install's temporary directory survives under the release root.
  pub fn has_temporary_release(&self) -> bool {
    let prefix = format!(".{}.", self.on_main);
    fs::read_dir(&self.releases)
      .expect("the release root")
      .any(|entry| entry.expect("a release entry").file_name().to_string_lossy().starts_with(&prefix))
  }
}

impl Default for World {
  fn default() -> Self {
    Self::new()
  }
}

/// The artifact fixture: both fake binaries and the checksums `sha256sum -c` verifies.
fn install_artifacts(artifact: &Path) {
  let binaries = [("charliebot", "fake CLI binary\n"), ("charliebot-server", "fake server binary\n")];
  let mut checksums = String::new();
  for (name, contents) in binaries {
    let binary = artifact.join(name);
    fs::write(&binary, contents).expect("an artifact binary");
    checksums.push_str(&format!("{}  {name}\n", sha256_hex(&binary)));
  }
  fs::write(artifact.join("sha256sums.txt"), checksums).expect("the artifact checksums");
}

/// A file beside this test target, such as the gh stub.
fn stub_file(name: &str) -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/release/stubs").join(name)
}

/// A git command in `cwd` with the fixture's identity. A failing command fails the test.
fn git(env: &Env, cwd: &Path, args: &[&str]) -> String {
  let mut command = harness::command("git", env);
  command.arg("-C").arg(cwd).args(args);
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "git {args:?} in {}: {}", cwd.display(), run.describe());
  run.stdout.trim().to_string()
}

fn sha256_hex(path: &Path) -> String {
  let bytes = fs::read(path).expect("an artifact file");
  format!("{:x}", Sha256::digest(bytes))
}

/// Every path under `root`, for an assertion message that names what a refusal wrote.
fn tree(root: &Path) -> String {
  if !root.exists() {
    return "(nothing)".to_string();
  }
  let mut paths = Vec::new();
  collect(root, &mut paths);
  paths.join(", ")
}

fn collect(dir: &Path, paths: &mut Vec<String>) {
  for entry in fs::read_dir(dir).expect("a directory") {
    let path = entry.expect("an entry").path();
    paths.push(path.display().to_string());
    if path.is_dir() {
      collect(&path, paths);
    }
  }
}

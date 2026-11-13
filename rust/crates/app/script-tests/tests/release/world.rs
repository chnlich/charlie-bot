//! The release's test world: one throwaway repository, release root, log root, stub directory and artifact fixture
//! under a sandbox of stub commands. Each test builds its own, so nothing touches ~/charliebot-releases or the live
//! home. A stub `gh` answers the authentication, repository, run and artifact queries from files beside it. A stub
//! `systemctl` keeps the user manager untouched, so the preflight refusals and the stage verdicts run without
//! downloading a real binary.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

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
  pub runtime: PathBuf,
  /// The temporary directory the script creates its artifact directory in: this world's TMPDIR, so one test's scan
  /// never sees another test's directories.
  pub tmp: PathBuf,
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
    let runtime = sandbox.dir("runtime");
    let home = sandbox.dir("home");
    harness::copy_file(&harness::repo_root().join("scripts/release.sh"), &scripts.join("release.sh"));
    // release.sh sources the host release lock from beside itself.
    fs::create_dir(scripts.join("release")).expect("the release scripts directory");
    harness::copy_file(&harness::repo_root().join("scripts/release/lock.sh"), &scripts.join("release/lock.sh"));
    // The units stage installs the commit's units, read from this repository's commit object.
    harness::copy_file(
      &harness::repo_root().join("scripts/install-slice-units.sh"),
      &scripts.join("install-slice-units.sh"),
    );
    harness::copy_file(&harness::repo_root().join("scripts/systemd-user.sh"), &scripts.join("systemd-user.sh"));
    let systemd = scripts.join("systemd");
    fs::create_dir_all(&systemd).expect("the unit directory");
    for unit in fs::read_dir(harness::repo_root().join("scripts/systemd")).expect("the unit files") {
      let unit = unit.expect("a unit file");
      harness::copy_file(&unit.path(), &systemd.join(unit.file_name()));
    }
    fs::write(repo.join("README.md"), "release test\n").expect("the README");
    // The user manager is never touched: its reload lands in the unit-calls log beside the sandbox.
    sandbox.stub("systemctl", "printf '%s\\t%s\\n' \"${DBUS_SESSION_BUS_ADDRESS-}\" \"$*\" >> \"$UNIT_CALLS\"\n");

    let mut env = sandbox.env();
    let tmp = sandbox.dir("tmp");
    env.insert("TMPDIR".to_string(), tmp.display().to_string());
    env.insert("UNIT_CALLS".to_string(), sandbox.root().join("unit-calls.log").display().to_string());
    env.insert("CHARLIEBOT_SYSTEMD_USER_DIR".to_string(), sandbox.dir("user-units").display().to_string());
    env.insert("XDG_RUNTIME_DIR".to_string(), runtime.display().to_string());
    env.insert("DBUS_SESSION_BUS_ADDRESS".to_string(), "disabled:".to_string());
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
    fs::create_dir(releases.join("prior")).expect("the prior release");
    symlink("prior", releases.join("current")).expect("the current link");

    env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), releases.display().to_string());
    env.insert("CHARLIEBOT_RELEASE_LOG_DIR".to_string(), logs.display().to_string());
    // The host release lock lives under the sandbox, never beside the user's home.
    env.insert("CHARLIEBOT_RELEASE_LOCK".to_string(), sandbox.root().join("release.lock").display().to_string());

    let world = Self { sandbox, repo, releases, logs, artifact, runtime, tmp, on_main, off_main, env };
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

  /// Every file and directory name under the log root, at any depth.
  pub fn log_names(&self) -> Vec<String> {
    let mut names = Vec::new();
    collect_names(&self.logs, &mut names);
    names
  }

  /// The entries under this world's TMPDIR: the artifact temporary directories the script creates and removes.
  pub fn artifact_temp_entries(&self) -> Vec<String> {
    match fs::read_dir(&self.tmp) {
      Ok(entries) => {
        entries.map(|entry| entry.expect("a tmp entry").file_name().into_string().expect("a name")).collect()
      }
      Err(_) => Vec::new(),
    }
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

  /// Makes a version directory named `commit` whose install time is `installed_at`, in Unix seconds. The prune reads
  /// the install time from the directory's mtime, so the fixture sets it.
  pub fn version_dir(&self, commit: &str, installed_at: u64) -> PathBuf {
    let dir = self.releases.join(commit);
    fs::create_dir(&dir).expect("a version directory");
    let time = UNIX_EPOCH + Duration::from_secs(installed_at);
    fs::File::open(&dir)
      .and_then(|handle| handle.set_times(fs::FileTimes::new().set_modified(time)))
      .expect("the version directory's install time");
    dir
  }

  /// Points `current` at `target`, a name under the release root.
  pub fn point_current_at(&self, target: &str) {
    fs::remove_file(self.releases.join("current")).expect("the current link");
    symlink(target, self.releases.join("current")).expect("the current link");
  }
}

impl Default for World {
  fn default() -> Self {
    Self::new()
  }
}

/// The artifact fixture: both fake binaries, executable as the CI build's are, and the checksums `sha256sum -c`
/// verifies.
fn install_artifacts(artifact: &Path) {
  let binaries = [("charliebot", "fake CLI binary\n"), ("charliebot-server", "fake server binary\n")];
  let mut checksums = String::new();
  for (name, contents) in binaries {
    let binary = artifact.join(name);
    fs::write(&binary, contents).expect("an artifact binary");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("the artifact binary mode");
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

/// Every file and directory name under `root`, at any depth, in directory order.
fn collect_names(dir: &Path, names: &mut Vec<String>) {
  if !dir.exists() {
    return;
  }
  for entry in fs::read_dir(dir).expect("a directory") {
    let path = entry.expect("an entry").path();
    names.push(path.file_name().expect("a name").to_string_lossy().into_owned());
    if path.is_dir() {
      collect_names(&path, names);
    }
  }
}

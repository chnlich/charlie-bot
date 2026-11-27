//! One improve run: `dev/improve/remote-launch.sh` on this host. A round then runs
//! `dev/improve/remote-runner.sh` on the remote host. A fake `ssh` records its arguments and runs the remote side
//! locally. Fake `claude`, `codex`, `systemd-run`, `systemctl` and `cargo` record what they receive, so each test checks
//! the run's contract without a network, a server or a real remote host.
//!
//! The tests that assert a bound on time measure it: the approval preflight must finish inside five seconds.

mod world;

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use charliebot_shell_harness::{self as harness, Sandbox};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use world::{
  CODEX_REFRESH_TOKEN, DIRECTION, GATEWAY_TOKEN, LOCAL_CLAUDE_VERSION, LOGIN_REFRESH_TOKEN, LOGIN_TOKEN, NEWER_RULE,
  PINNED_RULE, REVIEW_CREDENTIAL, REVIEW_KEY, REVIEW_LOGIN_TOKEN, SUBSCRIPTION_REVIEW_TYPE, SYNTHETIC_PROBE_ANSWER,
  World,
};

/// The prompt the server writes to the launcher's standard input for a plain-text run.
const PROMPT: &str = "Improve one topic in the performance direction.\nReply with the word OK.\n";

fn stderr(run: &harness::Run) -> String {
  run.stderr.clone()
}

/// Whether the list names `path`, bare or with the branch that carries it.
fn lists_path(list: &str, path: &str) -> bool {
  list.lines().any(|line| line == path || line.starts_with(&format!("{path}\t")))
}

/// Whether `condition` holds within `limit`, polled until it does.
fn wait_until(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
  let start = Instant::now();
  while start.elapsed() < limit {
    if condition() {
      return true;
    }
    std::thread::sleep(Duration::from_millis(20));
  }
  condition()
}

/// Removes the evidence directory of the newest round that `world` launched for DIRECTION. Two launches in one UTC minute
/// name one evidence directory, which the runner refuses to reuse, so a test that launches again calls this between the
/// launches, as a round started in a later minute would find no directory of its stamp.
fn remove_newest_round_evidence(world: &World) {
  let root = world.remote_home.join("improve").join(DIRECTION).join("evidence");
  // A round's directory is named by its start stamp, which begins with the date; other entries are not rounds.
  let newest = fs::read_dir(&root)
    .expect("the round's evidence root")
    .map(|entry| entry.expect("an evidence entry").path())
    .filter(|path| path.is_dir() && path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("2")))
    .max()
    .expect("the first round left an evidence directory");
  fs::remove_dir_all(&newest).expect("the first round's evidence directory");
}

/// The launcher resolves the endpoint member of `backends.calls.probe` on its host and ships the answer with the
/// launch. A host that runs no server has no other way to resolve it, so the round's environment names that answer, and
/// the round's agent reads the answer the launcher shipped.
#[test]
#[ignore = "integration"]
fn the_round_reads_the_probe_answer_the_launcher_shipped() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  assert_eq!(world.claude_probe_answer(), SYNTHETIC_PROBE_ANSWER);
}

#[test]
#[ignore = "integration"]
fn the_round_receives_claudes_arguments_with_the_launch_directory_paths() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());

  let received = world.claude_arguments_received();
  let position = received
    .iter()
    .position(|argument| argument == "--append-system-prompt-file")
    .unwrap_or_else(|| panic!("no --append-system-prompt-file in {received:?}"));
  let copied = std::path::Path::new(&received[position + 1]);
  assert!(copied.starts_with(world.remote_home.join("improve/launch")), "{received:?}");
  assert_eq!(copied.file_name().map(|name| name.to_string_lossy().into_owned()), Some("instructions.md".to_string()));
  assert_ne!(received[position + 1], world.instructions.display().to_string());
  assert!(world.launch_directory().is_none(), "the runner left its launch directory behind");
  assert!(
    world.ssh_arguments().contains(&world.commit),
    "the runner was sent no CLI commit: {}",
    world.ssh_arguments()
  );
  assert!(received.iter().all(|argument| argument != "--mcp-config"), "{received:?}");
  assert!(received.windows(2).any(|pair| pair == ["--output-format", "stream-json"]), "{received:?}");
  assert!(
    received.windows(2).any(|pair| pair == ["--settings", "{\"disableClaudeAiConnectors\":true}"]),
    "{received:?}"
  );
}

#[test]
#[ignore = "integration"]
fn the_prompt_on_the_launchers_standard_input_reaches_the_remote_claude() {
  let world = World::new();
  let prompt = world.sandbox.root().join("prompt.txt");
  fs::write(&prompt, PROMPT).expect("the prompt");
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_STDIN".to_string(), world.claude_stdin.display().to_string());
  // The launcher's standard input is a file here, as the server's pipe is in production. Every process that inherits
  // it shares one offset, so a process that reads the prompt before the round's own ssh leaves that ssh nothing.
  let stdin = fs::File::open(&prompt).expect("open the prompt");
  let mut command = world.launcher_command(&env);
  let run = harness::run_with_stdin(&mut command, Stdio::from(stdin));
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let text = fs::read_to_string(&world.claude_stdin).expect("the fake claude recorded its standard input");
  assert_eq!(text, PROMPT, "the remote claude did not receive the prompt:\n{}", run.describe());
}

/// The Skills Discovery section of the instructions a round receives (prompts/task_base.md renders it).
const SKILLS_DISCOVERY: &str = "- Look in `~/.claude/skills/` or `~/.agents/skills/`: both symlink the two canonical \
  sources, the charlie-bot checkout's `skills/` (general, writing-style included) and `$CHARLIEBOT_HOME/skills/`.\n";
/// The genre paths of the worker workflow (prompts/worker.md renders them).
const WORKER_GENRES: &str = concat!(
  "2. Commit with the commits-and-prs genre\n",
  "   (~/.claude/skills/writing-style/genres/commits-and-prs.md)\n",
  "   Comments follow the code genre\n",
  "   (~/.claude/skills/writing-style/genres/code.md).\n",
);
/// A direction's prompt on standard input: its skill path is relative, and a git ref names a commit's tree.
const TASK_BODY: &str =
  "Take one rule from `skills/writing-style/genres/code.md`, and read origin/main:skills/x too.\n";

/// Launches one round with `instructions` as its instructions file and `prompt` on its standard input, and returns the
/// instructions and the prompt that the round's claude received.
fn launch_with_skill_roots(world: &World, instructions: &str, prompt: &str) -> (String, String) {
  fs::write(&world.instructions, instructions).expect("the instructions file");
  let prompt_file = world.sandbox.root().join("prompt.txt");
  fs::write(&prompt_file, prompt).expect("the prompt");
  let recorded = world.sandbox.root().join("claude-instructions.md");
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_INSTRUCTIONS".to_string(), recorded.display().to_string());
  env.insert("FAKE_CLAUDE_STDIN".to_string(), world.claude_stdin.display().to_string());
  let stdin = fs::File::open(&prompt_file).expect("open the prompt");
  let mut command = world.launcher_command(&env);
  let run = harness::run_with_stdin(&mut command, Stdio::from(stdin));
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let received_instructions = fs::read_to_string(&recorded).expect("the instructions that claude received");
  let received_prompt = fs::read_to_string(&world.claude_stdin).expect("the prompt that claude received");
  (received_instructions, received_prompt)
}

/// Host skill roots that a round's instructions must not leave in their text.
const STALE_ROOTS: [&str; 3] = ["~/.claude/skills", "~/.agents/skills", "$CHARLIEBOT_HOME/skills"];

#[test]
#[ignore = "integration"]
fn the_launch_maps_the_skills_discovery_roots_to_the_round_skills_dir() {
  let world = World::new();
  let (instructions, _) = launch_with_skill_roots(&world, SKILLS_DISCOVERY, PROMPT);
  assert!(instructions.contains("`$IMPROVE_SKILLS_DIR/`"), "the discovery root did not map:\n{instructions}");
  for root in STALE_ROOTS {
    assert!(!instructions.contains(root), "the launch left {root} in the instructions:\n{instructions}");
  }
}

#[test]
#[ignore = "integration"]
fn the_launch_maps_the_worker_genre_paths_to_the_round_skills_dir() {
  let world = World::new();
  let (instructions, _) = launch_with_skill_roots(&world, WORKER_GENRES, PROMPT);
  for path in
    ["$IMPROVE_SKILLS_DIR/writing-style/genres/commits-and-prs.md", "$IMPROVE_SKILLS_DIR/writing-style/genres/code.md"]
  {
    assert!(instructions.contains(path), "the genre path did not map to {path}:\n{instructions}");
  }
  for root in STALE_ROOTS {
    assert!(!instructions.contains(root), "the launch left {root} in the instructions:\n{instructions}");
  }
}

#[test]
#[ignore = "integration"]
fn the_launch_maps_the_task_body_skill_path_and_keeps_a_git_ref() {
  let world = World::new();
  let (_, prompt) = launch_with_skill_roots(&world, SKILLS_DISCOVERY, TASK_BODY);
  assert!(
    prompt.contains("`$IMPROVE_SKILLS_DIR/writing-style/genres/code.md`"),
    "the task body's skill path did not map:\n{prompt}"
  );
  assert!(prompt.contains("origin/main:skills/x"), "the launch rewrote a git ref:\n{prompt}");
}

#[test]
#[ignore = "integration"]
fn a_round_worker_whose_parent_is_a_cron_session_still_runs_over_ssh() {
  let world = World::new();
  // The worker's metadata names PARENT_ID as its task_parent_id, and the cron task file binds
  // PARENT_ID. This run is therefore a round even though a cron session is one link away.
  let approval = world.sandbox.root().join("approval-needed");
  fs::write(&approval, "1\n").expect("the approval marker");
  let mut env = world.env();
  env.insert("SSH_APPROVAL_FILE".to_string(), approval.display().to_string());

  let run = world.run(&env);

  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(world.ssh_arguments().contains("\ntrue\n"), "the preflight did not reach ssh:\n{}", world.ssh_arguments());
  assert!(!world.claude_ran(), "the round ran before its preflight passed");
}

/// The in-flight list's run from `cwd`.
fn run_inflight_list_from(world: &World, env: &harness::Env, cwd: &Path) -> harness::Run {
  let mut command = harness::command("bash", env);
  command.arg(world.repo.join("dev/improve/inflight-files.sh")).arg(format!("improve/{DIRECTION}/")).current_dir(cwd);
  harness::run(&mut command)
}

fn run_launcher_from(world: &World, env: &harness::Env, cwd: &Path, arguments: &[String]) -> harness::Run {
  let mut command = harness::command("bash", env);
  command.arg(world.launcher()).args(arguments).current_dir(cwd);
  harness::run(&mut command)
}

#[test]
#[ignore = "integration"]
fn the_inflight_list_names_its_origin_repository_outside_a_git_checkout() {
  let world = World::new();
  let cwd = world.sandbox.root();
  assert!(!cwd.join(".git").exists(), "the test working directory is a git checkout");

  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "12\tcharliebot/task-open\tpr-only.md\n").expect("the open pull requests");

  let mut env = world.env();
  env.remove("GH_REPO");
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());
  env.insert("FAKE_GH_REPO".to_string(), world::GITHUB_REPOSITORY.to_string());
  env.insert("FAKE_GH_HOST".to_string(), "github.com".to_string());

  let run = run_inflight_list_from(&world, &env, cwd);

  assert_eq!(run.code, 0, "{}", run.describe());
  // The fixture's main has no pr-only.md, so the open pull request's claim on it carries the absent field.
  assert_eq!(run.stdout, "pr-only.md\tcharliebot/task-open\tabsent-on-origin-main\n", "{}", run.describe());

  // An origin that names no GitHub repository stops the list with an error naming the condition.
  world.git(&world.repo, &["remote", "set-url", "origin", "not-a-github-origin"]);
  let run = run_inflight_list_from(&world, &env, cwd);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("cannot parse the origin"), "{}", run.describe());

  // So does a checkout whose origin cannot be read at all.
  world.git(&world.repo, &["remote", "remove", "origin"]);
  let run = run_inflight_list_from(&world, &env, cwd);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("cannot read the origin"), "{}", run.describe());

  let mut relative_profile_home = env;
  relative_profile_home.insert("CHARLIEBOT_HOME".to_string(), "profile".to_string());
  let run = run_inflight_list_from(&world, &relative_profile_home, cwd);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("CHARLIEBOT_HOME must be an absolute path"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn the_remote_launcher_rejects_relative_input_and_temp_paths() {
  let world = World::new();
  let cwd = world.sandbox.root();

  let mut arguments = world.agent_arguments();
  let instructions = arguments
    .iter()
    .position(|argument| argument == "--append-system-prompt-file")
    .expect("the launcher arguments name the instructions file");
  arguments[instructions + 1] = "harness_instructions.md".to_string();
  let env = world.env();
  let run = run_launcher_from(&world, &env, cwd, &arguments);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("the instructions file path must be absolute"), "{}", run.describe());

  let mut env = world.env();
  env.insert("TMPDIR".to_string(), "relative-temp".to_string());
  let arguments = world.agent_arguments();
  let run = run_launcher_from(&world, &env, cwd, &arguments);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("TMPDIR must be an absolute path"), "{}", run.describe());

  for (name, path) in [("HOME", "relative-home"), ("CHARLIEBOT_HOME", "relative-profile")] {
    let mut env = world.env();
    env.insert(name.to_string(), path.to_string());
    let run = run_launcher_from(&world, &env, cwd, &arguments);
    assert_ne!(run.code, 0, "{}", run.describe());
    assert!(run.stderr.contains(&format!("{name} must be an absolute path")), "{}", run.describe());
  }
}

#[test]
#[ignore = "integration"]
fn the_remote_runner_rejects_a_relative_home() {
  let world = World::new();
  let mut env = world.env();
  env.insert("HOME".to_string(), "relative-home".to_string());
  let mut command = harness::command("bash", &env);
  command.arg(world.repo.join("dev/improve/remote-runner.sh")).args(["--prepare", "launch-id"]);

  let run = harness::run(&mut command);

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("HOME must be an absolute path"), "{}", run.describe());
}

/// The SHA-256 of a key, in the form the fake agents record it.
fn digest(value: &str) -> String {
  format!("{:x}", Sha256::digest(value))
}

/// The files under `root` whose text contains `needle`, relative to `root`.
fn files_holding(world: &World, root: &Path, needle: &str) -> Vec<String> {
  world
    .tree(root)
    .into_iter()
    .filter(|path| {
      let file = root.join(path);
      file.is_file() && fs::read_to_string(&file).is_ok_and(|text| text.contains(needle))
    })
    .collect()
}

#[test]
#[ignore = "integration"]
fn the_gateway_member_gets_its_token_and_no_review_key() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let env = world.claude_environment();
  let member = digest(GATEWAY_TOKEN);
  assert_eq!(env.get("ANTHROPIC_AUTH_TOKEN_SHA256").map(String::as_str), Some(member.as_str()), "{}", run.describe());
  // The fake claude records `unset` for a subscription login its environment does not hold, and the review key sits in
  // no variable of the member's environment or its record.
  assert_eq!(env.get("CLAUDE_CODE_OAUTH_TOKEN_SHA256").map(String::as_str), Some("unset"), "{}", run.describe());
  let record = fs::read_to_string(&world.claude_env).expect("the fake claude's environment record");
  assert!(!record.contains(REVIEW_KEY), "the review key reached the member's environment:\n{record}");
}

/// The CLI refuses a server-backed command only inside a round, and the refusal names the round's state sources from
/// the launch directory, the direction and the kit directory. The runner sets those and `CHARLIEBOT_HOME` at the round
/// home, so the round's server-backed commands refuse at once with the sources named.
#[test]
#[ignore = "integration"]
fn the_round_environment_names_its_state_sources_and_its_round_home() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let env = world.claude_environment();
  let round_home = format!("{}/improve/home", world.remote_home.display());
  assert_eq!(env.get("CHARLIEBOT_HOME"), Some(&round_home), "{}", run.describe());
  let launch_dir = env.get("IMPROVE_LAUNCH_DIR").expect("the round's environment names its launch directory");
  assert!(launch_dir.starts_with(&format!("{}/improve/launch/", world.remote_home.display())), "{launch_dir}");
  assert_eq!(env.get("IMPROVE_DIRECTION"), Some(&DIRECTION.to_string()), "{}", run.describe());
  let kit_dir = env.get("IMPROVE_KIT_DIR").expect("the round's environment names its kit directory");
  assert!(
    Path::new(kit_dir).join("dev/improve/github-post.sh").is_file(),
    "the kit holds no posting script: {kit_dir}"
  );
}

/// A merge outside the round reads the round's facts from its evidence directory, on the improve host and in the copy
/// the launcher brings back. The directory is the one the round's start stamp names.
#[test]
#[ignore = "integration"]
fn the_round_keeps_its_facts_in_the_evidence_directory_its_start_stamp_names() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the round did not start:\n{}", run.describe());
  let stamp = world.claude_environment().get("IMPROVE_ROUND_START").cloned().expect("the round's start stamp");
  let remote = world.remote_home.join("improve").join(DIRECTION).join("evidence").join(&stamp);
  for evidence in [remote, world.local_evidence().join(&stamp)] {
    for fact in ["inflight-files.txt", "round-events.ndjson"] {
      assert!(evidence.join(fact).is_file(), "{} lacks {fact}:\n{}", evidence.display(), run.describe());
    }
    let revision = fs::read_to_string(evidence.join("kit-revision")).expect("the round's kit revision");
    assert_eq!(revision.trim(), world.commit, "{} names another kit revision", evidence.display());
  }
}

/// The round's tools and the repository's browser tests start the browser that the start check ran: the round
/// environment names that browser in each input that reads a Chrome binary, so no test or tool resolves one of its own
/// inside a round.
#[test]
#[ignore = "integration"]
fn the_round_environment_hands_its_browser_to_each_tool_input() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let env = world.claude_environment();
  let browser = world.browser_path().display().to_string();
  for input in [
    "IMPROVE_BROWSER",
    "LIVE_SHOT_CHROME",
    "POOR_NETWORK_CHROME",
    "CHARLIEBOT_TEST_CHROME_BIN",
    "CHROME_BIN",
    "README_CAPTURE_CHROME",
  ] {
    assert_eq!(
      env.get(input),
      Some(&browser),
      "the round's {input} does not name the browser its start check ran: {}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn the_review_runs_backend_run_on_the_launch_review_directory_from_its_scratch_directory() {
  let world = World::new();
  let task = world.sandbox.root().join("review-task.md");
  fs::write(&task, "Review the diff.\nPR: 1 https://example.invalid/pull/1\n").expect("the review task file");
  let scratch = world.sandbox.dir("review-scratch");
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_REVIEW_TASK".to_string(), task.display().to_string());
  env.insert("FAKE_CLAUDE_REVIEW_DIR".to_string(), scratch.display().to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.review_ran(), "the round's review did not run backend run:\n{}", run.describe());

  // The review names no model, option or key: its launch directory holds the option and the login that run it.
  let arguments = world.review_arguments_received();
  assert_eq!(arguments.len(), 6, "{}", run.describe());
  assert_eq!(&arguments[..2], ["backend", "run"], "{}", run.describe());
  assert_eq!(arguments[2], "--launch", "{}", run.describe());
  assert!(arguments[3].ends_with("/review"), "the review names no review directory: {}", arguments[3]);
  assert_eq!(&arguments[4..], ["--task-file", task.display().to_string().as_str()], "{}", run.describe());
  assert_eq!(world.review_directory_received(), scratch.display().to_string(), "{}", run.describe());
  // The gateway key is not in the review's environment: the gateway backend reads it from the round's credentials file.
  let env = world.review_environment();
  assert!(!env.values().any(|value| *value == digest(REVIEW_KEY)), "the review key reached the review's environment");
  let credentials =
    fs::read_to_string(world.remote_home.join("improve/home/credentials.yaml")).expect("the round's credentials file");
  assert_eq!(credentials, format!("{REVIEW_CREDENTIAL}:\n  api_key: {REVIEW_KEY}\n"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn the_review_runs_with_the_round_clone_as_its_repo_root() {
  let world = World::new();
  let task = world.sandbox.root().join("review-task.md");
  fs::write(&task, "Review the diff.\nPR: 1 https://example.invalid/pull/1\n").expect("the review task file");
  let scratch = world.sandbox.dir("review-scratch");
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_REVIEW_TASK".to_string(), task.display().to_string());
  env.insert("FAKE_CLAUDE_REVIEW_DIR".to_string(), scratch.display().to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.review_ran(), "the round's review did not run backend run:\n{}", run.describe());

  // The CLI composes its checkout from CHARLIEBOT_REPO_ROOT, so the review names the round's own clone.
  let clone = world.remote_home.join("improve").join(DIRECTION).join("charlie-bot");
  let env = world.review_environment();
  assert_eq!(env.get("CHARLIEBOT_REPO_ROOT"), Some(&digest(&clone.display().to_string())), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_subscription_review_gets_its_account_login_in_its_own_process_only() {
  let world = World::new();
  world.write_review_answer(SUBSCRIPTION_REVIEW_TYPE, expiry_ms(10));
  let task = world.sandbox.root().join("review-task.md");
  fs::write(&task, "Review the diff.\nPR: 1 https://example.invalid/pull/1\n").expect("the review task file");
  let scratch = world.sandbox.dir("review-scratch");
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_REVIEW_TASK".to_string(), task.display().to_string());
  env.insert("FAKE_CLAUDE_REVIEW_DIR".to_string(), scratch.display().to_string());
  env.insert("FAKE_REVIEW_LOGIN".to_string(), world.sandbox.root().join("review-login.txt").display().to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.review_ran(), "the round's review did not run backend run:\n{}", run.describe());

  // The review's login file holds the account token at mode 600, and the review's environment carries its digest.
  let login = fs::read_to_string(world.sandbox.root().join("review-login.txt")).expect("the review's login record");
  assert!(login.contains("mode=600\n"), "the review's login is not mode 600:\n{login}");
  assert!(
    login.contains(&format!("CLAUDE_CODE_OAUTH_TOKEN={}\n", digest(REVIEW_LOGIN_TOKEN))),
    "the review's login holds the wrong token:\n{login}"
  );
  // The member's environment, a gateway member's in this world, holds no part of the review's login.
  let record = fs::read_to_string(&world.claude_env).expect("the fake claude's environment record");
  assert!(!record.contains(REVIEW_LOGIN_TOKEN), "the review's login reached the member's environment:\n{record}");
  assert!(!world.ssh_arguments().contains(REVIEW_LOGIN_TOKEN), "the review's login reached an ssh argument");
}

#[test]
#[ignore = "integration"]
fn an_unset_improve_review_exits_7_before_ssh_and_names_the_key() {
  let world = World::new();
  fs::remove_file(&world.resolve_answer).expect("the unset call class has no answer");
  let run = world.run(&world.env());
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(
    stderr(&run).contains("backends.calls.improve_review"),
    "the refusal does not name the unset key: {}",
    run.describe()
  );
  assert!(world.ssh_arguments().is_empty(), "the launcher reached ssh before it refused:\n{}", run.describe());
}

/// The expiry, in milliseconds since the epoch, of a login whose access token lasts `hours` more hours.
fn expiry_ms(hours: i64) -> i64 {
  (world::now_seconds() + hours * 3600) * 1000
}

/// Write a launch's member-credential file as `--prepare` writes it: each record as `NAME=value` and a NUL. The launcher
/// always sends a review record with the member's, so the gateway review's record is added unless `records` names one.
fn write_member_credential(launch: &Path, records: &[(&str, &str)]) {
  let mut records = records.to_vec();
  if !records.iter().any(|(name, _)| name.starts_with("REVIEW_")) {
    records.push(("REVIEW_ANTHROPIC_AUTH_TOKEN", REVIEW_KEY));
  }
  let mut bytes = Vec::new();
  for (name, value) in &records {
    bytes.extend_from_slice(format!("{name}={value}").as_bytes());
    bytes.push(0);
  }
  fs::write(launch.join("member-credential"), bytes).expect("the launch's member credential");
}

#[test]
#[ignore = "integration"]
fn a_subscription_claude_forwards_its_access_token_and_no_refresh_token() {
  let world = World::new();
  world.write_claude_login(expiry_ms(10));
  let run = world.run(&world.host_login_env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let env = world.claude_environment();
  let value = |name: &str| env.get(name).map(String::as_str);
  let login = digest(LOGIN_TOKEN);
  assert_eq!(value("CLAUDE_CODE_OAUTH_TOKEN_SHA256"), Some(login.as_str()), "{}", run.describe());
  assert_eq!(value("ANTHROPIC_AUTH_TOKEN_SHA256"), Some("unset"), "{}", run.describe());
  let record = fs::read_to_string(&world.claude_env).expect("the fake claude's environment record");
  assert!(!record.contains(REVIEW_KEY), "the review key reached the subscription member:\n{record}");
  // The access token reaches no ssh argument and no file on either home once the round ends. The refresh token
  // reaches nothing but the login file on this host.
  let arguments = world.ssh_arguments();
  assert!(!arguments.contains(LOGIN_TOKEN), "the access token reached an ssh argument");
  assert!(!arguments.contains(LOGIN_REFRESH_TOKEN), "the refresh token reached an ssh argument");
  assert!(
    files_holding(&world, &world.local_home, LOGIN_TOKEN).is_empty(),
    "the access token reached this host's files"
  );
  assert!(files_holding(&world, &world.remote_home, LOGIN_TOKEN).is_empty(), "the access token outlived the round");
  assert!(
    files_holding(&world, &world.remote_home, LOGIN_REFRESH_TOKEN).is_empty(),
    "the refresh token reached the remote home"
  );
}

#[test]
#[ignore = "integration"]
fn a_codex_member_runs_under_its_round_codex_home_with_the_forwarded_login() {
  let world = World::new();
  let exp = world::now_seconds() + 200 * 3600;
  world.write_codex_login(exp);
  let arguments = world.codex_arguments();
  let run = harness::run(&mut world.launcher_command_for(&world.host_login_env(), &arguments));
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.codex_ran(), "the fake codex did not run:\n{}", run.describe());
  assert!(!world.claude_ran(), "a codex member ran claude:\n{}", run.describe());
  let codex_home = world.codex_home_directory().display().to_string();
  assert_eq!(world.codex_home_received(), codex_home, "{}", run.describe());
  let record = fs::read_to_string(&world.codex_env).expect("the fake codex recorded its environment");
  assert!(!record.contains(REVIEW_KEY), "the codex member holds the review key:\n{record}");
  assert_eq!(world.codex_arguments_received(), arguments[1..].to_vec(), "{}", run.describe());

  // The login the round's codex found: the host's auth.json with an empty refresh token, mode 600.
  let found = fs::read_to_string(&world.codex_auth).expect("the login the fake codex found");
  let mut expected: Value = serde_json::from_str(&world::codex_login_json(exp)).expect("the fixture login is JSON");
  expected["tokens"]["refresh_token"] = json!("");
  let forwarded: Value = serde_json::from_str(&found).expect("the forwarded login is JSON");
  assert!(forwarded == expected, "the forwarded Codex login has the wrong fields");
  assert!(
    !forwarded.as_object().expect("the forwarded login is an object").contains_key("OPENAI_API_KEY"),
    "the forwarded Codex login includes an unrequested API key field"
  );
  assert_eq!(fs::read_to_string(&world.codex_auth_mode).expect("the login's mode").trim(), "600");

  // The round removed its login when it ended, and the refresh token stayed on this host.
  assert!(!world.codex_home_directory().join("auth.json").exists(), "the round left its login behind");
  assert!(
    files_holding(&world, &world.remote_home, CODEX_REFRESH_TOKEN).is_empty(),
    "the refresh token reached the remote home"
  );
  assert!(!world.ssh_arguments().contains(CODEX_REFRESH_TOKEN), "the refresh token reached an ssh argument");
}

/// The stub improve-sandbox of the focused export checks: it records the digest of the member's gateway token and the
/// digest of every variable the scope holds, so the assertion holds no value. It also records the CODEX_HOME it runs
/// with, the login file under it with that file's mode, and the TMPDIR the scope exports.
const SCOPE_ENV_STUB: &str = r#"# Fake improve-sandbox: record the digest of the gateway token, the digest of every variable of the scope, the CODEX_HOME
# it runs with and the login file under it with that file's mode, then exit.
{
  printf 'ANTHROPIC_AUTH_TOKEN_SHA256=%s\n' "$(printf '%s' "${ANTHROPIC_AUTH_TOKEN-}" | sha256sum | cut -d' ' -f1)"
  while IFS= read -r name; do
    printf '%s=%s\n' "$name" "$(printf '%s' "${!name}" | sha256sum | cut -d' ' -f1)"
  done < <(compgen -e)
  printf 'CODEX_HOME=%s\n' "${CODEX_HOME-unset}"
  printf 'SCOPE_TMPDIR=%s\n' "${TMPDIR-unset}"
} >"$FAKE_SCOPE_ENV"
if [[ -n ${CODEX_HOME-} && -f $CODEX_HOME/auth.json ]]; then
  cp -- "$CODEX_HOME/auth.json" "$FAKE_SCOPE_AUTH"
  printf 'CODEX_AUTH_MODE=%s\n' "$(stat -c %a -- "$CODEX_HOME/auth.json")" >>"$FAKE_SCOPE_ENV"
fi
# A review that the agent left running: a `charliebot backend run` starts in the background and outlives this scope's agent.
if [[ -n ${FAKE_SCOPE_LEFT_REVIEW:-} ]]; then
  bash "$FAKE_REVIEW_CHARLIEBOT" backend run --launch "$IMPROVE_LAUNCH_DIR/review" --task-file /dev/null \
    >/dev/null 2>&1 </dev/null &
  for _ in $(seq 1 600); do
    [[ -e $FAKE_REVIEW_STARTED ]] && break
    sleep 0.1
  done
fi
# The agent's stream, when a test names one: the runner tees it into the round's event log.
if [[ -n ${FAKE_SCOPE_STREAM:-} ]]; then
  cat -- "$FAKE_SCOPE_STREAM"
fi
exit 0
"#;

/// The direction of the focused export checks. Its TMPDIR name no real direction's round shares.
const FOCUS_DIRECTION: &str = "focused";
const SYMLINK_DIRECTION: &str = "symlink_review";
const SCOPE_TMPDIR_DIRECTION: &str = "scope_tmpdir";
const OTHER_DIRECTION: &str = "test_ux";

/// The run of the runner's `--in-scope` form alone, with the stub improve-sandbox that records the scope's variables and
/// the Codex home. The scope writes the review's login into the launch's review directory, and the round's credentials
/// file under `CHARLIEBOT_HOME`, which the run exports as the round's profile home.
fn run_in_scope(world: &World, launch_id: &str, kind: &str) -> harness::Run {
  run_in_scope_with(world, launch_id, kind, &[])
}

/// [`run_in_scope`] with `extra` variables set over its environment, such as a `PATH` that names a stub.
fn run_in_scope_with(world: &World, launch_id: &str, kind: &str, extra: &[(&str, String)]) -> harness::Run {
  run_in_scope_for(world, launch_id, FOCUS_DIRECTION, kind, extra)
}

/// [`run_in_scope_with`] for an explicit direction, so a test that asserts on `/tmp/improve-<direction>-tmp` does not
/// race with the `FOCUS_DIRECTION` tests.
fn run_in_scope_for(
  world: &World,
  launch_id: &str,
  direction: &str,
  kind: &str,
  extra: &[(&str, String)],
) -> harness::Run {
  let launch = world.stage_launch(launch_id);
  // The launcher always sends a review record with the member's, so a launch with no records gets the gateway
  // member's and the gateway review's. A test that stages other records keeps them.
  if !launch.join("member-credential").exists() {
    write_member_credential(&launch, &[("ANTHROPIC_AUTH_TOKEN", FOCUSED_MEMBER_TOKEN)]);
  }
  fs::write(launch.join("args"), "").expect("the launch arguments file");
  fs::create_dir_all(format!("/tmp/improve-{direction}-tmp")).expect("the round's TMPDIR");
  fs::create_dir_all(world.remote_home.join("improve").join(direction).join("evidence"))
    .expect("the round's evidence root");
  let bin = world.remote_home.join("improve/runner/bin");
  fs::create_dir_all(&bin).expect("the runner bin directory");
  harness::write_executable(&bin.join("improve-sandbox"), format!("#!/usr/bin/env bash\n{SCOPE_ENV_STUB}"));
  let mut env = harness::Env::new();
  env.insert("HOME".to_string(), world.remote_home.display().to_string());
  env.insert("IMPROVE_SKILLS_DIR".to_string(), round_skills_dir(world).display().to_string());
  env.insert("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string());
  env.insert("FAKE_SCOPE_ENV".to_string(), world.sandbox.root().join("scope-env.txt").display().to_string());
  env.insert("FAKE_SCOPE_AUTH".to_string(), world.sandbox.root().join("scope-auth.json").display().to_string());
  env.insert("CHARLIEBOT_HOME".to_string(), world.remote_home.join("improve/home").display().to_string());
  // The start stamp names the round's evidence directory, so each start in one world names its own by its launch id.
  env.insert("IMPROVE_ROUND_START".to_string(), launch_id.to_string());
  for (name, value) in extra {
    env.insert((*name).to_string(), value.clone());
  }
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("dev/improve/remote-runner.sh")).args([
    "--in-scope",
    launch_id,
    direction,
    kind,
  ]);
  harness::run(&mut command)
}

/// The skills directory that the round's runner exports as `IMPROVE_SKILLS_DIR`.
fn round_skills_dir(world: &World) -> PathBuf {
  world.remote_home.join("improve/runner/bin").join(&world.commit).join("skills")
}

/// One stream-json line of a tool call, as the round's agent writes it.
fn tool_call(name: &str, input: Value) -> String {
  json!({"type": "assistant", "message": {"content": [
    {"type": "tool_use", "id": "toolu_1", "name": name, "input": input}
  ]}})
  .to_string()
}

/// Writes `lines` as one event log in the sandbox and returns its path.
fn write_event_log(world: &World, name: &str, lines: &[String]) -> PathBuf {
  let path = world.sandbox.root().join(name);
  fs::write(&path, lines.join("\n") + "\n").expect("the event log");
  path
}

/// Runs the skill check of `dev/improve/skill-reads.sh` over each of `events`, against the round's `skills` directory.
fn skill_check(skills: &Path, events: &[&Path]) -> harness::Run {
  let mut env = harness::Env::new();
  env.insert("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string());
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("dev/improve/skill-reads.sh")).arg(skills).args(events);
  harness::run(&mut command)
}

#[test]
#[ignore = "integration"]
fn the_skill_check_names_each_skill_read_outside_the_skills_dir_and_passes_the_kit_reads() {
  let world = World::new();
  let skills = round_skills_dir(&world);
  let kit_rule = skills.join("writing-style/genres/code.md").display().to_string();
  // Two skill reads from host copies, as a round's event log holds them.
  let improve_worker = "/synthetic/checkout/skills/improve-worker/SKILL.md";
  let commits = "/synthetic/home/.claude/skills/writing-style/genres/commits-and-prs.md";
  let listing = "ls -la ~/.claude/skills/ ~/.agents/skills/ 2>/dev/null || true";
  let stale = write_event_log(
    &world,
    "stale-round.ndjson",
    &[
      tool_call("Bash", json!({"command": listing})),
      tool_call("Read", json!({"file_path": improve_worker})),
      tool_call("Read", json!({"file_path": commits})),
      tool_call("Read", json!({"file_path": kit_rule})),
    ],
  );

  let run = skill_check(&skills, &[&stale]);

  assert_eq!(run.code, 1, "{}", run.describe());
  let stderr = stderr(&run);
  assert!(stderr.contains(&format!("skill read outside {}: {improve_worker}", skills.display())), "{}", run.describe());
  assert!(stderr.contains(&format!("skill read outside {}: {commits}", skills.display())), "{}", run.describe());
  assert!(!stderr.contains(&kit_rule), "the kit's own read is refused:\n{}", run.describe());

  let clean = write_event_log(
    &world,
    "clean-round.ndjson",
    &[tool_call("Read", json!({"file_path": kit_rule})), tool_call("Bash", json!({"command": listing}))],
  );
  let run = skill_check(&skills, &[&clean]);
  assert_eq!(run.code, 0, "a round that reads only its skills dir is refused:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn the_skill_check_passes_a_relative_citation_and_names_each_rooted_command_read() {
  let world = World::new();
  let skills = round_skills_dir(&world);
  // A commit message and a heredoc body cite skills by their relative paths: they read nothing.
  let commit = "git commit -m \"docs: cite skills/writing-style/genres/code.md\n\n\
    See skills/writing-style/genres/commits-and-prs.md\"";
  let heredoc = "cat > notes.md <<'EOF'\nRead skills/writing-style/genres/code.md first.\nEOF";
  let citing = write_event_log(
    &world,
    "citing-round.ndjson",
    &[tool_call("Bash", json!({"command": commit})), tool_call("Bash", json!({"command": heredoc}))],
  );
  let run = skill_check(&skills, &[&citing]);
  assert_eq!(run.code, 0, "a citation was refused:\n{}", run.describe());

  // Each of these reads a skill file from a copy other than the round's: git's ref form, a variable's path, a home
  // path behind a flag.
  let rooted = [
    "git show origin/main:skills/writing-style/genres/code.md",
    "cat $CHARLIEBOT_HOME/skills/llm-context-guideline/SKILL.md",
    "tool --file=~/.claude/skills/writing-style/genres/code.md",
  ];
  let lines: Vec<String> = rooted.iter().map(|command| tool_call("Bash", json!({"command": command}))).collect();
  let reads = write_event_log(&world, "rooted-round.ndjson", &lines);
  let run = skill_check(&skills, &[&reads]);
  assert_eq!(run.code, 1, "{}", run.describe());
  for token in [
    "origin/main:skills/writing-style/genres/code.md",
    "$CHARLIEBOT_HOME/skills/llm-context-guideline/SKILL.md",
    "~/.claude/skills/writing-style/genres/code.md",
  ] {
    assert!(stderr(&run).contains(&format!("skill read outside {}: {token}", skills.display())), "{}", run.describe());
  }
}

#[test]
#[ignore = "integration"]
fn a_scope_keeps_the_agent_stream_as_the_round_event_log() {
  let world = World::new();
  let launch_id = "20261124T080000Z-1-302";
  let stream_file = write_event_log(
    &world,
    "stream.ndjson",
    &[tool_call("Read", json!({"file_path": "/synthetic/evidence/shot.png"}))],
  );

  let run = run_in_scope_with(&world, launch_id, "claude", &[("FAKE_SCOPE_STREAM", stream_file.display().to_string())]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let logged = fs::read_to_string(world.remote_home.join("improve/launch").join(launch_id).join("round-events.ndjson"))
    .expect("the round's event log");
  assert_eq!(logged, fs::read_to_string(&stream_file).expect("the stream"));
  assert_eq!(run.stdout, logged, "the agent's stream did not reach the runner's standard output");
}

#[test]
#[ignore = "integration"]
fn the_in_scope_runner_exports_the_member_token_and_no_review_key() {
  use std::os::unix::fs::PermissionsExt;

  let world = World::new();
  let launch_id = "20261101T090000Z-1-300";
  let launch = world.stage_launch(launch_id);
  write_member_credential(
    &launch,
    &[("ANTHROPIC_AUTH_TOKEN", FOCUSED_MEMBER_TOKEN), ("REVIEW_ANTHROPIC_AUTH_TOKEN", FOCUSED_REVIEW_KEY)],
  );

  let run = run_in_scope(&world, launch_id, "claude");

  assert_eq!(run.code, 0, "{}", run.describe());
  let text = fs::read_to_string(world.sandbox.root().join("scope-env.txt"))
    .expect("the stub improve-sandbox recorded its environment");
  assert!(text.contains(&format!("ANTHROPIC_AUTH_TOKEN_SHA256={}\n", digest(FOCUSED_MEMBER_TOKEN))), "{text}");
  assert!(!text.contains(&digest(FOCUSED_REVIEW_KEY)), "the round's environment holds the review key:\n{text}");
  // The gateway review reads its key from the round's credentials file, and its login file stays empty at mode 600.
  let credentials =
    fs::read_to_string(world.remote_home.join("improve/home/credentials.yaml")).expect("the round's credentials file");
  assert_eq!(credentials, format!("{REVIEW_CREDENTIAL}:\n  api_key: {FOCUSED_REVIEW_KEY}\n"), "{}", run.describe());
  let login = launch.join("review/login");
  assert_eq!(fs::read(&login).expect("the review's login file"), b"", "{}", run.describe());
  assert_eq!(fs::metadata(&login).expect("the review's login file").permissions().mode() & 0o777, 0o600);
}

/// The TMPDIR that the stub improve-sandbox of the last [`run_in_scope`] call recorded. The record is
/// `SCOPE_TMPDIR=` because the stub's digest dump already holds a `TMPDIR=` line for the variable's digest.
fn scope_tmpdir(world: &World) -> PathBuf {
  let text = fs::read_to_string(world.sandbox.root().join("scope-env.txt")).expect("the stub recorded its environment");
  let tmpdir = text.lines().find_map(|line| line.strip_prefix("SCOPE_TMPDIR=")).expect("the stub recorded TMPDIR");
  PathBuf::from(tmpdir)
}

#[test]
#[ignore = "integration"]
fn a_scope_removes_the_directories_earlier_scopes_made_and_keeps_the_session_tmpdir() {
  let world = World::new();
  let session_tmpdir = PathBuf::from(format!("/tmp/improve-{SCOPE_TMPDIR_DIRECTION}-tmp"));
  let _ = fs::remove_dir_all(&session_tmpdir);
  fs::create_dir_all(session_tmpdir.join("session-copy")).expect("a directory that no runner made");
  fs::write(session_tmpdir.join("session-copy/data.txt"), "session data").expect("the session copy");
  fs::write(session_tmpdir.join("loose.txt"), "loose").expect("a loose file in the session TMPDIR");

  let first = run_in_scope_for(&world, "20261101T090000Z-1-310", SCOPE_TMPDIR_DIRECTION, "claude", &[]);
  assert_eq!(first.code, 0, "{}", first.describe());
  let first_tmpdir = scope_tmpdir(&world);
  assert_eq!(first_tmpdir.parent(), Some(session_tmpdir.as_path()), "{}", first_tmpdir.display());
  fs::write(first_tmpdir.join("killed-copy.bin"), "copy").expect("the first scope's copy");

  let second = run_in_scope_for(&world, "20261101T090000Z-1-311", SCOPE_TMPDIR_DIRECTION, "claude", &[]);
  let second_tmpdir = scope_tmpdir(&world);
  let first_exists = first_tmpdir.exists();
  let second_is_dir = second_tmpdir.is_dir();
  let session_copy =
    fs::read_to_string(session_tmpdir.join("session-copy/data.txt")).expect("the session copy survived");
  let loose = fs::read_to_string(session_tmpdir.join("loose.txt")).expect("the loose file survived");
  let _ = fs::remove_dir_all(&session_tmpdir);

  assert_eq!(second.code, 0, "{}", second.describe());
  assert!(!first_exists, "the earlier scope's directory survived: {}", first_tmpdir.display());
  assert!(second_is_dir, "the second scope made no TMPDIR: {}", second_tmpdir.display());
  assert_eq!(second_tmpdir.parent(), Some(session_tmpdir.as_path()), "{}", second_tmpdir.display());
  assert_eq!(session_copy, "session data");
  assert_eq!(loose, "loose");
}

/// Whether process `pid` has ended: its entry is gone, or it is a zombie that its parent has not reaped.
fn process_ended(pid: u32) -> bool {
  match fs::read_to_string(format!("/proc/{pid}/stat")) {
    Ok(stat) => stat.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
    Err(error) => panic!("cannot read the stat of process {pid}: {error}"),
  }
}

#[test]
#[ignore = "integration"]
fn a_review_that_outlives_its_agent_fails_the_round_and_names_it() {
  let world = World::new();
  let launch_id = "20261101T090200Z-1-302";
  let launch = world.remote_home.join("improve/launch").join(launch_id);
  fs::create_dir_all(&launch).expect("the launch directory");
  write_member_credential(&launch, &[("ANTHROPIC_AUTH_TOKEN", FOCUSED_MEMBER_TOKEN)]);
  // The review is a `charliebot backend run` of the improve host. The leftover check matches a program whose basename is
  // `charliebot`, as the commit's `charliebot` link is in use, so the review runs a copy of the fake under that name.
  let review_dir = world.sandbox.dir("review-bin");
  let review_charliebot = review_dir.join("charliebot");
  fs::copy(
    world.remote_home.join("improve/runner/bin").join(format!("charliebot-{}", world.commit)),
    &review_charliebot,
  )
  .expect("the review's charliebot");
  let pid_file = world.sandbox.root().join("review-pid");
  let started = world.sandbox.root().join("review-started");
  let release = world.sandbox.root().join("review-release");

  let run = run_in_scope_with(
    &world,
    launch_id,
    "claude",
    &[
      ("FAKE_SCOPE_LEFT_REVIEW", "1".to_string()),
      ("FAKE_REVIEW_CHARLIEBOT", review_charliebot.display().to_string()),
      ("FAKE_REVIEW_ARGV", world.sandbox.root().join("review-argv.txt").display().to_string()),
      ("FAKE_REVIEW_CWD", world.sandbox.root().join("review-cwd.txt").display().to_string()),
      ("FAKE_REVIEW_ENV", world.sandbox.root().join("review-env.txt").display().to_string()),
      ("FAKE_REVIEW_PID", pid_file.display().to_string()),
      ("FAKE_REVIEW_STARTED", started.display().to_string()),
      ("FAKE_REVIEW_RELEASE", release.display().to_string()),
    ],
  );

  // The stub ends before this test does, so no later test's scope holds it.
  fs::write(&release, "").expect("the release file");
  let pid: u32 = fs::read_to_string(&pid_file)
    .unwrap_or_else(|error| panic!("the stub review wrote no process id ({error}): {}", run.describe()))
    .trim()
    .parse()
    .expect("a process id");
  assert!(wait_until(Duration::from_secs(10), || process_ended(pid)), "the stub review {pid} did not end");
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("charliebot backend run"), "the failure does not name the review: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn an_unsupported_member_kind_exits_7_before_any_input_is_sent() {
  let world = World::new();
  let mut arguments = world.agent_arguments();
  arguments[0] = "/opt/agents/gemini".to_string();
  let run = harness::run(&mut world.launcher_command_for(&world.env(), &arguments));
  assert_refused_before_input(&world, &run, "the member kind 'gemini' is not supported");
}

#[test]
#[ignore = "integration"]
fn a_gateway_member_without_its_token_exits_7_before_any_input_is_sent() {
  let world = World::new();
  let mut env = world.env();
  env.remove("ANTHROPIC_AUTH_TOKEN");
  let run = world.run(&env);
  assert_refused_before_input(&world, &run, "ANTHROPIC_AUTH_TOKEN");
}

#[test]
#[ignore = "integration"]
fn a_missing_review_key_exits_7_before_any_input_is_sent() {
  let world = World::new();
  fs::write(world.profile_home.join("credentials.yaml"), "synthetic-decoy:\n  api_key: decoy-value\n")
    .expect("the profile home's credentials without the review section");
  let run = world.run(&world.env());
  assert_refused_before_input(&world, &run, REVIEW_CREDENTIAL);
}

#[test]
#[ignore = "integration"]
fn a_round_without_its_review_login_exits_7_before_inventory_or_scope() {
  let world = World::new();
  let launch_id = "20261101T090200Z-1-302";
  let launch = world.stage_launch(launch_id);
  fs::write(launch.join("env"), "").expect("the launch environment");
  // The launch carries the member's record and no review record.
  fs::write(launch.join("member-credential"), "ANTHROPIC_AUTH_TOKEN=synthetic-member-token\0")
    .expect("the launch's login records");

  let cargo_log = world.sandbox.root().join("preflight-cargo-builds.log");
  let mut env = world.env();
  env.insert("HOME".to_string(), world.remote_home.display().to_string());
  env.insert("FAKE_CARGO_LOG".to_string(), cargo_log.display().to_string());
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("dev/improve/remote-runner.sh")).args([
    launch_id,
    DIRECTION,
    &world.commit,
    "claude",
    world::LOCAL_CLAUDE_VERSION,
    "claude",
    world::LOCAL_CLAUDE_VERSION,
  ]);
  let run = harness::run(&mut command);

  assert_refused_before_start(&world, &run, "the round has no review login");
  assert!(!launch.join("inflight-files.txt").exists(), "the runner listed in-flight files before preflight");
  assert!(!cargo_log.exists(), "the runner built the CLI before preflight");
}

#[test]
#[ignore = "integration"]
fn a_subscription_claude_without_its_login_exits_7_before_any_input_is_sent() {
  let world = World::new();
  let run = world.run(&world.host_login_env());
  assert_refused_before_input(&world, &run, &world.claude_login_file().display().to_string());
}

#[test]
#[ignore = "integration"]
fn a_codex_member_without_its_login_exits_7_before_any_input_is_sent() {
  let world = World::new();
  let run = harness::run(&mut world.launcher_command_for(&world.host_login_env(), &world.codex_arguments()));
  assert_refused_before_input(&world, &run, &world.local_home.join(".codex/auth.json").display().to_string());
}

#[test]
#[ignore = "integration"]
fn a_profile_home_on_the_host_exits_7_and_starts_no_scope() {
  let world = World::new();
  let host_profile_home = world.remote_home.join(".charliebot");
  fs::create_dir_all(&host_profile_home).expect("a profile home on the host");
  let run = world.run(&world.env());
  assert_refused_before_start(&world, &run, &host_profile_home.display().to_string());
  let round_home = world.remote_home.join("improve/home").display().to_string();
  assert!(
    stderr(&run).contains(&format!("{round_home} needs no change")),
    "the refusal does not say the round home needs no change: {}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_refused_write_boundary_exits_7_and_starts_no_scope() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_SANDBOX_REFUSE".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_refused_before_start(&world, &run, "the write boundary does not start");
}

#[test]
#[ignore = "integration"]
fn a_round_whose_browser_does_not_start_exits_7_naming_its_path_and_starts_no_scope() {
  let world = World::new();
  let browser = world.browser_path();
  harness::write_executable(
    &browser,
    "#!/usr/bin/env bash\necho 'snap-confine is packaged without necessary permissions' >&2\nexit 1\n",
  );
  let run = world.run(&world.env());
  assert_refused_before_start(&world, &run, &browser.display().to_string());
  assert!(
    stderr(&run).contains("snap-confine is packaged without necessary permissions"),
    "the refusal does not carry the browser's error: {}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_round_without_its_pinned_browser_exits_7_naming_the_build_and_starts_no_scope() {
  let world = World::new();
  fs::remove_file(world.browser_path()).expect("the synthetic browser");
  let run = world.run(&world.env());
  assert_refused_before_start(&world, &run, &world.browser_build_dir().display().to_string());
}

/// The assertion that the launcher refused a launch with exit 7 before it sent any input, and that stderr
/// names `needle`.
fn assert_refused_before_input(world: &World, run: &harness::Run, needle: &str) {
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(stderr(run).contains(needle), "the refusal does not name {needle}: {}", run.describe());
  assert!(world.ssh_arguments().is_empty(), "the refused launch sent input:\n{}", world.ssh_arguments());
}

/// The assertion that the runner refused a round with exit 7, that stderr names `needle`, and that the round
/// started no scope and no agent.
fn assert_refused_before_start(world: &World, run: &harness::Run, needle: &str) {
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(stderr(run).contains(needle), "the refusal does not name {needle}: {}", run.describe());
  assert!(!world.scope_started(), "the refused round started a scope:\n{}", world.systemctl_calls());
  assert!(!world.claude_ran(), "the refused round started claude");
  assert!(!world.codex_ran(), "the refused round started codex");
}

const FOCUSED_REVIEW_KEY: &str = "focused-review-key";
const FOCUSED_MEMBER_TOKEN: &str = "focused-member-token";

#[test]
#[ignore = "integration"]
fn the_round_writes_only_the_evidence_copy_on_this_host() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());

  let evidence_root = world.local_home.join("improve");
  let tree = world.tree(&evidence_root);
  assert!(!tree.is_empty(), "the launcher copied no evidence back:\n{}", run.describe());
  for path in &tree {
    assert!(
      path == DIRECTION
        || *path == format!("{DIRECTION}/evidence")
        || path.starts_with(&format!("{DIRECTION}/evidence/")),
      "the launcher wrote outside {}/{DIRECTION}/evidence: {path}",
      evidence_root.display()
    );
  }
  let copied = world.local_evidence().join("claude-argv.txt");
  assert!(copied.is_file(), "the round's evidence did not arrive: {:?}", world.tree(&evidence_root));
}

#[test]
#[ignore = "integration"]
fn a_failed_evidence_copy_reports_the_copys_status_and_the_round_still_starts() {
  let world = World::new();
  let mut env = world.env();
  env.insert("SSH_FAIL_EVIDENCE_LISTING".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the failed copy stopped the round itself:\n{}", run.describe());
  let message = stderr(&run);
  assert!(
    message.contains("the evidence copy stopped (status 7)"),
    "the copy's own status did not reach stderr: {message}"
  );
}

#[test]
#[ignore = "integration"]
fn a_second_evidence_copy_leaves_complete_evidence_alone() {
  let world = World::new();
  let previous = world.remote_home.join("improve").join(DIRECTION).join("evidence").join("20261101T090000Z");
  fs::create_dir_all(&previous).expect("the previous round's evidence directory");
  fs::write(previous.join("measure.txt"), "measurement\n").expect("the previous round's evidence");

  let first = world.run(&world.env());

  assert_eq!(first.code, 0, "{}", first.describe());
  let copied = world.local_evidence().join("20261101T090000Z/measure.txt");
  assert_eq!(fs::read_to_string(&copied).expect("the evidence arrived"), "measurement\n");
  // A second copy that transferred this file again would rewrite it, which moves its status change
  // time. The wait keeps the two times in different seconds even on a coarse filesystem.
  std::thread::sleep(Duration::from_secs(1));
  let before = status_change_time(&copied);
  remove_newest_round_evidence(&world);

  let second = world.run(&world.env());

  assert_eq!(second.code, 0, "{}", second.describe());
  assert_eq!(
    status_change_time(&copied),
    before,
    "the second copy rewrote evidence that was already complete:\n{}",
    second.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_partly_copied_evidence_directory_completes_on_the_next_copy() {
  let world = World::new();
  let remote_round = world.remote_home.join("improve").join(DIRECTION).join("evidence").join("20261101T090000Z");
  fs::create_dir_all(&remote_round).expect("the previous round's evidence directory");
  for (name, contents) in
    [("complete.txt", "complete\n"), ("partial.txt", "partial evidence\n"), ("missing.txt", "missing\n")]
  {
    fs::write(remote_round.join(name), contents).expect("the previous round's evidence");
  }
  // The state a stopped copy leaves: one file complete, one truncated, one absent.
  let local_round = world.local_evidence().join("20261101T090000Z");
  fs::create_dir_all(&local_round).expect("the partly copied directory");
  fs::write(local_round.join("complete.txt"), "complete\n").expect("the complete file");
  fs::write(local_round.join("partial.txt"), "part").expect("the truncated file");

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  for (name, contents) in
    [("complete.txt", "complete\n"), ("partial.txt", "partial evidence\n"), ("missing.txt", "missing\n")]
  {
    assert_eq!(
      fs::read_to_string(local_round.join(name)).unwrap_or_else(|error| panic!("{name}: {error}")),
      contents,
      "the next copy did not complete {name}:\n{}",
      run.describe()
    );
  }
}

/// The time a file's status last changed, as `(seconds, nanoseconds)`.
fn status_change_time(path: &std::path::Path) -> (i64, i64) {
  let metadata = fs::metadata(path).expect("the local evidence's metadata");
  (metadata.ctime(), metadata.ctime_nsec())
}

#[test]
#[ignore = "integration"]
fn no_key_value_reaches_an_ssh_argument_or_any_file_but_the_credentials_file() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let arguments = world.ssh_arguments();
  assert!(!arguments.is_empty(), "the fake ssh recorded no call");
  let agent_arguments = world.claude_arguments_received().join("\n");
  for key in [GATEWAY_TOKEN, REVIEW_KEY, world::RUN_TOKEN, world::LOCAL_CLAUDE_OAUTH_TOKEN] {
    assert!(!arguments.contains(key), "a key value reached an ssh argument");
    assert!(!agent_arguments.contains(key), "a key value reached the agent's arguments");
  }
  assert_eq!(
    world.claude_environment().get("CLAUDE_CODE_OAUTH_TOKEN_SHA256").map(String::as_str),
    Some("unset"),
    "the launcher's own Claude login reached the round"
  );

  // The credentials file holds the review key only. The gateway token lived in the launch's member-credential file,
  // which the round removed when it ended.
  let credentials = world.remote_home.join("improve/home/credentials.yaml");
  let text = fs::read_to_string(&credentials).expect("the runner wrote the credentials file");
  assert!(text.contains(REVIEW_KEY) && !text.contains(GATEWAY_TOKEN), "the credentials file holds the wrong keys");
  let mode = fs::metadata(&credentials).expect("credentials metadata").permissions().mode() & 0o777;
  assert_eq!(mode, 0o600, "{credentials:?} has mode {mode:o}");
  assert!(files_holding(&world, &world.local_home, GATEWAY_TOKEN).is_empty(), "a key reached this host's files");
  assert!(files_holding(&world, &world.local_home, REVIEW_KEY).is_empty(), "a key reached this host's files");
  assert_eq!(
    files_holding(&world, &world.remote_home, REVIEW_KEY),
    ["improve/home/credentials.yaml"],
    "the review key reached a file other than the credentials file"
  );
  assert!(files_holding(&world, &world.remote_home, GATEWAY_TOKEN).is_empty(), "the gateway token outlived the round");
  for key in [world::RUN_TOKEN, world::LOCAL_CLAUDE_OAUTH_TOKEN] {
    assert!(files_holding(&world, &world.local_home, key).is_empty(), "a key reached this host's files");
    assert!(files_holding(&world, &world.remote_home, key).is_empty(), "a key reached the remote host's files");
  }
}

/// The assertion that the launcher refused a launch with exit 8 before it sent any input: the login's token has less
/// life than a round needs, and stderr names the login and says no round started.
fn assert_short_before_input(world: &World, run: &harness::Run, login: &Path) {
  assert_eq!(run.code, 8, "{}", run.describe());
  let message = stderr(run);
  assert!(
    message.contains(&login.display().to_string()) && message.contains("min of access-token life left"),
    "the refusal does not name the login and its life: {message}"
  );
  assert!(!message.contains("401") && !message.contains("403"), "the refusal names an auth status: {message}");
  assert!(world.ssh_arguments().is_empty(), "the short login sent input:\n{}", world.ssh_arguments());
}

#[test]
#[ignore = "integration"]
fn a_subscription_login_that_does_not_parse_exits_7_naming_the_file_before_any_input() {
  let world = World::new();
  fs::create_dir_all(&world.claude_login_dir).expect("the Claude login directory");
  fs::write(world.claude_login_file(), "{\"claudeAiOauth\":").expect("the truncated login");
  let run = world.run(&world.host_login_env());
  assert_refused_before_input(&world, &run, &world.claude_login_file().display().to_string());
}

#[test]
#[ignore = "integration"]
fn a_subscription_login_without_a_numeric_expiry_exits_7_naming_the_file_before_any_input() {
  let world = World::new();
  fs::create_dir_all(&world.claude_login_dir).expect("the Claude login directory");
  fs::write(world.claude_login_file(), r#"{"claudeAiOauth":{"accessToken":"synthetic","expiresAt":"soon"}}"#)
    .expect("the login without a numeric expiry");
  let run = world.run(&world.host_login_env());
  assert_refused_before_input(&world, &run, &world.claude_login_file().display().to_string());
}

#[test]
#[ignore = "integration"]
fn a_subscription_login_with_less_than_four_hours_of_token_life_exits_8_before_any_input() {
  let world = World::new();
  world.write_claude_login(expiry_ms(3));
  let run = world.run(&world.host_login_env());
  assert_short_before_input(&world, &run, &world.claude_login_file());
  assert!(!world.claude_ran(), "a short login started claude:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_codex_login_with_less_than_four_hours_of_token_life_exits_8_before_any_input() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 2 * 3600);
  let run = harness::run(&mut world.launcher_command_for(&world.host_login_env(), &world.codex_arguments()));
  assert_short_before_input(&world, &run, &world.local_home.join(".codex/auth.json"));
  assert!(!world.codex_ran(), "a short login started codex:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_missing_token_life_threshold_exits_7_before_any_input() {
  let world = World::new();
  let mut env = world.env();
  env.remove("IMPROVE_TOKEN_MIN_SECONDS");

  let run = world.run(&env);

  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(stderr(&run).contains("IMPROVE_TOKEN_MIN_SECONDS is not set"), "{}", run.describe());
  assert!(world.ssh_arguments().is_empty(), "a missing threshold contacted the host");
}

/// The forwarded form of a codex login whose access token expires at `exp`: the refresh token is the empty string,
/// as the launch sends it.
fn forwarded_codex_login(exp: i64) -> String {
  let mut login: Value = serde_json::from_str(&world::codex_login_json(exp)).expect("the fixture login is JSON");
  login["tokens"]["refresh_token"] = json!("");
  login.to_string()
}

/// Make a codex launch's directory with its member credential holding `forwarded`, the login the launch carries.
fn codex_in_scope_fixture(world: &World, launch_id: &str, forwarded: &str) {
  let launch = world.stage_launch(launch_id);
  write_member_credential(&launch, &[("IMPROVE_CODEX_AUTH", forwarded)]);
}

#[test]
#[ignore = "integration"]
fn the_in_scope_codex_round_runs_under_a_codex_home_holding_its_login_at_mode_600() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  let launch_id = "20261101T090300Z-1-303";
  let forwarded = forwarded_codex_login(world::now_seconds() + 200 * 3600);
  codex_in_scope_fixture(&world, launch_id, &forwarded);
  fs::create_dir(&round_tmp).expect("the round's directory");
  // The round's start asks the codex on PATH which skills it lists: this stub lists none, with the section marker.
  let discovery = world.sandbox.dir("codex-discovery");
  let answer = format!(
    "#!/usr/bin/env bash\nif [[ ${{1-}} == debug ]]; then\n  printf '%s\\n' '{}'\nfi\n",
    world::NO_SKILLS_DISCOVERY
  );
  harness::write_executable(&discovery.join("codex"), answer);
  let path = format!("{}:/usr/local/bin:/usr/bin:/bin", discovery.display());

  let run = run_in_scope_with(&world, launch_id, "codex", &[("PATH", path)]);

  let scope_env = fs::read_to_string(world.sandbox.root().join("scope-env.txt")).unwrap_or_default();
  let found = fs::read_to_string(world.sandbox.root().join("scope-auth.json")).ok();
  let _ = fs::remove_dir_all(&round_tmp);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(scope_env.contains(&format!("CODEX_HOME={}\n", round_tmp.join("codex-home").display())), "{scope_env}");
  assert!(scope_env.contains("CODEX_AUTH_MODE=600\n"), "the login is not mode 600:\n{scope_env}");
  let found = found.expect("the stub improve-sandbox copied the login");
  assert_eq!(
    serde_json::from_str::<Value>(&found).expect("the round's login is JSON"),
    serde_json::from_str::<Value>(&forwarded).expect("the forwarded login is JSON"),
    "the round's login is not the forwarded one"
  );
}

#[test]
#[ignore = "integration"]
fn a_codex_round_refuses_a_round_directory_that_is_a_symbolic_link() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  let target = world.sandbox.dir("elsewhere");
  std::os::unix::fs::symlink(&target, &round_tmp).expect("the symbolic link");
  let launch_id = "20261101T090400Z-1-304";
  codex_in_scope_fixture(&world, launch_id, &forwarded_codex_login(world::now_seconds() + 200 * 3600));

  let run = run_in_scope(&world, launch_id, "codex");

  let _ = fs::remove_file(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains(&format!("{} is a symbolic link", round_tmp.display())), "{}", run.describe());
  assert!(!target.join("codex-home").exists(), "a Codex home was made through the link");
}

#[test]
#[ignore = "integration"]
fn a_codex_round_refuses_a_codex_home_that_is_a_symbolic_link() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  fs::create_dir(&round_tmp).expect("the round's directory");
  let target = world.sandbox.dir("elsewhere");
  let codex_home = round_tmp.join("codex-home");
  std::os::unix::fs::symlink(&target, &codex_home).expect("the symbolic link");
  let launch_id = "20261101T090500Z-1-305";
  codex_in_scope_fixture(&world, launch_id, &forwarded_codex_login(world::now_seconds() + 200 * 3600));

  let run = run_in_scope(&world, launch_id, "codex");

  let _ = fs::remove_dir_all(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains(&format!("{} is a symbolic link", codex_home.display())), "{}", run.describe());
  assert!(!target.join("auth.json").exists(), "a login was written through the link");
}

#[test]
#[ignore = "integration"]
fn a_codex_round_refuses_a_codex_home_another_user_owns() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  fs::create_dir_all(round_tmp.join("codex-home")).expect("the Codex home");
  // A stat that reports another owner for the Codex home only; the round's own directory keeps its real owner.
  let stub = world.sandbox.dir("owner-stub");
  harness::write_executable(
    &stub.join("stat"),
    r#"#!/usr/bin/env bash
# Reports uid 0 for the Codex home, and the real owner for every other path.
if [[ $* == *codex-home* ]]; then
  printf '0\n'
else
  command -p stat "$@"
fi
"#,
  );
  let launch_id = "20261101T090600Z-1-306";
  codex_in_scope_fixture(&world, launch_id, &forwarded_codex_login(world::now_seconds() + 200 * 3600));
  let path = format!("{}:/usr/local/bin:/usr/bin:/bin", stub.display());

  let run = run_in_scope_with(&world, launch_id, "codex", &[("PATH", path)]);

  let _ = fs::remove_dir_all(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("codex-home is not owned by the runner's user"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_codex_round_refuses_a_parent_directory_another_user_owns() {
  let world = World::new();
  let round_tmp = PathBuf::from(format!("/tmp/improve-{FOCUS_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  fs::create_dir(&round_tmp).expect("the round's directory");
  let stub = world.sandbox.dir("owner-stub");
  harness::write_executable(
    &stub.join("stat"),
    format!(
      r#"#!/usr/bin/env bash
# Reports uid 0 for the round directory, and the real owner for every other path.
if [[ $* == *"{}"* ]]; then
  printf '0\n'
else
  command -p stat "$@"
fi
"#,
      round_tmp.display()
    ),
  );
  let launch_id = "20261101T090700Z-1-307";
  codex_in_scope_fixture(&world, launch_id, &forwarded_codex_login(world::now_seconds() + 200 * 3600));
  let path = format!("{}:/usr/local/bin:/usr/bin:/bin", stub.display());

  let run = run_in_scope_with(&world, launch_id, "codex", &[("PATH", path)]);

  let _ = fs::remove_dir_all(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("is not owned by the runner's user"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_start_refuses_a_codex_parent_symlink_before_removing_its_target_login() {
  let world = World::new_with_direction(SYMLINK_DIRECTION);
  let round_tmp = PathBuf::from(format!("/tmp/improve-{SYMLINK_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  let target = world.sandbox.dir("elsewhere");
  let auth = target.join("codex-home/auth.json");
  fs::create_dir_all(auth.parent().expect("the target Codex home")).expect("the target Codex home");
  fs::write(&auth, "synthetic-codex-auth").expect("the target login");
  std::os::unix::fs::symlink(&target, &round_tmp).expect("the round directory symlink");

  let run = world.run(&world.env());

  let login_kept = fs::read_to_string(&auth).expect("the target login remains readable");
  let _ = fs::remove_file(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("is a symbolic link"), "{}", run.describe());
  assert!(login_kept == "synthetic-codex-auth", "the start sweep changed the target login");
}

#[test]
#[ignore = "integration"]
fn a_start_refuses_a_codex_home_symlink_before_removing_its_target_login() {
  let world = World::new_with_direction(SYMLINK_DIRECTION);
  let round_tmp = PathBuf::from(format!("/tmp/improve-{SYMLINK_DIRECTION}"));
  assert!(!round_tmp.exists(), "{} exists, so the test does not own it", round_tmp.display());
  fs::create_dir(&round_tmp).expect("the round directory");
  let target = world.sandbox.dir("elsewhere");
  let auth = target.join("auth.json");
  fs::write(&auth, "synthetic-codex-auth").expect("the target login");
  let codex_home = round_tmp.join("codex-home");
  std::os::unix::fs::symlink(&target, &codex_home).expect("the Codex home symlink");

  let run = world.run(&world.env());

  let login_kept = fs::read_to_string(&auth).expect("the target login remains readable");
  let _ = fs::remove_file(&codex_home);
  let _ = fs::remove_dir(&round_tmp);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("is a symbolic link"), "{}", run.describe());
  assert!(login_kept == "synthetic-codex-auth", "the start sweep changed the target login");
}

#[test]
#[ignore = "integration"]
fn a_duplicate_codex_fire_leaves_the_live_login_and_removes_its_own_launch_directory() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut leader = spawn_live_round(&world, DIRECTION);
  let live_login = world.codex_home_directory().join("auth.json");
  fs::create_dir_all(live_login.parent().expect("the Codex home")).expect("the live round's Codex home");
  fs::write(&live_login, "LIVE-CODEX-LOGIN").expect("the live round's login");
  let mut env = world.host_login_env();
  env.insert("FAKE_SCOPE_STATE".to_string(), "active".to_string());

  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));

  stop_live_round(&mut leader);
  let kept = fs::read_to_string(&live_login);
  let _ = fs::remove_file(&live_login);
  assert_eq!(run.code, 5, "{}", run.describe());
  assert!(kept.as_ref().is_ok_and(|login| login == "LIVE-CODEX-LOGIN"), "the duplicate fire changed the live login");
  assert!(!world.codex_ran(), "the duplicate fire started codex:\n{}", run.describe());
  let launches: Vec<String> = fs::read_dir(world.remote_home.join("improve/launch"))
    .expect("the launch directories")
    .map(|entry| entry.expect("a launch directory entry").file_name().to_string_lossy().into_owned())
    .collect();
  assert_eq!(launches, ["20261103T120000Z-9-900"], "the duplicate fire left its launch directory");
}

#[test]
#[ignore = "integration"]
fn a_start_removes_a_dead_launchs_member_credential_and_keeps_a_live_launchs() {
  let world = World::new();
  let dead = world.remote_home.join("improve/launch/20261001T000000Z-7-70");
  fs::create_dir_all(&dead).expect("the dead launch's directory");
  write_member_credential(&dead, &[("ANTHROPIC_AUTH_TOKEN", "synthetic-dead-launch-token")]);
  let mut leader = spawn_live_round(&world, OTHER_DIRECTION);
  let live = world.remote_home.join("improve/launch/20261103T120000Z-9-900/member-credential");
  write_member_credential(
    live.parent().expect("the live launch's directory"),
    &[("ANTHROPIC_AUTH_TOKEN", "synthetic-live-launch-token")],
  );

  let run = world.run(&world.env());

  let live_kept = live.exists();
  stop_live_round(&mut leader);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(!dead.join("member-credential").exists(), "a start kept a dead launch's member credential");
  assert!(live_kept, "a start removed a live launch's member credential");
}

#[test]
#[ignore = "integration"]
fn a_prepared_launch_keeps_its_member_credential_until_its_wrapper_starts() {
  let world = World::new();
  let ready = world.sandbox.root().join("main-run-ready");
  let release = world.sandbox.root().join("main-run-release");
  let mut first_env = world.env();
  first_env.insert("FAKE_SSH_PAUSE_MAIN_RUN".to_string(), "1".to_string());
  first_env.insert("FAKE_SSH_MAIN_RUN_READY".to_string(), ready.display().to_string());
  first_env.insert("FAKE_SSH_MAIN_RUN_RELEASE".to_string(), release.display().to_string());
  let mut first_command = world.launcher_command(&first_env);
  let first = harness::spawn(&mut first_command);

  let reached_gap = wait_until(Duration::from_secs(15), || ready.exists());
  if !reached_gap {
    let _ = fs::write(&release, "continue\n");
    let _ = first.wait_with_output();
    panic!("the first launch did not pause after preparing its member credential");
  }
  let first_launch = world.launch_directory().expect("the prepared launch directory");
  let member_credential = first_launch.join("member-credential");
  let second = world.run(&world.env());
  let survived_sweep = member_credential.is_file();
  remove_newest_round_evidence(&world);
  fs::write(&release, "continue\n").expect("release the first launch");
  let first_result = first.wait_with_output().expect("the first launch finished");

  assert_eq!(second.code, 0, "{}", second.describe());
  assert!(survived_sweep, "a concurrent start removed the prepared launch's member credential");
  assert!(first_result.status.success(), "the prepared launch failed before its wrapper started");
  assert!(!first_launch.exists(), "the prepared launch directory was not cleaned up");
}

#[test]
#[ignore = "integration"]
fn a_start_removes_an_idle_directions_codex_login_and_keeps_a_live_rounds() {
  let world = World::new();
  let login = world.codex_home_directory().join("auth.json");
  fs::create_dir_all(world.codex_home_directory()).expect("the Codex home");
  fs::write(&login, "IDLE-CODEX-LOGIN").expect("the idle login");

  let idle_run = world.run(&world.env());

  assert_eq!(idle_run.code, 0, "{}", idle_run.describe());
  assert!(!login.exists(), "a start kept an idle direction's Codex login");

  let mut leader = spawn_live_round(&world, DIRECTION);
  fs::write(&login, "LIVE-CODEX-LOGIN").expect("the live round's login");
  let live_run = world.run(&world.env());
  let kept = fs::read_to_string(&login);
  stop_live_round(&mut leader);
  let _ = fs::remove_file(&login);

  // The clone sync refuses a live round of the direction before the sweep, so the second start exits 5 and the live
  // login stays. The start refuses the live round's leader with that exit code, as the runner's header says.
  assert_eq!(live_run.code, 5, "{}", live_run.describe());
  assert!(
    stderr(&live_run).contains("the previous round still runs"),
    "a start did not refuse the live round:\n{}",
    live_run.describe()
  );
  assert!(kept.as_ref().is_ok_and(|login| login == "LIVE-CODEX-LOGIN"), "a start removed a live round's login");
}

#[test]
#[ignore = "integration"]
fn two_launches_prepared_at_once_keep_their_own_member_records_and_the_review_key() {
  let world = World::new();
  let first = "20261101T100000Z-1-400";
  let second = "20261101T100000Z-2-401";
  let launches = world.remote_home.join("improve/launch");
  fs::create_dir_all(launches.join(first)).expect("the first launch directory");
  fs::create_dir_all(launches.join(second)).expect("the second launch directory");
  let review = ("REVIEW_ANTHROPIC_AUTH_TOKEN", REVIEW_KEY);
  let first_member = ("ANTHROPIC_AUTH_TOKEN", "synthetic-first-gateway-token");
  let second_member = ("IMPROVE_CODEX_AUTH", "{\"synthetic\":\"second-launch\"}");
  let children: Vec<_> = [(first, first_member), (second, second_member)]
    .into_iter()
    .map(|(launch_id, member)| prepare_in_background(&world, launch_id, &[member, review]))
    .collect();
  for child in children {
    let output = child.wait_with_output().expect("the prepare call finished");
    assert!(output.status.success(), "prepare failed: {}", String::from_utf8_lossy(&output.stderr));
  }

  assert!(
    member_records(&launches.join(first).join("member-credential"))
      == ["ANTHROPIC_AUTH_TOKEN=synthetic-first-gateway-token", "REVIEW_ANTHROPIC_AUTH_TOKEN=synthetic-review-key"],
    "the first launch's records changed"
  );
  assert!(
    member_records(&launches.join(second).join("member-credential"))
      == ["IMPROVE_CODEX_AUTH={\"synthetic\":\"second-launch\"}", "REVIEW_ANTHROPIC_AUTH_TOKEN=synthetic-review-key"],
    "the second launch's records changed"
  );
  // Prepare writes the launches' records only: the round's credentials file is the scope's to write.
  assert!(
    !world.remote_home.join("improve/home/credentials.yaml").exists(),
    "prepare wrote the round's credentials file"
  );
  for launch_id in [first, second] {
    let mode = fs::metadata(launches.join(launch_id).join("member-credential"))
      .expect("the member credential")
      .permissions()
      .mode();
    assert_eq!(mode & 0o777, 0o600, "the member credential of {launch_id} has mode {mode:o}");
  }
}

/// Start `--prepare` for one launch with its records on standard input, and return the running child.
fn prepare_in_background(world: &World, launch_id: &str, records: &[(&str, &str)]) -> std::process::Child {
  let mut env = harness::Env::new();
  env.insert("HOME".to_string(), world.remote_home.display().to_string());
  env.insert("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string());
  let mut command = harness::command("bash", &env);
  command
    .arg(harness::repo_root().join("dev/improve/remote-runner.sh"))
    .args(["--prepare", launch_id])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  let mut child = command.spawn().expect("the prepare call starts");
  let mut bytes = Vec::new();
  for (name, value) in records {
    bytes.extend_from_slice(format!("{name}={value}").as_bytes());
    bytes.push(0);
  }
  if let Some(mut stdin) = child.stdin.take() {
    stdin.write_all(&bytes).expect("the records reach the prepare call");
  }
  child
}

/// The records of a member-credential file, each as its `NAME=value` text.
fn member_records(path: &Path) -> Vec<String> {
  let bytes = fs::read(path).expect("the member credential file");
  bytes
    .split(|byte| *byte == 0)
    .filter(|record| !record.is_empty())
    .map(|record| String::from_utf8_lossy(record).into_owned())
    .collect()
}

#[test]
#[ignore = "integration"]
fn claudes_exit_code_passes_through() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_EXIT".to_string(), "7".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(world.claude_ran());
}

#[test]
#[ignore = "integration"]
fn a_round_gh_write_with_a_credential_value_stops_the_round_with_its_kind() {
  // The round's member runs one gh write whose text carries the member's own login: the shim refuses it before the
  // real gh runs, and the round ends with the credential reason.
  let world = World::new();
  let mut env = world.env();
  env.insert(
    "FAKE_CLAUDE_COMMAND".to_string(),
    r#"gh pr comment 1 --body "leaked $CLAUDE_CODE_OAUTH_TOKEN $ANTHROPIC_AUTH_TOKEN""#.to_string(),
  );
  let run = world.run(&env);
  assert_eq!(run.code, 3, "{}", run.describe());
  let text = format!("{}{}", run.stdout, run.stderr);
  assert!(text.contains("environment variable"), "the kind is the reason: {text}");
  assert!(!text.contains(world::LOGIN_TOKEN) && !text.contains(world::GATEWAY_TOKEN), "no value is printed: {text}");
}

#[test]
#[ignore = "integration"]
fn the_runner_generates_a_missing_shared_mock_home_before_the_round_starts() {
  let world = World::new();
  let mock_home = world.remote_home.join("improve/mock-home").join(world::MOCK_HOME_VERSION);
  assert!(!mock_home.exists(), "the fixture starts with a shared mock home");

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(
    fs::read_to_string(&world.mock_home_log).expect("the mock-home calls"),
    format!("version\ngenerate {} --shape serve-task-tree\nround {DIRECTION}\n", mock_home.display()),
    "the runner did not generate the complete mock home before starting the round"
  );
  assert_eq!(
    fs::read_to_string(mock_home.join("manifest.json")).expect("the generated manifest"),
    "synthetic complete mock home\n"
  );
}

#[test]
#[ignore = "integration"]
fn a_mock_home_generation_failure_names_the_cause_and_stops_the_round() {
  let world = World::new();
  let mock_home = world.remote_home.join("improve/mock-home").join(world::MOCK_HOME_VERSION);
  let mut env = world.env();
  env.insert("FAKE_MOCK_HOME_FAIL".to_string(), "1".to_string());

  let run = world.run(&env);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(stderr(&run).contains("synthetic mock-home generation failure: disk full"), "{}", run.describe());
  assert!(stderr(&run).contains(&mock_home.display().to_string()), "{}", run.describe());
  assert!(!world.claude_ran(), "the round started after mock-home generation failed");
  assert!(
    fs::read_to_string(&world.systemd_run_log).unwrap_or_default().is_empty(),
    "the runner started a scope after mock-home generation failed"
  );
  assert!(!mock_home.exists(), "the failed generation published a shared mock home");
}

#[test]
#[ignore = "integration"]
fn the_forwarded_variables_reach_the_round_and_the_run_token_and_this_hosts_config_dir_do_not() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());

  let text = fs::read_to_string(&world.claude_env).expect("the fake claude recorded its environment");
  let env = world.claude_environment();
  let value = |name: &str| env.get(name).map(String::as_str);
  let remote_home = world.remote_home.display().to_string();

  assert_eq!(value("ANTHROPIC_BASE_URL"), Some("https://gateway.example"));
  assert_eq!(value("ANTHROPIC_MODEL"), Some("synthetic-model"));
  assert!(text.contains("ANTHROPIC_CUSTOM_HEADERS=x-tag: synthetic\nx-line: two\n"), "{text}");
  assert_eq!(value("CLAUDE_CODE_EXTRA_BODY"), Some("{\"cache\":true}"));
  assert_eq!(value("BASH_DEFAULT_TIMEOUT_MS"), Some("900000"));
  assert_eq!(value("BASH_MAX_TIMEOUT_MS"), Some("900000"));
  assert_eq!(value("CHARLIEBOT_SESSION_ID"), Some(world::SESSION_ID));
  let round_home = format!("{remote_home}/improve/home");
  assert_eq!(value("CHARLIEBOT_HOME"), Some(round_home.as_str()));
  let config_dir = format!("{remote_home}/.claude-improve");
  assert_eq!(value("CLAUDE_CONFIG_DIR"), Some(config_dir.as_str()));
  let tmpdir = value("TMPDIR").expect("the round's TMPDIR");
  assert!(
    tmpdir.starts_with(&format!("/tmp/improve-{DIRECTION}-tmp/")),
    "the round's TMPDIR is not under the session TMPDIR: {tmpdir}"
  );
  assert!(!text.contains("CHARLIEBOT_RUN_TOKEN"), "the run token reached the round:\n{text}");
  assert!(!text.contains(world::REMOTE_HOST), "a launcher setting reached the round:\n{text}");
}

#[test]
#[ignore = "integration"]
fn an_evidence_copy_past_its_limit_stops_and_the_round_still_exits_with_claudes_code() {
  let world = World::new();
  let mut env = world.env();
  env.insert("SSH_HANG_TAR".to_string(), "1".to_string());
  env.insert("IMPROVE_EVIDENCE_COPY_SECONDS".to_string(), "1".to_string());
  let start = Instant::now();
  let run = world.run(&env);
  let elapsed = start.elapsed();
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(elapsed < Duration::from_secs(20), "the launcher waited {elapsed:?} on the copy");
  assert!(world.claude_ran(), "the copy limit stopped the round itself:\n{}", run.describe());
  assert!(
    stderr(&run).contains("the evidence copy stopped (status 124)"),
    "the stopped copy did not report its timeout:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn an_approval_prompt_exits_3_inside_five_seconds() {
  let world = World::new();
  let approval = world.sandbox.root().join("approval-needed");
  fs::write(&approval, "1\n").expect("the approval marker");
  let mut env = world.env();
  env.insert("SSH_APPROVAL_FILE".to_string(), approval.display().to_string());
  let start = Instant::now();
  let run = world.run(&env);
  let elapsed = start.elapsed();
  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(elapsed < Duration::from_secs(5), "the launcher waited {elapsed:?} for the approval");
  let text = stderr(&run);
  assert!(text.contains("asks for a login approval"), "the message names no approval: {text}");
  assert!(text.contains("/host-auth"), "the message names no approval page: {}", run.describe());
  assert!(
    text.contains("https://login.tailscale.com/a/synthetic-approval"),
    "the message does not quote the login link ssh printed: {text}"
  );
  assert!(!text.contains("did not answer"), "an approval was reported as an absent host: {text}");
  assert!(!world.claude_ran(), "the round started although the connection needed an approval");
  assert!(
    !world.ssh_arguments().contains("remote-runner.sh"),
    "the launcher started the runner before the preflight passed:\n{}",
    world.ssh_arguments()
  );
}

/// A host that does not answer names the ssh error and sends the line manager to no approval page. An approval page
/// for a host that cannot answer sends the line manager to check a login instead of the host, so the preflight must
/// not print one.
#[test]
#[ignore = "integration"]
fn an_unreachable_host_names_the_ssh_error_and_no_approval_page() {
  let world = World::new();
  let error = world.sandbox.root().join("unreachable.txt");
  fs::write(&error, "ssh: connect to host synthetic-improve-host port 22: Connection timed out\n")
    .expect("the unreachable host's error");
  let mut env = world.env();
  env.insert("SSH_UNREACHABLE_FILE".to_string(), error.display().to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 3, "{}", run.describe());
  let text = stderr(&run);
  assert!(text.contains("failed at the preflight"), "the message does not name the failed ssh call:\n{text}");
  assert!(
    text.contains("ssh: connect to host synthetic-improve-host port 22: Connection timed out"),
    "the message does not quote the ssh error:\n{text}"
  );
  assert!(!text.contains("did not answer"), "the message claims a cause beyond ssh's error:\n{text}");
  assert!(!text.contains("/host-auth"), "an unreachable host sent the line manager to the approval page:\n{text}");
  assert!(!world.claude_ran(), "the round started although the host did not answer");
}

/// A host that answers but refuses the login names the ssh error and no approval page. The evidence shows a host that
/// answered, so the message claims no absent host: the defect was the blanket cause, not the approval alone.
#[test]
#[ignore = "integration"]
fn a_publickey_denial_names_the_ssh_error_and_no_approval_page() {
  let world = World::new();
  let mut env = world.env();
  env.insert("SSH_DENY_PUBLICKEY".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 3, "{}", run.describe());
  let text = stderr(&run);
  assert!(
    text.contains("synthetic-user@improve.example.ts.net: Permission denied (publickey)."),
    "the message does not quote the ssh error:\n{text}"
  );
  assert!(!text.contains("did not answer"), "the message claims an absent host although the host answered:\n{text}");
  assert!(!text.contains("/host-auth"), "a publickey denial sent the line manager to the approval page:\n{text}");
  assert!(!world.claude_ran(), "the round started although the login was denied");
}

/// A host that stops answering after the preflight gives the input send the same treatment: the ssh error, and no
/// approval page.
#[test]
#[ignore = "integration"]
fn a_failed_input_send_names_the_ssh_error_and_no_approval_page() {
  let world = World::new();
  let mut env = world.env();
  env.insert("SSH_FAIL_SEND".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 3, "{}", run.describe());
  let text = stderr(&run);
  assert!(text.contains("failed at the input send"), "the message does not name the failed ssh call:\n{text}");
  assert!(text.contains("Connection timed out"), "the message does not quote the ssh error:\n{text}");
  assert!(!text.contains("did not answer"), "the message claims a cause beyond ssh's error:\n{text}");
  assert!(!text.contains("/host-auth"), "a failed send sent the line manager to the approval page:\n{text}");
  assert!(!world.claude_ran(), "the round started although the input send failed");
}

#[test]
#[ignore = "integration"]
fn the_direction_comes_from_the_worker_session_and_its_task_file() {
  let world = World::new();
  // A task file of another session comes first in the directory. Its direction must not win.
  fs::write(
    world.sandbox.dir("profile/config.d/cron.d").join(format!("improve-{OTHER_DIRECTION}.yaml")),
    format!("prompt_file: dev/improve/{OTHER_DIRECTION}.md\nsession_id: 00000000-0000-0000-0000-000000000000\n"),
  )
  .expect("another task file");
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  let scope = fs::read_to_string(&world.systemd_run_log).expect("the scope record");
  assert!(
    scope.contains(&format!("--unit improve-{DIRECTION}")),
    "the round ran in another direction's scope: {scope}"
  );
  assert!(!scope.contains(&format!("--unit improve-{OTHER_DIRECTION}")), "{scope}");
}

#[test]
#[ignore = "integration"]
fn a_round_without_a_direction_exits_6() {
  let world = World::new();
  fs::remove_file(world.profile_home.join("config.d/cron.d").join(format!("improve-{DIRECTION}.yaml")))
    .expect("remove the task file");
  let run = world.run(&world.env());
  assert_eq!(run.code, 6, "{}", run.describe());
  assert!(!world.claude_ran(), "the round started without a direction");
  assert!(!world.ssh_arguments().contains("remote-runner.sh"), "{}", world.ssh_arguments());
}

#[test]
#[ignore = "integration"]
fn a_worker_session_without_a_parent_exits_6() {
  let world = World::new();
  let metadata = world.profile_home.join("sessions").join(world::SESSION_ID).join("metadata.json");
  fs::write(&metadata, "{\n  \"id\": \"synthetic\"\n}\n").expect("the metadata without a parent");
  let run = world.run(&world.env());
  assert_eq!(run.code, 6, "{}", run.describe());
  assert!(!world.claude_ran());
  assert!(!world.ssh_arguments().contains("remote-runner.sh"), "{}", world.ssh_arguments());
}

#[test]
#[ignore = "integration"]
fn an_existing_scope_exits_5_and_starts_no_claude() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_SYSTEMD_RUN_EXISTING".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 5, "{}", run.describe());
  assert!(!world.claude_ran(), "a round started inside the previous round's scope");
  assert!(stderr(&run).contains("previous round"), "{}", run.describe());
  let scope = fs::read_to_string(&world.systemd_run_log).expect("the scope record");
  assert!(scope.contains(&format!("--unit improve-{DIRECTION}")), "{scope}");
}

#[test]
#[ignore = "integration"]
fn a_failed_scope_is_reset_and_the_round_starts() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_SCOPE_STATE".to_string(), "failed".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the round did not start after the reset:\n{}", run.describe());
  let calls: Vec<String> = world.systemctl_calls().lines().map(str::to_string).collect();
  let failed = calls.iter().position(|call| call.contains(&format!("is-failed --quiet improve-{DIRECTION}.scope")));
  let reset = calls.iter().position(|call| call.contains(&format!("reset-failed improve-{DIRECTION}.scope")));
  let (failed, reset) = (failed.expect("no is-failed call"), reset.expect("no reset-failed call"));
  assert!(failed < reset, "the reset did not follow the failed report:\n{calls:?}");
  assert!(stderr(&run).contains(&format!("cleared the failed scope improve-{DIRECTION}.scope")), "{}", run.describe());
  let scope = fs::read_to_string(&world.systemd_run_log).expect("the scope record");
  assert!(
    scope.contains(&format!("--unit improve-{DIRECTION}")),
    "the round started no scope after the reset: {scope}"
  );
}

#[test]
#[ignore = "integration"]
fn an_active_scope_whose_round_is_over_is_stopped_and_the_round_starts() {
  let world = World::new();
  // The scope a finished round left: active, its launch directory gone, and only the processes the round left
  // in it -- two stand-ins here, as the perf round's two test children were. No process of the round is alive, so
  // the start stops the scope and names what it stopped. It still needs the name free, like a real systemd-run
  // after a stop that landed.
  let cgroup = world.scope_cgroup("leftover");
  let mut leftovers = world.spawn_scope_leftovers(&cgroup, 2);
  let leftover_pids: Vec<String> = leftovers.iter().map(|child| child.id().to_string()).collect();
  let mut env = world.env();
  env.insert("FAKE_SYSTEMD_RUN_EXISTING".to_string(), "1".to_string());
  env.insert("FAKE_SCOPE_CGROUP".to_string(), cgroup.display().to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the round did not start after the stale scope was stopped:\n{}", run.describe());
  let calls = world.systemctl_calls();
  assert!(
    calls.contains(&format!("is-active --quiet improve-{DIRECTION}.scope")),
    "the runner did not ask whether the scope is active:\n{calls}"
  );
  assert!(
    calls.contains(&format!("stop improve-{DIRECTION}.scope")),
    "the runner did not stop the stale scope:\n{calls}"
  );
  assert!(!calls.contains("reset-failed"), "the runner reset an active scope:\n{calls}");
  let text = stderr(&run);
  assert!(
    text.contains(&format!("stopped the stale scope improve-{DIRECTION}.scope")),
    "the runner did not report the stale scope it stopped:\n{}",
    run.describe()
  );
  for pid in &leftover_pids {
    assert!(text.contains(pid.as_str()), "the stopped scope's report does not name process {pid}:\n{text}");
    assert!(process_ended(pid.parse().expect("a pid")), "process {pid} outlived the stop of its scope");
  }
  // Any stand-in the stop missed ends here, so no later test sees it.
  for child in &mut leftovers {
    let _ = child.kill();
    let _ = child.wait();
  }
  let scope = fs::read_to_string(&world.systemd_run_log).expect("the scope record");
  assert!(scope.contains(&format!("--unit improve-{DIRECTION}")), "the round started no scope after the stop: {scope}");
}

#[test]
#[ignore = "integration"]
fn an_active_scope_with_a_launch_directory_but_no_leader_is_stopped_and_the_round_starts() {
  let world = World::new();
  // A launch directory alone is not a live round. Its wrapper removes it when it exits, and the
  // leader that would name it is gone. The scope's round is therefore over, and only a leftover
  // process keeps the unit loaded.
  let launch = world.remote_home.join("improve/launch/20261101T083100Z-1-100");
  fs::create_dir_all(&launch).expect("the finished round's launch directory");
  let cgroup = world.scope_cgroup("no-leader");
  fs::create_dir_all(&cgroup).expect("the stand-in cgroup's directory");
  fs::write(cgroup.join("cgroup.procs"), "").expect("the stand-in cgroup's empty process list");
  let mut env = world.env();
  env.insert("FAKE_SYSTEMD_RUN_EXISTING".to_string(), "1".to_string());
  env.insert("FAKE_SCOPE_CGROUP".to_string(), cgroup.display().to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the round did not start after the stale scope was stopped:\n{}", run.describe());
  assert!(
    stderr(&run).contains(&format!("stopped the stale scope improve-{DIRECTION}.scope")),
    "the runner did not report the stale scope it stopped:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn an_active_scope_with_a_live_round_still_refuses_with_status_5() {
  let world = World::new();
  // The previous round: its leader, the --in-scope bash that runs from its launch directory, is
  // listed in the scope's process list and is alive. The launch directory goes, because this test
  // decides on the scope's own list: the start refuses the round on that list alone, and nothing
  // stops the live round.
  let launch = world.remote_home.join("improve/launch/20261101T103100Z-1-200");
  fs::create_dir_all(&launch).expect("the live round's launch directory");
  let leader_script = launch.join("remote-runner.sh");
  let leader_started = world.sandbox.root().join("leader-started");
  fs::write(&leader_script, "#!/usr/bin/env bash\n: >\"$LEADER_STARTED\"\nsleep 30\n").expect("the leader script");
  let mut command = harness::command("bash", &world.env());
  command.env("LEADER_STARTED", &leader_started);
  command.arg(&leader_script).args(["--in-scope", "20261101T103100Z-1-200", DIRECTION]);
  let mut leader = harness::spawn(&mut command);
  // The launch directory goes while the leader runs, so the leader must have opened its script first: a bash that
  // starts after the removal finds no script and exits, and the test would exercise no live round.
  assert!(
    wait_until(Duration::from_secs(10), || leader_started.exists()),
    "the leader never started from its launch directory"
  );
  fs::remove_dir_all(&launch).expect("the live round's launch directory goes");
  let cgroup = world.scope_cgroup("live-round");
  fs::create_dir_all(&cgroup).expect("the stand-in cgroup's directory");
  fs::write(cgroup.join("cgroup.procs"), format!("{}\n", leader.id())).expect("the stand-in cgroup's process list");
  let mut env = world.env();
  env.insert("FAKE_SCOPE_CGROUP".to_string(), cgroup.display().to_string());
  let run = world.run(&env);
  // The leader's `sleep` survives a lone kill of the bash that started it: stop the children first.
  let _ = std::process::Command::new("pkill").args(["-KILL", "-P", &leader.id().to_string()]).status();
  leader.kill().expect("kill the leader");
  let _ = leader.wait();
  assert_eq!(run.code, 5, "{}", run.describe());
  assert!(!world.claude_ran(), "a round started inside the live round's scope:\n{}", run.describe());
  assert!(stderr(&run).contains("previous round"), "{}", run.describe());
  let calls = world.systemctl_calls();
  assert!(
    !calls.contains(&format!("stop improve-{DIRECTION}.scope")),
    "the runner stopped a live round's scope:\n{calls}"
  );
  assert!(!calls.contains("reset-failed"), "the runner reset a live round's scope:\n{calls}");
  assert!(
    !world.systemd_run_log.exists(),
    "the runner tried to start a scope over the live round's:\n{}",
    world.systemd_run_log.display()
  );
}

#[test]
#[ignore = "integration"]
fn a_finished_round_ends_its_scope_and_leaves_no_process() {
  let world = World::new();
  // The round starts its own scope, and its agent leaves a process in it that outlives the agent, as
  // a test run's child does. The round's end takes the scope with it: the process goes, and the
  // report names it, so the next fire of the direction finds no loaded unit and no leftover.
  let cgroup = world.scope_cgroup("round");
  let leftover_pid = world.sandbox.root().join("leftover-pid");
  let mut env = world.env();
  env.insert("FAKE_SCOPE_CGROUP".to_string(), cgroup.display().to_string());
  env.insert("FAKE_CLAUDE_LEFTOVER_CGROUP".to_string(), cgroup.display().to_string());
  env.insert("FAKE_CLAUDE_LEFTOVER_PID".to_string(), leftover_pid.display().to_string());
  let run = world.run(&env);
  let pid: u32 = fs::read_to_string(&leftover_pid)
    .unwrap_or_else(|error| panic!("the member wrote no leftover pid ({error}): {}", run.describe()))
    .trim()
    .parse()
    .expect("a pid");
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "{}", run.describe());
  let text = stderr(&run);
  assert!(
    text.contains(&format!("ended the round's scope improve-{DIRECTION}.scope")),
    "the runner did not report the scope its round ended:\n{}",
    run.describe()
  );
  assert!(text.contains(&pid.to_string()), "the round's report does not name the process {pid} it left:\n{text}");
  assert!(process_ended(pid), "process {pid} outlived its round's scope");
  let calls = world.systemctl_calls();
  assert!(
    calls.contains(&format!("stop improve-{DIRECTION}.scope")),
    "the runner did not end its round's scope:\n{calls}"
  );
}

#[test]
#[ignore = "integration"]
fn a_termed_wrapper_ends_its_scope_and_leaves_no_process() {
  let world = World::new();
  // When a signal the wrapper handles arrives while the round runs inside systemd-run, the wrapper ends its scope
  // immediately instead of waiting for the round to exit, stops every process in it, and names what it stopped.
  let cgroup = world.scope_cgroup("termed-round");
  let leftover_pid = world.sandbox.root().join("leftover-pid");
  let mut env = world.env();
  env.insert("FAKE_SCOPE_CGROUP".to_string(), cgroup.display().to_string());
  env.insert("FAKE_CLAUDE_LEFTOVER_CGROUP".to_string(), cgroup.display().to_string());
  env.insert("FAKE_CLAUDE_LEFTOVER_PID".to_string(), leftover_pid.display().to_string());
  env.insert("FAKE_CLAUDE_WAIT_FOR".to_string(), world.scope_stopped.display().to_string());
  let mut command = world.launcher_command(&env);
  command.stdin(Stdio::null());
  command.stdout(Stdio::piped());
  command.stderr(Stdio::piped());
  let launcher = harness::spawn(&mut command);

  assert!(
    wait_until(Duration::from_secs(30), || {
      fs::read_to_string(&leftover_pid).is_ok_and(|text| text.trim().parse::<u32>().is_ok())
        && world.launch_directory().is_some_and(|launch| launch.join("started").exists())
    }),
    "the round never reached its scope:\n{}",
    world.ssh_arguments()
  );
  let pid: u32 = fs::read_to_string(&leftover_pid).expect("leftover pid").trim().parse().expect("a pid");
  let launch_script = world.launch_directory().expect("the launch directory").join("remote-runner.sh");
  let launch_script_arg = launch_script.display().to_string();
  let wrapper_pid = fs::read_dir("/proc")
    .expect("/proc")
    .flatten()
    .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
    .find(|candidate| {
      let Ok(bytes) = fs::read(format!("/proc/{candidate}/cmdline")) else {
        return false;
      };
      let args: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
      args.get(1).is_some_and(|arg| *arg == launch_script_arg.as_bytes())
        && args.get(2).is_some_and(|arg| *arg != b"--in-scope" && *arg != b"--prepare")
    })
    .expect("the remote-runner wrapper process");

  // The signal goes through kill(2), not the external `kill`, whose parser drops digits of a negative operand.
  let wrapper = nix::unistd::Pid::from_raw(i32::try_from(wrapper_pid).expect("a pid fits i32"));
  nix::sys::signal::kill(wrapper, nix::sys::signal::Signal::SIGTERM)
    .unwrap_or_else(|error| panic!("kill -TERM {wrapper_pid} failed: {error}"));

  let start = Instant::now();
  let output = launcher.wait_with_output().expect("wait for launcher");
  assert!(
    start.elapsed() < Duration::from_secs(10),
    "the wrapper waited for the scope instead of ending it on SIGTERM ({:?})",
    start.elapsed()
  );
  assert_eq!(output.status.code(), Some(143), "stderr:\n{}", String::from_utf8_lossy(&output.stderr));
  let text = String::from_utf8_lossy(&output.stderr);
  assert!(
    text.contains(&format!("ended the round's scope improve-{DIRECTION}.scope")),
    "the termed wrapper did not report ending its scope:\n{text}"
  );
  assert!(text.contains(&pid.to_string()), "the termed wrapper did not name process {pid}:\n{text}");
  assert!(process_ended(pid), "process {pid} outlived the termed wrapper's scope");
}

#[test]
#[ignore = "integration"]
fn an_absent_scope_starts_without_a_reset() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let calls = world.systemctl_calls();
  assert!(
    calls.contains(&format!("is-failed --quiet improve-{DIRECTION}.scope")),
    "the runner did not ask for the scope's state:\n{calls}"
  );
  assert!(!calls.contains("reset-failed"), "the runner reset a scope it never found failed:\n{calls}");
  assert!(!stderr(&run).contains("cleared the failed scope"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_missing_direction_clone_exits_4_and_starts_no_claude() {
  let world = World::new();
  fs::remove_dir_all(world.remote_home.join("improve").join(DIRECTION).join("charlie-bot")).expect("remove the clone");
  let run = world.run(&world.env());
  assert_eq!(run.code, 4, "{}", run.describe());
  assert!(!world.claude_ran(), "the round started without its clone");
  assert!(stderr(&run).contains("clone"), "{}", run.describe());
  assert!(!world.systemd_run_log.exists(), "a scope started without the clone");
}

#[test]
#[ignore = "integration"]
fn a_clone_without_the_hooks_exits_4_and_starts_no_claude() {
  let world = World::new();
  let clone = world.remote_home.join("improve").join(DIRECTION).join("charlie-bot");
  let mut env = world.env();
  env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
  let mut command = harness::command("git", &env);
  command.arg("-C").arg(&clone).args(["config", "--unset", "core.hooksPath"]);
  let unset = harness::run(&mut command);
  assert_eq!(unset.code, 0, "{}", unset.describe());
  let run = world.run(&world.env());
  assert_eq!(run.code, 4, "{}", run.describe());
  assert!(!world.claude_ran(), "the round started in a clone without the hooks");
  assert!(stderr(&run).contains("core.hooksPath"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_killed_launcher_leaves_its_round_to_finish_on_the_host() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_WAIT_FOR".to_string(), world.round_release.display().to_string());
  // The fake ssh forwards standard input, so the launcher gets an input that ends, as `run` gives it.
  let mut command = world.launcher_command(&env);
  command.stdin(Stdio::null());
  let mut launcher = harness::spawn(&mut command);
  let started = wait_until(Duration::from_secs(30), || {
    world.launch_directory().is_some_and(|launch| launch.join("started").exists())
  });
  assert!(started, "the round never started a scope:\n{}", world.ssh_arguments());
  launcher.kill().expect("kill the launcher");
  let _ = launcher.wait();
  // A launcher whose connections end looks to the host like a dropped connection, and a dropped connection never stops a
  // round: no stop may follow the kill.
  let stopped =
    wait_until(Duration::from_secs(3), || world.systemctl_calls().contains(&format!("stop improve-{DIRECTION}.scope")));
  assert!(!stopped, "the launcher's end stopped the round's scope: {}", world.systemctl_calls());
  fs::write(&world.round_release, "").expect("release the round");
  // The round then ends its own scope and its wrapper removes the launch directory.
  assert!(
    wait_until(Duration::from_secs(30), || world.launch_directory().is_none()),
    "the round did not finish after the launcher was killed: {}",
    world.ssh_arguments()
  );
}

/// The round's record directories under the remote home, named by launch id.
fn round_records(world: &World) -> Vec<PathBuf> {
  let Ok(entries) = fs::read_dir(world.remote_home.join("improve/results")) else {
    return Vec::new();
  };
  entries.map(|entry| entry.expect("a round record entry").path()).collect()
}

#[test]
#[ignore = "integration"]
fn a_dropped_connection_reattaches_to_the_round_and_returns_its_status_and_output() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_DROP".to_string(), world.drop_trigger.display().to_string());
  env.insert("FAKE_CLAUDE_WAIT_FOR".to_string(), world.round_release.display().to_string());
  env.insert("FAKE_CLAUDE_EXIT".to_string(), "7".to_string());
  env.insert("SSH_DROP_START_AFTER".to_string(), world.drop_trigger.display().to_string());
  // The first state query finds the round running, and releases it once the query has answered.
  env.insert("SSH_RELEASE_AFTER_STATE".to_string(), world.round_release.display().to_string());
  env.insert("IMPROVE_REATTACH_POLL_SECONDS".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 7, "the launch returns the round's own exit status: {}", run.describe());
  assert_eq!(
    run.stdout.trim_start_matches('\n'),
    "first part\nfinal part\n",
    "the launch passes on the round's output once, before and after the drop: {}",
    run.describe()
  );
  assert_eq!(world.cli_runs().len(), 1, "a dropped connection started a second round: {}", world.ssh_arguments());
  let calls = world.ssh_arguments();
  assert!(calls.matches("--round-state").count() >= 2, "the launch did not wait for the round's end: {calls}");
  assert!(calls.contains("--round-output"), "the launch did not fetch the round's output: {calls}");
  assert!(calls.contains("--round-release"), "the launch did not release the round's record: {calls}");
  assert_eq!(
    fs::read_to_string(world.local_evidence().join("round-result.txt")).expect("the round's evidence copy"),
    "the round result\n",
    "the launch did not copy the round's evidence"
  );
  assert!(round_records(&world).is_empty(), "the launch left its round record on the host");
}

#[test]
#[ignore = "integration"]
fn a_host_unreachable_past_the_reattach_bound_fails_while_the_round_runs() {
  let world = World::new();
  let mut env = world.env();
  env.insert("FAKE_CLAUDE_DROP".to_string(), world.drop_trigger.display().to_string());
  env.insert("FAKE_CLAUDE_UNREACHABLE".to_string(), world.host_unreachable.display().to_string());
  env.insert("SSH_UNREACHABLE_FILE".to_string(), world.host_unreachable.display().to_string());
  env.insert("SSH_DROP_START_AFTER".to_string(), world.drop_trigger.display().to_string());
  env.insert("FAKE_CLAUDE_WAIT_FOR".to_string(), world.round_release.display().to_string());
  env.insert("FAKE_CLAUDE_EXIT".to_string(), "7".to_string());
  env.insert("IMPROVE_REATTACH_SECONDS".to_string(), "2".to_string());
  env.insert("IMPROVE_REATTACH_POLL_SECONDS".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(
    stderr(&run).contains("stayed unreachable for 2 s"),
    "the launch does not name the bound it stopped at: {}",
    run.describe()
  );
  assert!(
    stderr(&run).contains("improve/results/"),
    "the launch does not name where the result is: {}",
    run.describe()
  );
  assert_eq!(run.stdout.trim_start_matches('\n'), "first part\n", "{}", run.describe());
  assert!(!world.round_release.exists(), "the round was released before the launch gave up");
  assert!(
    !world.systemctl_calls().contains("stop improve-"),
    "the launch stopped the round's scope: {}",
    world.systemctl_calls()
  );
  // The round still runs on the host: once it is released it ends with its own status and output in the record.
  fs::write(&world.round_release, "").expect("release the round");
  let records = round_records(&world);
  assert_eq!(records.len(), 1, "the round's record is missing: {}", world.ssh_arguments());
  let status = records[0].join("status");
  assert!(wait_until(Duration::from_secs(30), || status.exists()), "the round did not end on the host");
  assert_eq!(fs::read_to_string(&status).expect("the round's status"), "7\n");
  assert_eq!(
    fs::read_to_string(records[0].join("output")).expect("the round's output").trim_start_matches('\n'),
    "first part\nfinal part\n",
    "the round's record lacks the output after the drop"
  );
}

#[test]
#[ignore = "integration"]
fn a_dropped_connection_whose_round_left_no_status_exits_9_and_names_the_launch() {
  let world = World::new();
  let mut env = world.env();
  // The connection drops before the host runs the start: no runner, no status.
  env.insert("SSH_DROP_BEFORE_START".to_string(), "1".to_string());
  let run = world.run(&env);
  assert_eq!(run.code, 9, "{}", run.describe());
  assert!(stderr(&run).contains("left no result"), "{}", run.describe());
  assert!(stderr(&run).contains("improve-test_perf.scope"), "{}", run.describe());
  assert!(!world.claude_ran(), "the round started after its start had dropped");
}

/// A fake live round of `direction`: a leader whose command line the runner's round state reads, beside the
/// launch directory that command line names. The caller stops the leader after the run.
fn spawn_live_round(world: &World, direction: &str) -> std::process::Child {
  let launch_id = "20261103T120000Z-9-900";
  let launch = world.remote_home.join("improve/launch").join(launch_id);
  fs::create_dir_all(&launch).expect("the live round's launch directory");
  fs::write(launch.join("remote-runner.sh"), "#!/usr/bin/env bash\nsleep 30\n").expect("the leader script");
  let mut env = harness::Env::new();
  env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
  let mut command = harness::command("bash", &env);
  command.arg(launch.join("remote-runner.sh")).args(["--in-scope", launch_id, direction]);
  harness::spawn(&mut command)
}

/// The stop of a leader that [`spawn_live_round`] started: the children first, since a lone kill of the
/// bash that started them leaves them running.
fn stop_live_round(leader: &mut std::process::Child) {
  let _ = std::process::Command::new("pkill").args(["-KILL", "-P", &leader.id().to_string()]).status();
  leader.kill().expect("stop the fake round's leader");
  let _ = leader.wait();
}

#[test]
#[ignore = "integration"]
fn a_running_rounds_clone_adds_its_checked_out_branchs_files_to_the_list() {
  let world = World::new();
  // A standalone clone of another direction, like the host worker's five clones on the remote host.
  let clone = world.other_direction_clone(OTHER_DIRECTION);
  // A file whose removal main lands after the topic branch's base, as a rename's old name does.
  fs::write(clone.join("dropped.md"), "old\n").expect("the dropped fixture file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "add the dropped file"]);
  // The committed change rides the checked-out topic branch, as a round's branch does, and it
  // touches the old path beside the live one.
  world.git(&clone, &["switch", "-q", "-c", &format!("improve/{OTHER_DIRECTION}/topic")]);
  fs::write(clone.join("committed.md"), "topic\n").expect("the committed fixture file");
  fs::write(clone.join("dropped.md"), "topic\n").expect("the dropped path's topic change");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic topic"]);
  // main deletes the old path, as a rename's landing does.
  world.git(&clone, &["switch", "-q", "main"]);
  world.git(&clone, &["rm", "-q", "dropped.md"]);
  world.git(&clone, &["commit", "-q", "-m", "drop the old name"]);
  world.git(&clone, &["push", "-q", "origin", "main"]);
  world.git(&clone, &["switch", "-q", &format!("improve/{OTHER_DIRECTION}/topic")]);
  // The uncommitted change sits in the checkout. The untracked file has no git entry at all.
  fs::write(clone.join("base.md"), "changed\n").expect("the uncommitted change");
  fs::write(clone.join("dropped.md"), "uncommitted\n").expect("the dropped path's uncommitted change");
  fs::write(clone.join("untracked.md"), "new\n").expect("the untracked file");
  // The ux round is running while this perf round builds its list, as a burst's rounds do.
  let mut leader = spawn_live_round(&world, OTHER_DIRECTION);

  let run = world.run(&world.env());

  stop_live_round(&mut leader);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  // Every path the running ux round's clone holds prints with its branch: the committed paths
  // carry unmerged commits, and the uncommitted and untracked paths are uncommitted work. The paths
  // that origin/main lacks carry the absent field; base.md is on origin/main.
  for carried in [
    format!("committed.md\timprove/{OTHER_DIRECTION}/topic\tabsent-on-origin-main"),
    format!("base.md\timprove/{OTHER_DIRECTION}/topic"),
    format!("dropped.md\timprove/{OTHER_DIRECTION}/topic\tabsent-on-origin-main"),
    format!("untracked.md\timprove/{OTHER_DIRECTION}/topic\tabsent-on-origin-main"),
  ] {
    assert!(
      list.lines().any(|line| line == carried),
      "the in-flight list misses the {carried} change of the running ux round's clone:\n{list}"
    );
  }
}

#[test]
#[ignore = "integration"]
fn an_open_pull_requests_files_appear_in_the_in_flight_list() {
  let world = World::new();
  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "12\tcharliebot/task-open\tpr-only.md\n").expect("the open pull requests");
  let mut env = world.env();
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());

  let run = world.run(&env);

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(
    list.lines().any(|line| line == "pr-only.md\tcharliebot/task-open\tabsent-on-origin-main"),
    "the in-flight list misses an open pull request's file:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn a_failing_pull_request_files_api_call_stops_the_list_with_an_error() {
  let world = World::new();
  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "12\tcharliebot/task-open\tpr-only.md\n").expect("the open pull requests");
  let mut env = world.env();
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());
  env.insert("FAKE_GH_FAIL_FILES".to_string(), "1".to_string());

  let run = run_inflight_list_with_env(&world, &env);

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(
    run.stderr.contains("cannot read the open pull request condition for PR 12"),
    "the error does not name the PR condition:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_squash_merged_branch_of_an_unowned_worktree_names_no_landed_path() {
  let world = World::new();
  // A leftover branch of a round that already landed, checked out in its worktree as the host leaves it.
  let checkout = world.sandbox.root().join("stale-checkout");
  world.git(&world.repo, &["branch", "charliebot/stale"]);
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/stale"]);
  fs::write(checkout.join("stale.md"), "landed\n").expect("the landed topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic stale topic"]);
  // origin/main receives the topic as a squash merge, and a later change moves the same file on, as a later
  // round does. The launcher's tracking ref still points behind both, so only the script's fetch sees them.
  world.git(&world.repo, &["merge", "-q", "--squash", "charliebot/stale"]);
  world.git(&world.repo, &["commit", "-q", "-m", "land the stale topic"]);
  fs::write(world.repo.join("stale.md"), "landed\nand moved on\n").expect("the later change");
  world.git(&world.repo, &["commit", "-q", "-am", "move the landed file on"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  // The branch stays live through its commit ahead of origin/main, but main carries that commit's patch, so its
  // path is landed work and the list names it in no form.
  assert!(!lists_path(&list, "stale.md"), "the in-flight list names a landed path of an unowned worktree:\n{list}");
}

#[test]
#[ignore = "integration"]
fn an_unlanded_branch_names_its_file_even_when_origin_main_has_changed_it() {
  let world = World::new();
  // A leftover branch whose commit never reached origin/main, checked out in its worktree as the host leaves it.
  let checkout = world.sandbox.root().join("unlanded-checkout");
  world.git(&world.repo, &["branch", "charliebot/unlanded"]);
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/unlanded"]);
  fs::write(checkout.join("shared.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic unlanded topic"]);
  // Main changes the same file after the branch, while the unowned worktree keeps its committed path.
  fs::write(world.repo.join("shared.md"), "another change\n").expect("the other change");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "change the shared file on main"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(
    list.lines().any(|line| line == "shared.md\tcharliebot/unlanded"),
    "the in-flight list misses an unlanded branch's file that origin/main changed:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn a_branch_does_not_claim_a_path_it_restores_to_its_merge_base() {
  let world = World::new();
  fs::write(world.repo.join("restored.md"), "base\n").expect("the base file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "add the base file"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  let checkout = world.sandbox.root().join("restored-checkout");
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", "charliebot/restored"]);

  fs::write(checkout.join("restored.md"), "temporary change\n").expect("the temporary change");
  world.git(&checkout, &["commit", "-q", "-am", "change the file"]);
  fs::write(checkout.join("restored.md"), "base\n").expect("restore the base contents");
  world.git(&checkout, &["commit", "-q", "-am", "restore the file"]);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    !lists_path(&run.stdout, "restored.md"),
    "the branch claims a path that its final tree restores to the merge base:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn an_unowned_worktree_branch_keeps_all_changed_paths() {
  let world = World::new();
  // The branch changes four paths: two additions and two deletions.
  fs::write(world.repo.join("base.md"), "base\n").expect("the base file");
  fs::write(world.repo.join("together.md"), "together\n").expect("the together file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "add the deletable files"]);
  world.git(&world.repo, &["branch", "charliebot/partial"]);
  let checkout = world.sandbox.root().join("partial-checkout");
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/partial"]);
  // Main later lands one added path and one deletion, but the unowned worktree keeps all four changes.
  fs::write(checkout.join("landed.md"), "landed\n").expect("the landed topic's file");
  fs::write(checkout.join("unlanded.md"), "unlanded\n").expect("the unlanded topic's file");
  fs::remove_file(checkout.join("base.md")).expect("remove the base file");
  fs::remove_file(checkout.join("together.md")).expect("remove the together file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic partial topic"]);
  // Main later lands the added path, changes it again, and deletes one path from the branch.
  fs::write(world.repo.join("landed.md"), "landed\n").expect("the landed version");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "land the partial topic's file"]);
  fs::write(world.repo.join("landed.md"), "landed\nand moved on\n").expect("the later change");
  world.git(&world.repo, &["commit", "-q", "-am", "move the landed file on"]);
  fs::remove_file(world.repo.join("together.md")).expect("remove the together file on main");
  world.git(&world.repo, &["commit", "-q", "-am", "drop the file the branch drops too"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  let mut listed: Vec<&str> = list.lines().collect();
  listed.sort_unstable();
  // The branch's one commit carries an unmerged patch, so all four paths it touches print with the
  // branch that carries them.
  assert_eq!(
    listed,
    [
      "base.md\tcharliebot/partial",
      "landed.md\tcharliebot/partial",
      "together.md\tcharliebot/partial\tabsent-on-origin-main",
      "unlanded.md\tcharliebot/partial\tabsent-on-origin-main"
    ],
    "the unowned branch loses a changed path:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn an_unowned_rebased_branch_keeps_its_committed_path() {
  let world = World::new();
  // A leftover branch of a round that a rebase landed, checked out in its worktree as the host leaves it.
  let checkout = world.sandbox.root().join("rebased-checkout");
  world.git(&world.repo, &["branch", "charliebot/rebased"]);
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/rebased"]);
  fs::write(checkout.join("rebased.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic rebased topic"]);
  // Main commits the same subject with a resolved file value.
  fs::write(world.repo.join("rebased.md"), "topic, resolved\n").expect("the resolved file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "synthetic rebased topic"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  // Main's resolved commit is not the branch's patch, so the branch still carries an unmerged
  // commit for the path, and the path prints with that branch.
  assert!(
    list.lines().any(|line| line == "rebased.md\tcharliebot/rebased"),
    "the unowned worktree drops its committed path:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn an_idle_clones_branches_name_no_path_and_the_branch_stays() {
  let world = World::new();
  let clone = world.other_direction_clone(OTHER_DIRECTION);
  // The checked-out branch of a round that ended, with its committed, uncommitted and untracked
  // paths still in the checkout, as the host leaves a finished clone.
  world.git(&clone, &["switch", "-q", "-c", &format!("improve/{OTHER_DIRECTION}/ended")]);
  fs::write(clone.join("committed.md"), "topic\n").expect("the committed file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic ended topic"]);
  fs::write(clone.join("dirty.md"), "changed\n").expect("the uncommitted change");
  fs::write(clone.join("untracked.md"), "new\n").expect("the untracked file");
  // An older round's branch that no checkout carries.
  world.git(&clone, &["switch", "-q", "main"]);
  world.git(&clone, &["switch", "-q", "-c", &format!("improve/{OTHER_DIRECTION}/old")]);
  fs::write(clone.join("old.md"), "old\n").expect("the old branch's file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic old topic"]);
  world.git(&clone, &["switch", "-q", &format!("improve/{OTHER_DIRECTION}/ended")]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(list.trim().is_empty(), "the idle clone still claims paths without a running round:\n{list}");
  assert!(
    !world.git(&clone, &["branch", "--list", &format!("improve/{OTHER_DIRECTION}/old")]).trim().is_empty(),
    "the old branch left the clone"
  );
}

#[test]
#[ignore = "integration"]
fn a_running_rounds_clone_keeps_all_changed_paths() {
  let world = World::new();
  let clone = world.other_direction_clone(OTHER_DIRECTION);
  // The topic adds two files and deletes `base.md`, which origin/main never drops.
  world.git(&clone, &["switch", "-q", "-c", &format!("improve/{OTHER_DIRECTION}/partial")]);
  fs::write(clone.join("landed.md"), "landed\n").expect("the landed topic's file");
  fs::write(clone.join("unlanded.md"), "unlanded\n").expect("the unlanded topic's file");
  fs::remove_file(clone.join("base.md")).expect("remove the base file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic partial topic"]);
  // Main later lands the added path, then changes it again.
  let origin = world.other_direction_origin(OTHER_DIRECTION).display().to_string();
  let landing = world.sandbox.dir(&format!("landing/{OTHER_DIRECTION}"));
  world.git(&world.sandbox.dir("landing"), &["clone", "-q", &origin, OTHER_DIRECTION]);
  fs::write(landing.join("landed.md"), "landed\n").expect("the landed version");
  world.git(&landing, &["add", "-A"]);
  world.git(&landing, &["commit", "-q", "-m", "land the partial topic's file"]);
  fs::write(landing.join("landed.md"), "landed\nand moved on\n").expect("the later change");
  world.git(&landing, &["commit", "-q", "-am", "move the landed file on"]);
  world.git(&landing, &["push", "-q", "origin", "main"]);
  // The ux round is running while this perf round builds its list.
  let mut leader = spawn_live_round(&world, OTHER_DIRECTION);

  let run = world.run(&world.env());

  stop_live_round(&mut leader);
  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  let mut listed: Vec<&str> = list.lines().collect();
  listed.sort_unstable();
  assert_eq!(
    listed,
    [
      format!("base.md\timprove/{OTHER_DIRECTION}/partial"),
      format!("landed.md\timprove/{OTHER_DIRECTION}/partial"),
      format!("unlanded.md\timprove/{OTHER_DIRECTION}/partial\tabsent-on-origin-main"),
    ],
    "the live clone loses a changed path:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn a_process_in_another_directions_clone_keeps_its_checked_out_branchs_paths() {
  let world = World::new();
  let clone = world.other_direction_clone(OTHER_DIRECTION);
  world.git(&clone, &["switch", "-q", "-c", &format!("improve/{OTHER_DIRECTION}/live")]);
  fs::write(clone.join("committed.md"), "topic\n").expect("the committed file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic live topic"]);
  fs::write(clone.join("dirty.md"), "changed\n").expect("the uncommitted change");
  // No ux round runs, but a process works in the clone, as a round's worker does.
  let mut process = std::process::Command::new("bash")
    .arg("-c")
    .arg("exec sleep 30")
    .current_dir(&clone)
    .spawn()
    .expect("the clone process");
  let cwd = format!("/proc/{}/cwd", process.id());
  assert!(
    wait_until(Duration::from_secs(5), || fs::read_link(&cwd).is_ok_and(|target| target == clone)),
    "the clone process did not enter the clone"
  );

  let run = world.run(&world.env());

  process.kill().expect("stop the clone process");
  process.wait().expect("reap the clone process");
  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  for named in [
    format!("committed.md\timprove/{OTHER_DIRECTION}/live\tabsent-on-origin-main"),
    format!("dirty.md\timprove/{OTHER_DIRECTION}/live\tabsent-on-origin-main"),
  ] {
    assert!(list.lines().any(|line| line == named), "the in-flight list drops the process-held path {named}:\n{list}");
  }
}

#[test]
#[ignore = "integration"]
fn an_other_directions_clone_whose_fetch_fails_stops_the_round_with_exit_1() {
  let world = World::new();
  let clone = world.other_direction_clone(OTHER_DIRECTION);
  // The clone's origin is gone, so the fetch of its origin/main fails. The round must stop rather
  // than build its in-flight list without the ux clone's files.
  let gone = world.sandbox.root().join("gone.git").display().to_string();
  world.git(&clone, &["remote", "set-url", "origin", &gone]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(
    stderr(&run).contains(&format!("cannot fetch origin/main in {}", clone.display())),
    "the round did not name the clone it could not fetch:\n{}",
    run.describe()
  );
  assert!(!world.claude_ran(), "the round started without the ux clone's in-flight files");
}

#[test]
#[ignore = "integration"]
fn the_runners_own_checkout_contributes_no_files() {
  let world = World::new();
  // The runner's checkout, with mainline work that its remote's main does not hold.
  let runner_root = world.remote_home.join("improve/runner");
  let clone = runner_root.join("charlie-bot");
  world.git(&world.sandbox.dir("seeds/runner"), &["init", "-q", "-b", "main"]);
  fs::write(world.sandbox.root().join("seeds/runner/seed.md"), "seed\n").expect("the seed's file");
  world.git(&world.sandbox.dir("seeds/runner"), &["add", "-A"]);
  world.git(&world.sandbox.dir("seeds/runner"), &["commit", "-q", "-m", "synthetic base"]);
  let origin = world.sandbox.dir("remotes/runner.git").display().to_string();
  world.git(&world.sandbox.dir("seeds/runner"), &["clone", "-q", "--bare", ".", &origin]);
  world.git(&runner_root, &["clone", "-q", &origin, "charlie-bot"]);
  fs::write(clone.join("runner-mainline.md"), "runner\n").expect("the runner's mainline file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic runner mainline"]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(
    !list.lines().any(|line| line == "runner-mainline.md"),
    "the in-flight list names the runner's own checkout:\n{list}"
  );
}

#[test]
#[ignore = "integration"]
fn the_runner_builds_the_round_tools_with_its_own_cargo_home_and_tmpdir_and_the_launchers_origin() {
  let world = World::new();
  // The commit the runner builds needs a rust/ directory, and it reaches the launcher's origin, which the runner
  // clones from.
  fs::create_dir_all(world.repo.join("rust")).expect("the rust directory");
  fs::write(world.repo.join("rust/Cargo.toml"), "[workspace]\n").expect("the workspace file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "synthetic build commit"]);
  let commit = world.git(&world.repo, &["rev-parse", "HEAD"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  let origin_text = world.other_direction_origin("runtime").display().to_string();
  let cargo_env = world.sandbox.root().join("cargo-env.txt");
  let mut env = world.env();
  env.insert("FAKE_CARGO_ENV".to_string(), cargo_env.display().to_string());

  let run = world.run(&env);

  assert_eq!(run.code, 0, "{}", run.describe());
  let recorded = fs::read_to_string(&cargo_env).expect("the fake cargo ran");
  let cargo_home = world.remote_home.join("improve/runner/cargo-home");
  let tmp = world.remote_home.join("improve/runner/tmp");
  assert!(recorded.contains(&format!("CARGO_HOME={}\n", cargo_home.display())), "{recorded}");
  assert!(recorded.contains(&format!("TMPDIR={}\n", tmp.display())), "{recorded}");
  let runner_bin = world.remote_home.join("improve/runner/bin");
  for tool in ["charliebot", "improve-sandbox", "mock-home"] {
    assert!(runner_bin.join(format!("{tool}-{commit}")).is_file(), "the runner did not install {tool}");
  }
  let clone = world.remote_home.join("improve/runner/charlie-bot");
  assert_eq!(world.git(&clone, &["remote", "get-url", "origin"]), origin_text, "the runner cloned another origin");
}

#[test]
#[ignore = "integration"]
fn a_round_reads_and_runs_the_kit_of_its_charliebots_commit_when_origin_main_is_newer() {
  let world = World::new();
  // The direction's origin/main moves past the pinned commit, and its posting script gains a line the pinned one lacks.
  let clone = world.remote_home.join("improve").join(DIRECTION).join("charlie-bot");
  let pinned = fs::read_to_string(world.repo.join("dev/improve/github-post.sh")).expect("the pinned posting script");
  let newer = "# A line that only origin/main holds.\n";
  fs::write(clone.join("dev/improve/github-post.sh"), format!("{pinned}{newer}")).expect("the newer posting script");
  world.git(&clone, &["commit", "-qam", "newer posting script"]);
  // The pre-push hook checks each push against the profile home's leak patterns, which this fixture does not set up.
  world.git(&clone, &["push", "-q", "--no-verify", "origin", "HEAD:main"]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the round did not start:\n{}", run.describe());
  let bin = world.remote_home.join("improve/runner/bin").join(&world.commit);
  let kit = bin.join("kit");
  assert_eq!(
    fs::read_to_string(kit.join("dev/improve/github-post.sh")).expect("the kit's posting script"),
    pinned,
    "the kit is not the pinned commit's posting script"
  );
  assert_eq!(
    fs::read_to_string(clone.join("dev/improve/github-post.sh")).expect("the clone's posting script"),
    format!("{pinned}{newer}"),
    "the clone is not at origin/main"
  );
  let env = world.claude_environment();
  assert_eq!(env.get("IMPROVE_KIT_REVISION"), Some(&world.commit));
  assert_eq!(env.get("IMPROVE_KIT_DIR"), Some(&kit.display().to_string()));
  let path = env.get("PATH").expect("the round's PATH");
  assert!(path.split(':').any(|entry| Path::new(entry) == bin.as_path()), "the PATH holds no tool directory: {path}");
  let charliebot = fs::canonicalize(bin.join("charliebot")).expect("the round's charliebot");
  assert!(charliebot.ends_with(format!("charliebot-{}", world.commit)), "{}", charliebot.display());
}

/// Runs the round's wrapper, `remote-runner.sh`, on a launch staged under `launch_id`, with the arguments the start sends.
fn run_round_wrapper(world: &World, launch_id: &str) -> harness::Run {
  let mut env = world.env();
  env.insert("HOME".to_string(), world.remote_home.display().to_string());
  let mut command = harness::command("bash", &env);
  command.arg(world.repo.join("dev/improve/remote-runner.sh")).args([
    launch_id,
    DIRECTION,
    world.commit.as_str(),
    "claude",
    LOCAL_CLAUDE_VERSION,
    "claude",
    LOCAL_CLAUDE_VERSION,
  ]);
  harness::run(&mut command)
}

#[test]
#[ignore = "integration"]
fn a_round_loads_the_skills_of_its_charliebots_commit_when_origin_main_is_newer() {
  let world = World::new();
  // The direction's origin/main moves past the pinned commit, and its rule file gains a line the pinned one lacks.
  let clone = world.remote_home.join("improve").join(DIRECTION).join("charlie-bot");
  let rule = clone.join("skills/writing-style/genres/code.md");
  let newer = format!("{PINNED_RULE}{NEWER_RULE}");
  fs::write(&rule, &newer).expect("the newer rule file");
  world.git(&clone, &["commit", "-qam", "newer rule file"]);
  // The pre-push hook checks each push against the profile home's leak patterns, which this fixture does not set up.
  world.git(&clone, &["push", "-q", "--no-verify", "origin", "HEAD:main"]);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the round did not start:\n{}", run.describe());
  let skills = world.remote_home.join("improve/runner/bin").join(&world.commit).join("skills");
  let env = world.claude_environment();
  assert_eq!(env.get("IMPROVE_SKILLS_DIR"), Some(&skills.display().to_string()));
  assert_eq!(
    fs::read_to_string(skills.join("writing-style/genres/code.md")).expect("the round's rule file"),
    PINNED_RULE,
    "the round's skills are not the skills of its charliebot's revision"
  );
  assert_eq!(fs::read_to_string(&rule).expect("the clone's rule file"), newer, "the clone is not at origin/main");
}

#[test]
#[ignore = "integration"]
fn a_launch_whose_skills_name_another_revision_than_its_charliebot_stops_before_any_side_effect() {
  let world = World::new();
  let launch = world.stage_launch("skills-mismatch");
  let skills_revision = "2".repeat(40);
  fs::write(launch.join("skills-revision"), format!("{skills_revision}\n")).expect("the launch's skills revision");

  let run = run_round_wrapper(&world, "skills-mismatch");

  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(
    stderr(&run).contains(&skills_revision) && stderr(&run).contains(&world.commit),
    "the refusal does not name both revisions: {}",
    run.describe()
  );
  assert!(!world.scope_started(), "the refused round started a scope:\n{}", world.systemctl_calls());
  assert!(!world.claude_ran(), "the refused round started claude");
}

#[test]
#[ignore = "integration"]
fn a_launch_whose_kit_names_another_revision_than_its_charliebot_stops_before_any_side_effect() {
  let world = World::new();
  let launch = world.stage_launch("kit-mismatch");
  let kit_revision = "1".repeat(40);
  fs::write(launch.join("kit-revision"), format!("{kit_revision}\n")).expect("the launch's kit revision");

  let run = run_round_wrapper(&world, "kit-mismatch");

  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(
    stderr(&run).contains(&kit_revision) && stderr(&run).contains(&world.commit),
    "the refusal does not name both revisions: {}",
    run.describe()
  );
  assert!(!world.scope_started(), "the refused round started a scope:\n{}", world.systemctl_calls());
  assert!(!world.claude_ran(), "the refused round started claude");
}

#[test]
#[ignore = "integration"]
fn a_launcher_checkout_whose_dev_improve_differs_from_head_exits_7_before_any_input_is_sent() {
  let world = World::new();
  let script = world.repo.join("dev/improve/clone-check.sh");
  let edited = format!("{}# An edit that HEAD does not hold.\n", fs::read_to_string(&script).expect("the clone check"));
  fs::write(&script, edited).expect("the edited clone check");

  let run = world.run(&world.env());

  assert_refused_before_input(&world, &run, "differs from HEAD");
}

#[test]
#[ignore = "integration"]
fn two_concurrent_runners_build_tools_once_and_preserve_one_shared_mock_home() {
  let world = World::new();
  // The commit both rounds need carries a rust/ directory, as the build's `cd` needs, and sits on
  // an origin the runner can clone from.
  fs::create_dir_all(world.repo.join("rust")).expect("the rust directory");
  fs::write(world.repo.join("rust/Cargo.toml"), "[workspace]\n").expect("the workspace file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "synthetic build commit"]);
  let commit = world.git(&world.repo, &["rev-parse", "HEAD"]);
  let origin = world.sandbox.dir("remotes/build/charlie-bot.git");
  let origin_text = origin.display().to_string();
  world.git(&world.repo, &["clone", "-q", "--bare", ".", &origin_text]);
  let cargo_log = world.sandbox.root().join("cargo-builds.log");
  let cargo_env = world.sandbox.root().join("cargo-env.txt");
  let mock_home_barrier = world.sandbox.dir("mock-home-barrier");
  fs::write(
    world.remote_home.join("improve/home/credentials.yaml"),
    format!("{REVIEW_CREDENTIAL}:\n  api_key: {REVIEW_KEY}\n"),
  )
  .expect("the round's credentials file");
  // The round home keeps the pattern file of the previous round, as the host's does. Both
  // runners' copies therefore take the in-place path instead of racing on a fresh file.
  fs::write(world.remote_home.join("improve/home/skills_leak_patterns.local.txt"), "").expect("the pattern file");

  // Two directions start in the same second, and both need the commit that the runner bin lacks.
  // Each direction brings its own clone, whose origin the other direction's runner can fetch, and
  // its own launch directory, records and TMPDIR.
  let mut commands = Vec::new();
  for (index, direction) in [DIRECTION, OTHER_DIRECTION].into_iter().enumerate() {
    let bare = world.sandbox.dir(&format!("remotes/{index}/charlie-bot.git"));
    let bare_text = bare.display().to_string();
    world.git(&world.repo, &["clone", "-q", "--bare", ".", &bare_text]);
    let round_root = world.remote_home.join("improve").join(direction);
    let clone = round_root.join("charlie-bot");
    if direction == DIRECTION {
      // The fixture clone's origin names no fetchable repository, and the other direction's
      // runner fetches in this clone, so the origin becomes the local bare one.
      world.git(&clone, &["remote", "set-url", "origin", &bare_text]);
    } else {
      fs::create_dir_all(&round_root).expect("the ux round root");
      world.git(&round_root, &["clone", "-q", &bare_text, "charlie-bot"]);
      world.git(&clone, &["config", "core.hooksPath", "dev/git-hooks"]);
    }
    let launch_id = format!("20261102T090000Z-1-40{index}");
    let launch = world.stage_launch(&launch_id);
    // This test's round runs the commit it built, so its launch names that commit as the kit and skills revision.
    fs::write(launch.join("kit-revision"), format!("{commit}\n")).expect("the launch's kit revision");
    fs::write(launch.join("skills-revision"), format!("{commit}\n")).expect("the launch's skills revision");
    harness::copy_file(&harness::repo_root().join("dev/improve/remote-runner.sh"), &launch.join("remote-runner.sh"));
    // The launcher sends the rule script beside the runner, so the runner's clone scan sources it.
    harness::copy_file(&harness::repo_root().join("dev/improve/inflight-files.sh"), &launch.join("inflight-files.sh"));
    fs::write(launch.join("args"), "").expect("the launch arguments");
    write_member_credential(&launch, &[("ANTHROPIC_AUTH_TOKEN", GATEWAY_TOKEN)]);
    fs::write(launch.join("env"), "").expect("the launch environment");
    fs::write(launch.join("paths"), "").expect("the launch paths");
    fs::write(launch.join("origin"), format!("{origin_text}\n")).expect("the launch origin");
    fs::write(launch.join("skills_leak_patterns.local.txt"), "fictionalmarker\n").expect("the leak patterns");
    fs::create_dir_all(format!("/tmp/improve-{direction}-tmp")).expect("the round's TMPDIR");
    let records = world.sandbox.dir(&format!("records-{direction}"));
    let mut env = harness::Env::new();
    env.insert("HOME".to_string(), world.remote_home.display().to_string());
    env.insert(
      "PATH".to_string(),
      format!("{}:/usr/local/bin:/usr/bin:/bin", world.remote_home.join(".local/bin").display()),
    );
    env.insert("FAKE_CLAUDE_ARGV".to_string(), records.join("claude-argv.txt").display().to_string());
    env.insert("FAKE_CLAUDE_ENV".to_string(), records.join("claude-env.txt").display().to_string());
    env.insert("FAKE_SYSTEMCTL_LOG".to_string(), world.systemctl_log.display().to_string());
    env.insert("FAKE_SYSTEMD_RUN_LOG".to_string(), world.systemd_run_log.display().to_string());
    env.insert("FAKE_SCOPE_STOPPED".to_string(), world.scope_stopped.display().to_string());
    env.insert("FAKE_CARGO_ENV".to_string(), cargo_env.display().to_string());
    env.insert("FAKE_CARGO_LOG".to_string(), cargo_log.display().to_string());
    env.insert("FAKE_MOCK_HOME_LOG".to_string(), world.mock_home_log.display().to_string());
    env.insert("FAKE_MOCK_HOME_FIXTURE".to_string(), world.mock_home_fixture.display().to_string());
    env.insert(
      "FAKE_MOCK_HOME_DIR".to_string(),
      world.remote_home.join("improve/mock-home").join(world::MOCK_HOME_VERSION).display().to_string(),
    );
    env.insert("FAKE_CLAUDE_BARRIER".to_string(), mock_home_barrier.display().to_string());
    env.insert("FAKE_VENDOR_ROOT".to_string(), world.vendor_root.display().to_string());
    env.insert("FAKE_CURL_LOG".to_string(), world.curl_log.display().to_string());
    env.insert("FAKE_CLI_RUNS".to_string(), world.cli_runs.display().to_string());
    // The build runs long enough that the second runner arrives while the first one holds the
    // work, as two burst starts do on the host.
    env.insert("FAKE_CARGO_SLEEP".to_string(), "2".to_string());
    env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
    env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
    let mut command = harness::command("bash", &env);
    command.arg(launch.join("remote-runner.sh")).args([
      &launch_id,
      direction,
      &commit,
      "claude",
      world::LOCAL_CLAUDE_VERSION,
      "claude",
      world::LOCAL_CLAUDE_VERSION,
    ]);
    commands.push(command);
  }

  let runs: Vec<harness::Run> = std::thread::scope(|scope| {
    commands
      .iter_mut()
      .map(|command| scope.spawn(move || harness::run(command)))
      .collect::<Vec<_>>()
      .into_iter()
      .map(|handle| handle.join().expect("the runner thread"))
      .collect()
  });

  for run in &runs {
    assert_eq!(run.code, 0, "{}", run.describe());
  }
  let mock_home = world.remote_home.join("improve/mock-home").join(world::MOCK_HOME_VERSION);
  let mock_home_calls = fs::read_to_string(&world.mock_home_log).expect("the mock-home calls");
  assert_eq!(mock_home_calls.lines().filter(|line| line.starts_with("generate ")).count(), 1, "{mock_home_calls}");
  assert_eq!(
    fs::read_to_string(mock_home.join("manifest.json")).expect("the shared mock-home manifest"),
    "synthetic complete mock home\n"
  );
  for direction in [DIRECTION, OTHER_DIRECTION] {
    assert!(mock_home_barrier.join(direction).is_file(), "the {direction} round did not read alongside its peer");
  }
  let builds = fs::read_to_string(&cargo_log).unwrap_or_default();
  assert_eq!(builds.lines().count(), 1, "the runners built the commit more than once");
  let runner_bin = world.remote_home.join("improve/runner/bin");
  for tool in ["charliebot", "improve-sandbox", "mock-home"] {
    assert!(runner_bin.join(format!("{tool}-{commit}")).is_file(), "the runners did not install {tool}");
    assert_eq!(
      fs::read_link(runner_bin.join(tool)).expect("the runner's tool link"),
      std::path::PathBuf::from(format!("{tool}-{commit}")),
      "a round's {tool} link names another commit"
    );
  }
  for (direction, run) in std::iter::zip([DIRECTION, OTHER_DIRECTION], &runs) {
    let records = world.sandbox.dir(&format!("records-{direction}"));
    let ran = records.join("claude-argv.txt");
    assert!(ran.is_file(), "the {direction} round did not run the tools of the commit:\n{}", run.describe());
    let env = fs::read_to_string(records.join("claude-env.txt")).expect("the round's environment");
    // The round's PATH starts with its member CLI's bin, then the review CLI's bin, then the tools of this commit. The
    // member and the review are both claude at the launcher's version here, so their bins are the same directory.
    let commit_bin = runner_bin.join(&commit).display().to_string();
    let cli_bin =
      runner_bin.parent().expect("the runner root").join(format!("members/claude-{}/bin", world::LOCAL_CLAUDE_VERSION));
    let pinned = format!("PATH={}:{}:{commit_bin}:", cli_bin.display(), cli_bin.display());
    assert!(env.contains(&pinned), "the {direction} round did not pin its tools:\n{env}");
  }
}

#[test]
#[ignore = "integration"]
fn another_clones_locked_origin_main_ref_does_not_stop_the_rounds_in_flight_read() {
  let world = World::new();
  let clone = world.other_direction_clone(OTHER_DIRECTION);
  // The committed change rides a topic branch, as a round's branch does.
  world.git(&clone, &["switch", "-q", "-c", &format!("improve/{OTHER_DIRECTION}/topic")]);
  fs::write(clone.join("committed.md"), "topic\n").expect("the committed fixture file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "synthetic topic"]);
  // The origin's main moves ahead, so the runner's fetch has a ref to write, as a live host's does.
  world.git(&clone, &["switch", "-q", "main"]);
  fs::write(clone.join("advanced.md"), "later\n").expect("the main commit's file");
  world.git(&clone, &["add", "-A"]);
  world.git(&clone, &["commit", "-q", "-m", "advance main"]);
  world.git(&clone, &["push", "-q", "origin", "main"]);
  world.git(&clone, &["switch", "-q", &format!("improve/{OTHER_DIRECTION}/topic")]);
  // The uncommitted change sits in the checkout. The untracked file has no git entry at all.
  fs::write(clone.join("base.md"), "changed\n").expect("the uncommitted change");
  fs::write(clone.join("untracked.md"), "new\n").expect("the untracked file");
  // The clone's own origin/main stays behind, so the fetch updates it, and a concurrent writer
  // holds the ref lock while it does.
  let behind = world.git(&clone, &["rev-parse", "HEAD~1"]);
  world.git(&clone, &["update-ref", "refs/remotes/origin/main", &behind]);
  fs::create_dir_all(clone.join(".git/refs/remotes/origin")).expect("the ref directory");
  fs::write(clone.join(".git/refs/remotes/origin/main.lock"), "").expect("the ref lock");

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  // The clone's checked-out topic branch claims nothing: no ux round runs, so the read reaches
  // only the fetch, which the locked ref must not stop.
  let list = fs::read_to_string(&world.claude_inflight).expect("the round recorded its in-flight list");
  assert!(list.trim().is_empty(), "the locked clone still claimed paths:\n{list}");
  assert!(
    !clone.join(".git/FETCH_HEAD").exists(),
    "the runner wrote the clone's shared FETCH_HEAD:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_locked_origin_main_ref_does_not_stop_the_launchers_in_flight_list() {
  let world = World::new();
  // The origin's main moves ahead of the checkout's tracking ref, so the script's fetch has a ref
  // to write, as a live host's does.
  fs::write(world.repo.join("advanced.md"), "later\n").expect("the main commit's file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "advance main"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  world.git(&world.repo, &["update-ref", "refs/remotes/origin/main", &world.commit]);
  world.git(&world.repo, &["reset", "-q", "--hard", &world.commit]);
  // A worktree branch whose work origin/main does not hold, as a live line's checkout is.
  let checkout = world.sandbox.root().join("locked-checkout");
  world.git(&world.repo, &["branch", "charliebot/locked"]);
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "charliebot/locked"]);
  fs::write(checkout.join("locked.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic locked topic"]);
  fs::write(checkout.join("dirty.md"), "uncommitted\n").expect("the uncommitted change");
  // The lock a concurrent fetch of this checkout holds while it writes the shared tracking ref.
  fs::create_dir_all(world.repo.join(".git/refs/remotes/origin")).expect("the ref directory");
  fs::write(world.repo.join(".git/refs/remotes/origin/main.lock"), "").expect("the ref lock");
  let fetch_head = world.repo.join(".git/FETCH_HEAD");
  fs::remove_file(&fetch_head).expect("remove the setup fetch's FETCH_HEAD");

  let mut command = harness::command("bash", &world.env());
  // The launcher runs the script out of its checkout, so this run does too.
  command.arg(world.repo.join("dev/improve/inflight-files.sh")).arg(format!("improve/{DIRECTION}/"));
  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in
    ["locked.md\tcharliebot/locked\tabsent-on-origin-main", "dirty.md\tcharliebot/locked\tabsent-on-origin-main"]
  {
    assert!(run.stdout.lines().any(|line| line == listed), "the in-flight list misses {listed}:\n{}", run.describe());
  }
  assert!(!fetch_head.exists(), "the launcher's in-flight list wrote the shared FETCH_HEAD");
}

#[test]
#[ignore = "integration"]
fn an_unowned_worktree_keeps_a_path_that_main_deleted() {
  let world = World::new();
  // Two tracked files on main, as a live repository's paths are.
  fs::write(world.repo.join("old.md"), "old\n").expect("the old path's file");
  fs::write(world.repo.join("live.md"), "live\n").expect("the live path's file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "add the fixture files"]);
  // A worktree branch that changes both paths, as a live line's checkout is.
  let checkout = world.sandbox.root().join("renamed-checkout");
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", "charliebot/renamed"]);
  fs::write(checkout.join("old.md"), "topic\n").expect("the old path's topic change");
  fs::write(checkout.join("live.md"), "topic\n").expect("the live path's topic change");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", "synthetic rename topic"]);
  // main deletes the old path after the branch's base, as a rename's landing does.
  world.git(&world.repo, &["rm", "-q", "old.md"]);
  world.git(&world.repo, &["commit", "-q", "-m", "drop the old name"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  // The worktree directory still carries the old path's uncommitted change, beside a live one and
  // an untracked file. An open pull request holds its own path.
  fs::write(checkout.join("old.md"), "uncommitted\n").expect("the old path's uncommitted change");
  fs::write(checkout.join("live.md"), "uncommitted\n").expect("the live path's uncommitted change");
  fs::write(checkout.join("fresh.md"), "untracked\n").expect("the untracked file");
  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "13\tcharliebot/task-open\tpr-only.md\n").expect("the open pull requests");
  let mut env = world.env();
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());

  let mut command = harness::command("bash", &env);
  command.arg(world.repo.join("dev/improve/inflight-files.sh")).arg(format!("improve/{DIRECTION}/"));
  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
  // No task names this worktree, so its committed and dirty paths stay alongside the open pull request's file.
  // Each path prints with the branch that carries it. An unmerged commit's branch carries its file. The
  // dirty worktree's branch carries its uncommitted paths. An open pull request's head branch carries its file.
  // main deleted old.md and the fixture has no pr-only.md or fresh.md, so their claims carry the absent field;
  // live.md is on main, so its claim stays unmarked.
  let mut listed: Vec<&str> = run.stdout.lines().collect();
  listed.sort_unstable();
  assert_eq!(
    listed,
    [
      "fresh.md\tcharliebot/renamed\tabsent-on-origin-main",
      "live.md\tcharliebot/renamed",
      "old.md\tcharliebot/renamed\tabsent-on-origin-main",
      "pr-only.md\tcharliebot/task-open\tabsent-on-origin-main"
    ],
    "the in-flight list drops a path from an unowned worktree:\n{}",
    run.describe()
  );
}

/// One task lifecycle state written to the profile home's task store.
fn write_task_state(world: &World, task_id: &str, open: bool) -> std::path::PathBuf {
  let session_dir = world.profile_home.join(format!("sessions/{task_id}"));
  fs::create_dir_all(session_dir.join("data")).expect("the task session directory");
  let metadata = serde_json::json!({"id": task_id});
  fs::write(session_dir.join("metadata.json"), format!("{metadata:#}\n")).expect("the task metadata");
  let event = if open {
    serde_json::json!({"id": format!("{task_id}-state"), "type": "task_reopened"})
  } else {
    serde_json::json!({"id": format!("{task_id}-state"), "type": "task_closed", "outcome": "completed"})
  };
  let events = session_dir.join("data/chat_events.jsonl");
  fs::write(&events, format!("{event}\n")).expect("the task lifecycle event");
  events
}

/// A run record that links one task to its worktree. Its end time does not set the task state.
fn write_run_record(world: &World, task_id: &str, run_id: &str, worktree: &std::path::Path) {
  let run_dir = world.profile_home.join(format!("sessions/{task_id}/data/runs/{run_id}"));
  fs::create_dir_all(&run_dir).expect("the run record directory");
  let record = serde_json::json!({
    "id": run_id,
    "worktree_path": worktree.display().to_string(),
    "started_at": "2026-11-10T01:00:00.000000Z",
    "ended_at": "2026-11-10T02:00:00.000000Z",
  });
  fs::write(run_dir.join("metadata.json"), format!("{record:#}\n")).expect("the run record");
}

/// A task record that links one worktree to its owning session, as the store's `threads/` records do.
/// The session's run records may be gone with the session they belong to.
fn write_task_record(world: &World, owner_id: &str, thread_id: &str, worktree: &std::path::Path) {
  let record_dir = world.profile_home.join(format!("sessions/{owner_id}/threads/{thread_id}"));
  fs::create_dir_all(&record_dir).expect("the task record directory");
  let record = serde_json::json!({
    "id": thread_id,
    "session_id": owner_id,
    "status": "completed",
    "worktree_path": worktree.display().to_string(),
  });
  fs::write(record_dir.join("metadata.json"), format!("{record:#}\n")).expect("the task record");
}

/// One landed branch with dirty, staged and untracked paths, as a task worktree can hold after its commits land.
fn landed_worktree_with_uncommitted_state(world: &World, name: &str) -> std::path::PathBuf {
  let checkout = world.sandbox.root().join(name);
  world
    .git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", &format!("charliebot/{name}")]);
  fs::write(checkout.join("topic.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", &format!("synthetic {name} topic")]);
  fs::write(world.repo.join("topic.md"), "topic\n").expect("the squash commit's content");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", &format!("synthetic {name} topic, retitled (#9)")]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  fs::write(checkout.join("topic.md"), "uncommitted\n").expect("the uncommitted change");
  fs::write(checkout.join("staged.md"), "staged\n").expect("the staged change");
  world.git(&checkout, &["add", "staged.md"]);
  fs::write(checkout.join("untracked.md"), "untracked\n").expect("the untracked file");
  checkout
}

/// One unlanded branch with both a committed path and a dirty path.
fn unlanded_worktree_with_uncommitted_state(world: &World, name: &str) -> std::path::PathBuf {
  let checkout = world.sandbox.root().join(name);
  world
    .git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", &format!("charliebot/{name}")]);
  fs::write(checkout.join("topic.md"), "topic\n").expect("the topic's file");
  world.git(&checkout, &["add", "-A"]);
  world.git(&checkout, &["commit", "-q", "-m", &format!("synthetic {name} topic")]);
  fs::write(checkout.join("dirty.md"), "uncommitted\n").expect("the uncommitted change");
  checkout
}

/// The in-flight list's run out of its checkout, as the launcher does.
fn run_inflight_list_with_env(world: &World, env: &harness::Env) -> harness::Run {
  let mut command = harness::command("bash", env);
  command.arg(world.repo.join("dev/improve/inflight-files.sh")).arg(format!("improve/{DIRECTION}/"));
  harness::run(&mut command)
}

fn run_inflight_list(world: &World) -> harness::Run {
  run_inflight_list_with_env(world, &world.env())
}

#[test]
#[ignore = "integration"]
fn an_open_task_keeps_an_unlanded_worktrees_paths_claimed_after_its_run_ends() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "open-task");
  write_task_state(&world, "task-open", true);
  write_run_record(&world, "task-open", "run-open-ended", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in
    ["topic.md\tcharliebot/open-task\tabsent-on-origin-main", "dirty.md\tcharliebot/open-task\tabsent-on-origin-main"]
  {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops the open task's path {listed} after its run ended:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_closed_task_without_other_claimants_drops_an_unlanded_worktrees_paths() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "closed-task");
  write_task_state(&world, "task-closed", false);
  write_run_record(&world, "task-closed", "run-closed", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md", "dirty.md"] {
    assert!(
      !lists_path(&run.stdout, listed),
      "the in-flight list keeps the closed task's path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_closed_task_drops_a_landed_worktrees_paths() {
  let world = World::new();
  let checkout = landed_worktree_with_uncommitted_state(&world, "closed-landed-task");
  write_task_state(&world, "task-closed-landed", false);
  write_run_record(&world, "task-closed-landed", "run-closed-landed", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md", "staged.md", "untracked.md"] {
    assert!(
      !lists_path(&run.stdout, listed),
      "the in-flight list keeps the closed landed task's path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn an_open_pull_request_keeps_a_closed_tasks_worktree_paths_claimed() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "closed-pr");
  write_task_state(&world, "task-closed-pr", false);
  write_run_record(&world, "task-closed-pr", "run-closed-pr", &checkout);
  let pull_requests = world.sandbox.root().join("open-pull-requests.tsv");
  fs::write(&pull_requests, "17\tcharliebot/closed-pr\tpr-only.md\n").expect("the open pull request");
  let mut env = world.env();
  env.insert("FAKE_GH_PRS".to_string(), pull_requests.display().to_string());

  let run = run_inflight_list_with_env(&world, &env);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in [
    "topic.md\tcharliebot/closed-pr\tabsent-on-origin-main",
    "dirty.md\tcharliebot/closed-pr\tabsent-on-origin-main",
    "pr-only.md\tcharliebot/closed-pr\tabsent-on-origin-main",
  ] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the open pull request does not keep its worktree path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_worktree_without_a_task_name_keeps_its_uncommitted_paths_and_drops_its_landed_commit() {
  let world = World::new();
  landed_worktree_with_uncommitted_state(&world, "no-task");

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  // The topic commit is patch-equivalent on main, so its committed path names nothing. The same path's
  // uncommitted change, the staged path and the untracked path print with the branch that carries them.
  for listed in [
    "topic.md\tcharliebot/no-task",
    "staged.md\tcharliebot/no-task\tabsent-on-origin-main",
    "untracked.md\tcharliebot/no-task\tabsent-on-origin-main",
  ] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops the unowned worktree path {listed}:\n{}",
      run.describe()
    );
  }
  assert!(
    !run.stdout.lines().any(|line| line == "topic.md"),
    "the in-flight list prints the landed committed path bare:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_process_in_a_closed_tasks_worktree_keeps_its_paths_claimed() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "with-process");
  write_task_state(&world, "task-with-process", false);
  write_run_record(&world, "task-with-process", "run-with-process", &checkout);
  let mut process = std::process::Command::new("bash")
    .arg("-c")
    .arg("exec sleep 30")
    .current_dir(&checkout)
    .spawn()
    .expect("the worktree process");
  let cwd = format!("/proc/{}/cwd", process.id());
  assert!(
    wait_until(std::time::Duration::from_secs(5), || { fs::read_link(&cwd).is_ok_and(|target| target == checkout) }),
    "the worktree process did not enter the worktree"
  );

  let run = run_inflight_list(&world);

  process.kill().expect("stop the worktree process");
  process.wait().expect("reap the worktree process");
  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in [
    "topic.md\tcharliebot/with-process\tabsent-on-origin-main",
    "dirty.md\tcharliebot/with-process\tabsent-on-origin-main",
  ] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops the process-held worktree path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn an_unreadable_task_state_stops_the_list_with_an_error() {
  use std::os::unix::fs::PermissionsExt;
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "unreadable-task");
  let events = write_task_state(&world, "task-unreadable", false);
  write_run_record(&world, "task-unreadable", "run-unreadable", &checkout);
  fs::set_permissions(&events, fs::Permissions::from_mode(0o000)).expect("the unreadable task state");
  assert!(fs::File::open(&events).is_err(), "the test needs task state this user cannot read");

  let run = run_inflight_list(&world);

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(
    run.stderr.contains("cannot read the task-state condition") && run.stderr.contains(&events.display().to_string()),
    "the in-flight list does not name the unreadable task state:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_closed_owning_sessions_task_record_drops_an_unlanded_worktrees_paths() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "closed-record");
  write_task_state(&world, "record-owner", false);
  write_task_record(&world, "record-owner", "thread-closed", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["topic.md", "dirty.md"] {
    assert!(
      !lists_path(&run.stdout, listed),
      "the in-flight list keeps the closed task record's path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn an_open_owning_sessions_task_record_keeps_the_worktree_paths_claimed() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "open-record");
  write_task_state(&world, "record-owner-open", true);
  write_task_record(&world, "record-owner-open", "thread-open", &checkout);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in [
    "topic.md\tcharliebot/open-record\tabsent-on-origin-main",
    "dirty.md\tcharliebot/open-record\tabsent-on-origin-main",
  ] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops the open task record's path {listed}:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_launch_record_naming_the_worktree_claims_nothing() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "launch-record");
  write_task_state(&world, "record-owner-launch", false);
  // A trigger launch record beside the task records names the worktree as its worktree_path. The store
  // writes no such field there, and only the run and task records own a worktree. The record therefore
  // names no owner, and the worktree stays unrecorded.
  let launch_dir = world.profile_home.join("sessions/record-owner-launch/launches/20261113T000000-abcdef");
  fs::create_dir_all(&launch_dir).expect("the launch record directory");
  let record = serde_json::json!({
    "launch_id": "20261113T000000-abcdef",
    "cmd": "cargo test",
    "worktree_path": checkout.display().to_string(),
  });
  fs::write(launch_dir.join("metadata.json"), format!("{record:#}\n")).expect("the launch record");

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in [
    "topic.md\tcharliebot/launch-record\tabsent-on-origin-main",
    "dirty.md\tcharliebot/launch-record\tabsent-on-origin-main",
  ] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list dropped the launch-record worktree path {listed}:\n{}",
      run.describe()
    );
  }
}

/// A worktree on a branch at origin/main's tip, with one tracked edit and one untracked file and no commit of its own.
fn worktree_at_main_with_uncommitted_edits(world: &World, name: &str) -> std::path::PathBuf {
  fs::write(world.repo.join("tracked.md"), "base\n").expect("the tracked file");
  world.git(&world.repo, &["add", "-A"]);
  world.git(&world.repo, &["commit", "-q", "-m", "add the tracked fixture file"]);
  world.git(&world.repo, &["push", "-q", "origin", "main"]);
  let checkout = world.sandbox.root().join(name);
  world
    .git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", &format!("charliebot/{name}")]);
  fs::write(checkout.join("tracked.md"), "edited\n").expect("the uncommitted edit");
  fs::write(checkout.join("untracked.md"), "untracked\n").expect("the untracked file");
  checkout
}

/// The instant `days` days before now, for a change time that the in-flight list reads as old or recent.
fn days_ago(days: u64) -> std::time::SystemTime {
  std::time::SystemTime::now() - Duration::from_secs(days * 86_400)
}

/// Set a path's modification time, which the in-flight list reads as the path's change time.
fn set_change_time(path: &Path, when: std::time::SystemTime) {
  fs::File::options()
    .write(true)
    .open(path)
    .expect("the path to age")
    .set_modified(when)
    .expect("the aged change time");
}

/// `when` in the UTC form that the in-flight list's notice prints, formatted by the same `date`.
fn utc_timestamp(when: std::time::SystemTime) -> String {
  let seconds = when.duration_since(std::time::UNIX_EPOCH).expect("a time after the epoch").as_secs();
  let output = std::process::Command::new("date")
    .args(["-u", "-d", &format!("@{seconds}"), "+%Y-%m-%dT%H:%M:%SZ"])
    .output()
    .expect("date");
  String::from_utf8(output.stdout).expect("the date output").trim().to_string()
}

#[test]
#[ignore = "integration"]
fn dropped_worktree_notices_reach_the_remote_round() {
  let world = World::new();
  let checkout = world.sandbox.root().join("old-edits");
  world.git(&world.repo, &["worktree", "add", "-q", &checkout.display().to_string(), "-b", "charliebot/old-edits"]);
  let tracked = checkout.join("dev/improve/remote-launch.sh");
  let contents = fs::read_to_string(&tracked).expect("the tracked launcher script");
  fs::write(&tracked, format!("{contents}# stale edit\n")).expect("the tracked stale edit");
  let untracked = checkout.join("old-untracked.md");
  fs::write(&untracked, "stale untracked edit\n").expect("the untracked stale edit");
  let when = days_ago(8);
  set_change_time(&tracked, when);
  set_change_time(&untracked, when);

  let run = world.run(&world.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.claude_ran(), "the fake claude did not run:\n{}", run.describe());
  let files = fs::read_to_string(&world.claude_inflight).expect("the round's in-flight files list");
  assert!(!lists_path(&files, "dev/improve/remote-launch.sh"), "the stale path remained claimed:\n{files}");
  let notices = fs::read_to_string(&world.claude_inflight_notices).expect("the round's dropped worktree notices");
  assert!(
    notices.contains(&checkout.display().to_string())
      && notices.contains("charliebot/old-edits")
      && notices.contains(&utc_timestamp(when)),
    "the remote round did not receive the dropped worktree and its newest change time:\n{notices}\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_no_record_worktree_with_only_old_edits_and_no_commit_ahead_names_no_path_and_a_notice() {
  let world = World::new();
  let checkout = worktree_at_main_with_uncommitted_edits(&world, "old-edits");
  let when = days_ago(8);
  set_change_time(&checkout.join("tracked.md"), when);
  set_change_time(&checkout.join("untracked.md"), when);

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["tracked.md", "untracked.md"] {
    assert!(
      !lists_path(&run.stdout, listed),
      "the in-flight list claims {listed} of a worktree whose changes are all 8 days old:\n{}",
      run.describe()
    );
  }
  assert!(
    run.stderr.contains(&checkout.display().to_string())
      && run.stderr.contains("charliebot/old-edits")
      && run.stderr.contains(&utc_timestamp(when)),
    "the in-flight list does not name the dropped worktree with its newest change time:\n{}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn a_no_record_worktree_with_one_edit_from_the_last_day_claims_all_its_paths() {
  let world = World::new();
  let checkout = worktree_at_main_with_uncommitted_edits(&world, "recent-edit");
  set_change_time(&checkout.join("tracked.md"), days_ago(8));
  set_change_time(&checkout.join("untracked.md"), days_ago(1));

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in ["tracked.md\tcharliebot/recent-edit", "untracked.md\tcharliebot/recent-edit\tabsent-on-origin-main"] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops {listed} although one of the worktree's changes is a day old:\n{}",
      run.describe()
    );
  }
}

#[test]
#[ignore = "integration"]
fn a_no_record_worktree_with_a_commit_ahead_claims_its_paths_whatever_their_age() {
  let world = World::new();
  let checkout = unlanded_worktree_with_uncommitted_state(&world, "ahead-old-edits");
  set_change_time(&checkout.join("dirty.md"), days_ago(8));

  let run = run_inflight_list(&world);

  assert_eq!(run.code, 0, "{}", run.describe());
  for listed in [
    "topic.md\tcharliebot/ahead-old-edits\tabsent-on-origin-main",
    "dirty.md\tcharliebot/ahead-old-edits\tabsent-on-origin-main",
  ] {
    assert!(
      run.stdout.lines().any(|line| line == listed),
      "the in-flight list drops {listed} from a worktree with an unmerged commit:\n{}",
      run.describe()
    );
  }
}

/// The path that a fake member CLI records when the round or a probe runs the member's copy of `kind` at `version`.
fn member_path(world: &World, kind: &str, version: &str) -> String {
  world.remote_home.join(format!("improve/runner/members/{kind}-{version}/bin/{kind}")).display().to_string()
}

#[test]
#[ignore = "integration"]
fn a_codex_round_runs_the_version_the_launcher_reads_not_the_hosts_copy() {
  let world = World::new();
  world.publish_member_cli("codex", "9.9.9", "codex-cli 9.9.9");
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut env = world.host_login_env();
  env.insert("FAKE_LOCAL_CODEX_LINE".to_string(), "codex-cli 9.9.9".to_string());
  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(world.cli_runs(), [member_path(&world, "codex", "9.9.9")], "{}", run.describe());
  // Tool calls start the helpers beside the executable, so the install keeps the vendor's layout whole.
  let member = world.remote_home.join("improve/runner/members/codex-9.9.9");
  for helper in ["bin/codex-code-mode-host", "bin/codex-exec-server", "codex-path/rg", "codex-resources/bwrap"] {
    assert!(member.join(helper).is_file(), "the install lacks {helper}:\n{}", run.describe());
  }
}

#[test]
#[ignore = "integration"]
fn a_codex_package_that_differs_from_its_published_digest_exits_7_and_installs_nothing() {
  let world = World::new();
  world.publish_member_cli("codex", "9.9.9", "codex-cli 9.9.9");
  // The package changes after the release listing names its digest, so the install must refuse what it fetched.
  let package = world.vendor_root.join(format!(
    "github.com/openai/codex/releases/download/rust-v9.9.9/codex-package-{}-unknown-linux-musl.tar.gz",
    std::env::consts::ARCH
  ));
  let mut bytes = fs::read(&package).expect("the fake package");
  bytes.push(b'x');
  fs::write(&package, bytes).expect("the altered package");
  let run = harness::run(&mut world.member_cli_command("codex", "9.9.9"));
  assert_eq!(run.code, 7, "{}", run.describe());
  assert!(stderr(&run).contains("the vendor publishes"), "{}", run.describe());
  assert!(stderr(&run).contains("improve host version none"), "{}", run.describe());
  let members = world.remote_home.join("improve/runner/members");
  assert!(!members.join("codex-9.9.9").exists(), "a package that failed its digest was installed");
  let stages: Vec<String> = fs::read_dir(&members)
    .map(|entries| {
      entries.map(|entry| entry.expect("a member entry").file_name().to_string_lossy().into_owned()).collect()
    })
    .unwrap_or_default();
  assert!(stages.iter().all(|name| !name.starts_with(".stage-")), "a failed install left a stage: {stages:?}");
}

#[test]
#[ignore = "integration"]
fn a_claude_round_runs_the_vendor_claude_at_the_launchers_version() {
  let world = World::new();
  let run = world.run(&world.env());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(world.cli_runs(), [member_path(&world, "claude", world::LOCAL_CLAUDE_VERSION)], "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_member_cli_the_vendor_does_not_publish_exits_7_and_starts_no_round() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut env = world.host_login_env();
  env.insert("FAKE_LOCAL_CODEX_LINE".to_string(), "codex-cli 9.9.8".to_string());
  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));
  assert_refused_before_start(&world, &run, "9.9.8");
  assert!(stderr(&run).contains("improve host version none"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_member_cli_that_reports_another_version_exits_7_and_names_both() {
  let world = World::new();
  world.publish_member_cli("codex", "9.9.9", "codex-cli 9.9.8");
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut env = world.host_login_env();
  env.insert("FAKE_LOCAL_CODEX_LINE".to_string(), "codex-cli 9.9.9".to_string());
  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));
  assert_refused_before_start(&world, &run, "9.9.9");
  assert!(stderr(&run).contains("improve host version codex-cli 9.9.8"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn an_unparseable_local_cli_version_exits_7_before_any_input_is_sent() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let mut env = world.host_login_env();
  env.insert("FAKE_LOCAL_CODEX_LINE".to_string(), "codex-cli banana".to_string());
  let run = harness::run(&mut world.launcher_command_for(&env, &world.codex_arguments()));
  assert_refused_before_input(&world, &run, "codex-cli banana");
}

#[test]
#[ignore = "integration"]
fn a_cached_member_cli_is_reused_without_a_download() {
  let world = World::new();
  world.write_codex_login(world::now_seconds() + 200 * 3600);
  let env = world.host_login_env();
  let arguments = world.codex_arguments();
  let first = harness::run(&mut world.launcher_command_for(&env, &arguments));
  assert_eq!(first.code, 0, "{}", first.describe());
  // The codex member's release listing and package, then the review's claude manifest and binary: four fetches for the
  // first install of both CLIs.
  let fetched = world.curl_calls();
  assert_eq!(fetched.len(), 4, "the first round fetched {fetched:?}");
  remove_newest_round_evidence(&world);
  let second = harness::run(&mut world.launcher_command_for(&env, &arguments));
  assert_eq!(second.code, 0, "{}", second.describe());
  assert_eq!(world.curl_calls(), fetched, "the second round fetched again");
  let path = member_path(&world, "codex", world::LOCAL_CODEX_VERSION);
  assert_eq!(world.cli_runs(), [path.clone(), path]);
}

#[test]
#[ignore = "integration"]
fn concurrent_installs_of_one_member_cli_version_fetch_once_and_both_succeed() {
  let world = World::new();
  world.publish_member_cli("codex", "9.9.9", "codex-cli 9.9.9");
  let mut first = world.member_cli_command("codex", "9.9.9");
  first.env("FAKE_CURL_SLEEP", "1");
  let mut second = world.member_cli_command("codex", "9.9.9");
  second.env("FAKE_CURL_SLEEP", "1");
  let first = first.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("the first install starts");
  let second = second.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("the second install starts");
  let first = first.wait_with_output().expect("the first install ends");
  let second = second.wait_with_output().expect("the second install ends");
  assert!(first.status.success(), "the first install: {}", String::from_utf8_lossy(&first.stderr));
  assert!(second.status.success(), "the second install: {}", String::from_utf8_lossy(&second.stderr));
  assert_eq!(world.curl_calls().len(), 2, "two installs of one version fetched {:?}", world.curl_calls());
  assert!(Path::new(&member_path(&world, "codex", "9.9.9")).is_file(), "the installed CLI is missing");
}

#[test]
#[ignore = "integration"]
fn the_member_cache_keeps_the_live_and_last_used_versions_and_prunes_the_rest() {
  let world = World::new();
  let members = world.remote_home.join("improve/runner/members");
  for name in ["codex-0.100.0", "codex-0.101.0", "codex-0.102.0", ".stage-codex-0.90.0.abcdef"] {
    fs::create_dir_all(members.join(name).join("bin")).expect("a member directory");
  }
  std::os::unix::fs::symlink("codex-0.102.0", members.join(".current-codex")).expect("the current pointer");
  let stop = world.sandbox.root().join("stop-live-round");
  let mut live = world.spawn_live_runner(&stop, &["launch", DIRECTION, "0123456789abcdef", "codex", "0.101.0"]);
  let run = harness::run(&mut world.member_cli_command("codex", world::LOCAL_CODEX_VERSION));
  fs::write(&stop, "").expect("the stop file");
  live.wait().expect("the live runner stand-in ends");
  assert_eq!(run.code, 0, "{}", run.describe());
  // The live round's version and the installed current version stay; the other two versions and the stage go.
  let mut kept: Vec<String> = fs::read_dir(&members)
    .expect("the member directory")
    .map(|entry| entry.expect("a member entry").file_name().to_string_lossy().into_owned())
    .filter(|name| !name.starts_with('.'))
    .collect();
  kept.sort();
  assert_eq!(kept, ["codex-0.101.0", "codex-0.167.0"], "{}", run.describe());
  assert!(!members.join(".stage-codex-0.90.0.abcdef").exists(), "a stage directory stayed");
}

#[test]
#[ignore = "integration"]
fn a_round_prunes_the_tools_of_commits_that_no_live_process_names() {
  let world = World::new();
  let bin = world.remote_home.join("improve/runner/bin");
  let live_commit = "1111111111111111111111111111111111111111";
  let dead_commit = "2222222222222222222222222222222222222222";
  for commit in [live_commit, dead_commit] {
    fs::create_dir_all(bin.join(commit)).expect("a commit's directory");
    for tool in ["charliebot", "improve-sandbox", "mock-home"] {
      fs::write(bin.join(format!("{tool}-{commit}")), "stale tool\n").expect("a commit's tool");
    }
  }
  let stop = world.sandbox.root().join("stop-live-round");
  let mut live =
    world.spawn_live_runner(&stop, &["launch", DIRECTION, live_commit, "claude", world::LOCAL_CLAUDE_VERSION]);
  let run = world.run(&world.env());
  fs::write(&stop, "").expect("the stop file");
  live.wait().expect("the live runner stand-in ends");
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(bin.join(live_commit).is_dir(), "a live round's commit directory was pruned");
  assert!(bin.join(format!("charliebot-{live_commit}")).exists(), "a live round's tools were pruned");
  assert!(!bin.join(dead_commit).exists(), "a dead commit's directory stayed");
  assert!(!bin.join(format!("charliebot-{dead_commit}")).exists(), "a dead commit's tools stayed");
  assert!(bin.join(format!("charliebot-{}", world.commit)).exists(), "the round's own tools were pruned");
}

// The production scan (dev/improve/rust-production-scan.mjs), which the rounds scan Rust production code with.

/// Writes each `(relative path, text)` fixture under `root`.
fn write_fixtures(root: &Path, files: &[(&str, &str)]) {
  for (relative, text) in files {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("a fixture path has a parent")).expect("the fixture directory");
    fs::write(&path, text).expect("the fixture file");
  }
}

/// Runs the production scan on `arguments`, from the fixture root.
fn production_scan(sandbox: &Sandbox, arguments: &[&str]) -> harness::Run {
  let mut command = harness::command("node", &sandbox.env());
  command
    .current_dir(sandbox.root())
    .arg(harness::repo_root().join("dev/improve/rust-production-scan.mjs"))
    .args(arguments);
  harness::run(&mut command)
}

/// The line numbers that the scan printed for `file`, in order.
fn scanned_lines(stdout: &str, file: &str) -> Vec<usize> {
  stdout
    .lines()
    .filter_map(|line| line.strip_prefix(file)?.strip_prefix(':')?.split(':').next()?.parse().ok())
    .collect()
}

const PRODUCTION_AFTER_TEST_ELEMENTS: &str = "\
pub fn before() -> u8 {
  1
}

#[cfg(test)]
mod tests {
  use super::*;

  mod nested {
    fn deep() {
      let _ = \"}\";
    }
  }

  #[test]
  fn reads_home() {
    let _ = std::env::var(\"HOME\");
  }
}

#[tokio::test(flavor = \"multi_thread\")]
async fn outside_a_module() {
  let _ = std::env::var(\"HOME\");
}

pub struct Counter {
  #[cfg(test)]
  builds: HashMap<String, u8>,
  kept: u8,
}

pub fn choose(value: u8) -> u8 {
  #[cfg(test)]
  let _ = std::env::var(\"HOME\");
  match value {
    #[cfg(test)]
    0 => 9,
    _ => value,
  }
}

pub fn after() -> u8 {
  2
}
";

#[test]
#[ignore = "integration"]
fn production_scan_keeps_production_code_after_test_elements_and_names_its_revision() {
  let sandbox = Sandbox::new();
  write_fixtures(sandbox.root(), &[("src/lib.rs", PRODUCTION_AFTER_TEST_ELEMENTS)]);
  let run = production_scan(&sandbox, &["src/lib.rs"]);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(
    scanned_lines(&run.stdout, "src/lib.rs"),
    vec![1, 2, 3, 26, 29, 30, 32, 35, 38, 39, 40, 42, 43, 44],
    "{}",
    run.describe()
  );
  // The summary line names the tool's revision: twelve hex digits, then a colon.
  let revision = run.stderr.strip_prefix("rust-production-scan ").and_then(|rest| rest.get(..12));
  assert!(revision.is_some_and(|hex| hex.chars().all(|c| c.is_ascii_hexdigit())), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn production_scan_keeps_macro_tokens_and_hidden_rust_files() {
  const MACROS: &str = "\
macro_rules! emit_test_module {
  () => {
    #[cfg(test)]
    mod generated;
  };
}
pub fn production() {}
pub fn calls_macro() {
  token_tree! {
    #[cfg(test)]
    mod not_a_module;
  }
}
#[cfg(test)]
test_item!();
pub fn after() {}
";
  let sandbox = Sandbox::new();
  write_fixtures(sandbox.root(), &[("src/lib.rs", MACROS), (".private/private.rs", "pub fn hidden() {}\n")]);
  let run = production_scan(&sandbox, &["."]);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(scanned_lines(&run.stdout, "src/lib.rs"), (1..=13).chain([16]).collect::<Vec<_>>(), "{}", run.describe());
  assert_eq!(scanned_lines(&run.stdout, ".private/private.rs"), vec![1], "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn production_scan_ignores_braces_inside_literals_and_comments_of_a_test_module() {
  const LITERALS: &str = r##"pub fn before() {}
#[cfg(test)]
mod literals {
  const A: &str = "}";
  const B: &str = r#"{ "}" }"#;
  const C: char = '}';
  const D: char = '\u{7B}';
  const E: u8 = b'{';
  // } an unbalanced brace in a comment
  /* outer /* nested } */ still { */
  fn lifetimes<'a>(x: &'a str) -> &'a str {
    x
  }
}
pub fn after() {}
"##;
  let sandbox = Sandbox::new();
  write_fixtures(sandbox.root(), &[("src/lib.rs", LITERALS)]);
  let run = production_scan(&sandbox, &["src/lib.rs"]);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(scanned_lines(&run.stdout, "src/lib.rs"), vec![1, 15], "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn production_scan_leaves_out_external_test_modules_and_tests_directories() {
  let sandbox = Sandbox::new();
  write_fixtures(
    sandbox.root(),
    &[
      (
        "src/lib.rs",
        "#[cfg(test)]\nmod helpers;\n#[cfg(test)]\n#[path = \"elsewhere/by_path.rs\"]\nmod by_path;\nmod real;\n",
      ),
      ("src/helpers.rs", "pub fn helper() -> u8 {\n  1\n}\n\nmod deeper;\n"),
      ("src/helpers/deeper.rs", "pub fn deep() {}\n"),
      ("src/elsewhere/by_path.rs", "pub fn by_path() {}\n"),
      ("src/real.rs", "pub fn real() -> u8 {\n  3\n}\n"),
      ("tests/integration.rs", "pub fn it_works() {}\n"),
    ],
  );
  let run = production_scan(&sandbox, &["."]);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(scanned_lines(&run.stdout, "src/lib.rs"), vec![6], "{}", run.describe());
  assert_eq!(scanned_lines(&run.stdout, "src/real.rs"), vec![1, 2, 3], "{}", run.describe());
  assert!(!run.stdout.contains("helper") && !run.stdout.contains("deep") && !run.stdout.contains("by_path"));

  // A scan with no production file fails: an empty result would read as "no finding".
  let tests_only = production_scan(&sandbox, &["tests/integration.rs"]);
  assert_eq!(tests_only.code, 1, "{}", tests_only.describe());
  assert!(tests_only.stdout.is_empty(), "{}", tests_only.describe());
}

#[test]
#[ignore = "integration"]
fn production_scan_fails_with_the_file_and_line_of_input_it_cannot_parse() {
  let cases: [(&str, &str, usize); 5] = [
    ("an unclosed brace", "pub fn f() {\n  1\n", 1),
    ("an unterminated string", "pub fn f() {}\nconst S: &str = \"abc;\n", 2),
    ("a closer that does not match", "pub fn f() {\n  (\n}\n", 3),
    ("a test attribute at the end of the file", "pub fn f() {}\n#[cfg(test)]\n", 2),
    ("a test module declaration with no file", "#[cfg(test)]\nmod missing;\n", 2),
  ];
  for (case, text, line) in cases {
    let sandbox = Sandbox::new();
    write_fixtures(sandbox.root(), &[("src/bad.rs", text)]);
    let run = production_scan(&sandbox, &["src/bad.rs"]);
    assert_eq!(run.code, 1, "{case}: {}", run.describe());
    assert!(run.stdout.is_empty(), "{case} printed production lines: {}", run.describe());
    assert!(run.stderr.contains(&format!("src/bad.rs:{line}:")), "{case} did not name its line: {}", run.describe());
  }
}

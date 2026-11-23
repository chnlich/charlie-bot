//! The stand-in for a release's `charliebot` binary in the switch tests: it serves `switch-helper`, the
//! `screenshots` reads the switch's capture step makes, and `backend run` for the post-capture live view check.
//! `KNOWN_VIEW` lists the View line(s) (one per line) that `screenshots resolve` passes.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::exit;

fn main() {
  let args: Vec<OsString> = std::env::args_os().skip(1).collect();
  match args.first().and_then(|argument| argument.to_str()) {
    Some("screenshots") => screenshots(&args[1..]),
    Some("backend") => backend(&args[1..]),
    Some(sub) if sub == charliebot_switch_helper::SUBCOMMAND => {}
    _ => {
      eprintln!("charliebot: unknown subcommand");
      exit(2);
    }
  }
  match charliebot_switch_helper::main(&args[1..]) {
    Ok(()) => exit(0),
    Err(failure) => {
      match serde_json::to_string(&failure.body) {
        Ok(body) => eprintln!("{body}"),
        Err(error) => eprintln!("cannot render the failure: {error}"),
      }
      exit(failure.code);
    }
  }
}

/// `sizes` prints `FAKE_SCREENSHOT_SIZES` (one per line) or `1536x900`. `resolve VIEW` passes only views listed in
/// `KNOWN_VIEW` (one per line), and refuses any other with the shape the real CLI prints.
fn screenshots(args: &[OsString]) -> ! {
  let args: Option<Vec<&str>> = args.iter().map(|argument| argument.to_str()).collect();
  let code = match args.as_deref() {
    Some(["sizes"]) => {
      if let Ok(sizes) = std::env::var("FAKE_SCREENSHOT_SIZES") {
        for size in sizes.split_whitespace() {
          println!("{size}");
        }
      } else {
        println!("1536x900");
      }
      0
    }
    Some(["resolve", view])
      if std::env::var("KNOWN_VIEW").is_ok_and(|known| known.lines().any(|line| line == *view)) =>
    {
      0
    }
    Some(["resolve", view]) => {
      eprintln!("the View line `{view}` must name a view on the live server, but the live server does not know it");
      1
    }
    _ => {
      eprintln!("charliebot: unknown screenshots command");
      2
    }
  };
  exit(code)
}

/// `backend run --call <class> --task-file <path>`: refuses `--one-shot`, checks `backends.calls.judge` in
/// `$CHARLIEBOT_HOME/config.yaml` when `FAKE_JUDGE_CHECK_CONFIG=1`, records invocation state when
/// `FAKE_JUDGE_RECORD_DIR` is set, writes `events.ndjson` in the current directory, and returns the configured answer
/// or failure.
fn backend(args: &[OsString]) -> ! {
  let args: Option<Vec<&str>> = args.iter().map(|argument| argument.to_str()).collect();
  let Some(args) = args else {
    eprintln!("charliebot backend: non-UTF-8 arguments");
    exit(2);
  };
  if args.first().copied() != Some("run") {
    eprintln!("charliebot: unknown backend command");
    exit(2);
  }
  if args.contains(&"--one-shot") {
    eprintln!("charliebot backend run: --one-shot is forbidden for live view check");
    exit(2);
  }
  let call = flag_value(&args[1..], "--call");
  let task_file = flag_value(&args[1..], "--task-file");
  if call.is_none() || task_file.is_none() {
    eprintln!("charliebot backend run: expected --call and --task-file");
    exit(2);
  }
  let task_file = task_file.expect("checked");
  let task_text = fs::read_to_string(task_file).unwrap_or_else(|error| {
    eprintln!("charliebot backend run: cannot read {task_file}: {error}");
    exit(1);
  });

  if std::env::var("FAKE_JUDGE_CHECK_CONFIG").ok().as_deref() == Some("1") {
    let home = std::env::var("CHARLIEBOT_HOME").unwrap_or_default();
    let cfg_path = Path::new(&home).join("config.yaml");
    let cfg_text = fs::read_to_string(&cfg_path).unwrap_or_default();
    if !cfg_text.lines().any(|line| line.trim_start().starts_with("judge:")) {
      eprintln!(
        "{{\"error\":\"backends.calls.judge is unset; \
         name a backends.options or backends.pools id for it in config.yaml\"}}"
      );
      exit(1);
    }
  }

  let cwd = std::env::current_dir().expect("cwd");
  if let Ok(record_dir) = std::env::var("FAKE_JUDGE_RECORD_DIR") {
    let record_dir = Path::new(&record_dir);
    fs::create_dir_all(record_dir).expect("record dir");
    fs::write(record_dir.join("args.txt"), args.join("\n")).expect("args.txt");
    fs::write(record_dir.join("cwd.txt"), cwd.display().to_string()).expect("cwd.txt");
    fs::write(record_dir.join("task.md"), &task_text).expect("task.md");
    let mut entries: Vec<String> = fs::read_dir(&cwd)
      .expect("read cwd")
      .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
      .collect();
    entries.sort();
    fs::write(record_dir.join("cwd_entries.txt"), entries.join("\n")).expect("cwd_entries.txt");
  }

  fs::write(cwd.join("events.ndjson"), "{\"type\":\"result\",\"result\":\"recorded\"}\n").expect("events.ndjson");

  if let Ok(stderr) = std::env::var("FAKE_JUDGE_STDERR") {
    eprintln!("{stderr}");
  }
  if let Ok(code_str) = std::env::var("FAKE_JUDGE_EXIT_CODE")
    && let Ok(code) = code_str.parse::<i32>()
    && code != 0
  {
    exit(code);
  }
  if let Ok(stdout) = std::env::var("FAKE_JUDGE_STDOUT") {
    print!("{stdout}");
  }
  exit(0)
}

fn flag_value<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
  args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1])
}

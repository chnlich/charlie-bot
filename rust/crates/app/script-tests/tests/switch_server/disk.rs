//! The switch's disk preflight (scripts/switch-server.sh): the bounded wait for a quiet root disk, the error that
//! names the heaviest readers, and the per-session read records around a switch.
//!
//! The tests inject the disk-rate sampler by replacing `disk_read_rate_mb`, and they read their cgroup and process
//! accounting from the world's fake cgroup and proc roots, so no test depends on the live disk.

use super::world::{self, World};

/// A sampler that answers each sample from `rates`, in order, and repeats the last value.
fn sequence_sampler(world: &World, rates: &[u64]) -> String {
  let counter = world.root().join("disk-samples");
  let values = rates.iter().map(|rate| rate.to_string()).collect::<Vec<_>>().join(" ");
  format!(
    r#"disk_read_rate_mb() {{
  local n=0
  [ -f {counter} ] && n=$(cat {counter})
  n=$((n + 1))
  echo "$n" > {counter}
  local value=0 index=1
  for value in {values}; do
    [ "$index" -ge "$n" ] && break
    index=$((index + 1))
  done
  echo "$value"
}}
"#,
    counter = world::quote(&counter),
  )
}

#[test]
fn test_disk_preflight_proceeds_at_once_on_a_quiet_disk() {
  let world = World::new();
  let run = world.stubbed_preflight_with(
    &format!("rust {}", world.rust_sha),
    "rust",
    &world.rust_sha,
    "disk_read_rate_mb() { echo 3; }\n",
  );

  assert_eq!(run.code, 0, "{}", world.report(&run));
  let quiet = "root disk read 3 MB/s over 0s, at or below 40 MB/s; proceeding";
  assert!(run.stdout.contains(quiet), "{}", world.report(&run));
  assert!(!run.stdout.contains("waiting for the root disk"), "{}", world.report(&run));
  assert!(run.stdout.contains("preflight passed:"), "{}", world.report(&run));
}

#[test]
fn test_disk_preflight_proceeds_after_a_busy_disk_falls_under_the_threshold() {
  let world = World::new();
  let run = world.stubbed_preflight_with(
    &format!("rust {}", world.rust_sha),
    "rust",
    &world.rust_sha,
    &sequence_sampler(&world, &[100, 5]),
  );

  assert_eq!(run.code, 0, "{}", world.report(&run));
  let waiting = "waiting for the root disk: 100 MB/s read over 0s, above 40 MB/s; up to 5s";
  assert!(run.stdout.contains(waiting), "{}", world.report(&run));
  let quiet = "root disk read 5 MB/s over 0s, at or below 40 MB/s; proceeding";
  assert!(run.stdout.contains(quiet), "{}", world.report(&run));
  assert!(run.stdout.contains("preflight passed:"), "{}", world.report(&run));
}

#[test]
fn test_disk_preflight_times_out_and_names_the_heaviest_cgroup_and_process() {
  let mut world = World::new();
  world.write_session_cgroup("charliebot-sess-aaaa", 1000);
  world.write_process(1234, "grep", 5000);
  let io_stat = world.cgroups.join("charliebot-sess-aaaa/io.stat");
  let process_io = world.proc.join("1234/io");
  // The sampler advances both counters while it runs, so the last sample sees a delta for each.
  let sampler = format!(
    r#"disk_read_rate_mb() {{
  printf '8:0 rbytes=2000 wbytes=0 rios=2 wios=0 dbytes=0 dios=0\n' > {io_stat}
  printf 'read_bytes: 9000\n' > {process_io}
  echo 100
}}
"#,
    io_stat = world::quote(&io_stat),
    process_io = world::quote(&process_io),
  );
  world.env.insert("CHARLIEBOT_SWITCH_DISK_WAIT_S".to_string(), "0".to_string());

  let run = world.stubbed_preflight_with(&format!("rust {}", world.rust_sha), "rust", &world.rust_sha, &sampler);

  assert_eq!(run.code, 1, "{}", world.report(&run));
  let logged = world.report(&run);
  assert!(logged.contains("waiting for the root disk: 100 MB/s"), "{logged}");
  assert!(logged.contains("preflight failed: the root disk read stayed above 40 MB/s within 0s"), "{logged}");
  assert!(logged.contains("cgroup charliebot-sess-aaaa 1000 bytes (io.stat)"), "{logged}");
  assert!(logged.contains("process 1234 (grep) 4000 bytes"), "{logged}");
  assert!(!logged.contains("stopping the old server"), "{logged}");
}

#[test]
fn test_session_rbytes_table_names_a_missing_io_stat_and_reads_proc_instead() {
  let world = World::new();
  let pid = 4321;
  world.write_session_cgroup_without_io("charliebot-sess-bbbb", "cpu memory pids", &[pid]);
  world.write_process(pid, "rg", 777);

  let run = world.disk_command("session_rbytes_table");

  assert_eq!(run.code, 1, "{}", run.describe());
  let io_stat = world.cgroups.join("charliebot-sess-bbbb/io.stat");
  let row = format!(
    "charliebot-sess-bbbb\tproc\t777\tio.stat missing: {}; cgroup.controllers reads: cpu memory pids",
    io_stat.display()
  );
  assert!(run.stdout.contains(&row), "{}", run.describe());
  assert!(!run.stdout.contains("charliebot-sess-bbbb\tio.stat\t0"), "{}", run.describe());
}

#[test]
fn test_switch_records_session_reads_before_the_stop_and_after_the_new_server_is_ready() {
  let mut world = World::new();
  world.write_session_cgroup("charliebot-sess-cccc", 4096);
  let pid = world.spawn_live();

  let run = world.switch_forward(pid, "", ":");

  assert_eq!(run.code, 0, "{}", world.report(&run));
  let logged = world.report(&run);
  let before = "session reads before the old server stops: charliebot-sess-cccc 4096 bytes from io.stat";
  let after = "session reads after the new server is ready: charliebot-sess-cccc 4096 bytes from io.stat";
  assert!(logged.contains(before), "{logged}");
  assert!(logged.contains(after), "{logged}");
  let stopped = logged.find("old server stopped").unwrap_or_else(|| panic!("{logged}"));
  assert!(logged.find(before).expect("the before record") < stopped, "{logged}");
  assert!(stopped < logged.find(after).expect("the after record"), "{logged}");
}

/// A sampler's exit code reaches the preflight: a sampler that cannot read the disk fails it instead of waiting.
#[test]
fn test_disk_preflight_fails_when_the_sampler_cannot_read_the_disk() {
  let world = World::new();
  let run = world.stubbed_preflight_with(
    &format!("rust {}", world.rust_sha),
    "rust",
    &world.rust_sha,
    "disk_read_rate_mb() { return 1; }\n",
  );

  assert_eq!(run.code, 1, "{}", world.report(&run));
  assert!(run.stdout.contains("cannot read the root disk's read rate"), "{}", world.report(&run));
  assert!(!run.stdout.contains("preflight passed:"), "{}", world.report(&run));
}

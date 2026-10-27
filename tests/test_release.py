"""The release entry point (scripts/release.sh) and the scripts it runs, against stubbed CI and stubbed stages.

Each test builds a throwaway git repository, release root and log root under tmp_path, so nothing touches
~/charliebot-releases or the live home. A stub `gh` answers the authentication, repository, run and artifact queries,
and CHARLIEBOT_RELEASE_TRIAL and CHARLIEBOT_RELEASE_REHEARSAL point at stub stage commands, so the preflight refusals
and the stage verdicts are exercised without downloading a binary or starting a server.
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import pathlib
import shutil
import subprocess

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[1]
RELEASE = ROOT / "scripts" / "release.sh"
SWEEP_CHECK = ROOT / "scripts" / "release" / "sweep-check.py"
TRIAL = ROOT / "scripts" / "release" / "trial.sh"
REHEARSAL = ROOT / "scripts" / "release" / "switch-rehearsal.sh"
GIT_ENV = {
    **os.environ,
    "GIT_AUTHOR_NAME": "release-test",
    "GIT_AUTHOR_EMAIL": "release-test@example.invalid",
    "GIT_COMMITTER_NAME": "release-test",
    "GIT_COMMITTER_EMAIL": "release-test@example.invalid",
}
SLUG = "example/charlie-bot"

# The stub gh. Its answers come from gh_behavior.json beside it; `run download` copies the artifact fixture into --dir.
GH_STUB = r'''#!/usr/bin/env python3
import json
import pathlib
import shutil
import sys

HERE = pathlib.Path(__file__).resolve().parent
BEHAVIOR = json.loads((HERE / "gh_behavior.json").read_text(encoding="utf-8"))
args = sys.argv[1:]


def value(flag):
  return args[args.index(flag) + 1]


if args[:2] == ["auth", "status"]:
  sys.exit(0 if BEHAVIOR["auth"] else 1)
if args[:2] == ["repo", "view"]:
  if not BEHAVIOR["auth"]:
    sys.exit(1)
  print(BEHAVIOR["slug"])
  sys.exit(0)
if args[:2] == ["run", "list"]:
  print(json.dumps(BEHAVIOR["runs"]))
  sys.exit(0)
if args[:2] == ["run", "view"]:
  print(json.dumps({"jobs": BEHAVIOR["jobs"].get(args[2], [])}))
  sys.exit(0)
if args[:2] == ["run", "download"]:
  if not BEHAVIOR["download_ok"]:
    sys.exit(1)
  destination = pathlib.Path(value("--dir"))
  destination.mkdir(parents=True, exist_ok=True)
  for name in ("charliebot-server", "sha256sums.txt"):
    shutil.copy2(pathlib.Path(BEHAVIOR["artifact_dir"]) / name, destination / name)
  sys.exit(0)
sys.exit(2)
'''

STAGE_STUB = "#!/usr/bin/env bash\nexit {code}\n"


@dataclasses.dataclass
class World:
  """One test's throwaway repository, release root, log root, stub directory and stub CI behavior."""

  root: pathlib.Path
  repo: pathlib.Path
  releases: pathlib.Path
  logs: pathlib.Path
  bin: pathlib.Path
  artifact: pathlib.Path
  on_main: str
  off_main: str
  env: dict[str, str]

  def write_gh(self, **behavior: object) -> None:
    defaults = {
        "auth": True,
        "slug": SLUG,
        "runs": [{
            "databaseId": 7,
            "event": "push",
            "headSha": self.on_main,
            "conclusion": "success"
        }],
        "jobs":
            {
                "7":
                    [
                        {
                            "name": "release-build",
                            "conclusion": "success"
                        }, {
                            "name": "rust-test",
                            "conclusion": "success"
                        }
                    ]
            },
        "download_ok": True,
        "artifact_dir": str(self.artifact),
    }
    (self.bin / "gh_behavior.json").write_text(json.dumps({**defaults, **behavior}), encoding="utf-8")


def _git(cwd: pathlib.Path, *args: str) -> str:
  return subprocess.run(["git", "-C", str(cwd), *args], capture_output=True, text=True, env=GIT_ENV,
                        check=True).stdout.strip()


def _build_world(root: pathlib.Path) -> World:
  repo = root / "repo"
  releases = root / "releases"
  logs = root / "logs"
  bin_dir = root / "bin"
  artifact = root / "artifact"
  for path in (repo / "scripts", releases, bin_dir, artifact):
    path.mkdir(parents=True)
  shutil.copy2(RELEASE, repo / "scripts" / "release.sh")
  (repo / "README.md").write_text("release test\n", encoding="utf-8")
  _git(repo, "init", "-q", "-b", "main")
  _git(repo, "add", ".")
  _git(repo, "commit", "-q", "-m", "the release commit")
  on_main = _git(repo, "rev-parse", "HEAD")
  _git(repo, "update-ref", "refs/remotes/origin/main", on_main)
  _git(repo, "commit", "-q", "--allow-empty", "-m", "a commit off origin/main")
  off_main = _git(repo, "rev-parse", "HEAD")

  binary = artifact / "charliebot-server"
  binary.write_text("fake server binary\n", encoding="utf-8")
  sums = hashlib.sha256(binary.read_bytes()).hexdigest()
  (artifact / "sha256sums.txt").write_text(f"{sums}  charliebot-server\n", encoding="utf-8")

  (bin_dir / "gh").write_text(GH_STUB, encoding="utf-8")
  (bin_dir / "gh").chmod(0o755)
  for name, code in (("stage-ok", 0), ("stage-fail", 3)):
    (bin_dir / name).write_text(STAGE_STUB.format(code=code), encoding="utf-8")
    (bin_dir / name).chmod(0o755)
  (releases / "prior").mkdir()
  (releases / "current").symlink_to("prior")

  env = {
      **os.environ,
      "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
      "CHARLIEBOT_RELEASES_DIR": str(releases),
      "CHARLIEBOT_RELEASE_LOG_DIR": str(logs),
      "CHARLIEBOT_RELEASE_TRIAL": str(bin_dir / "stage-ok"),
      "CHARLIEBOT_RELEASE_REHEARSAL": str(bin_dir / "stage-ok"),
  }
  return World(
      root=root,
      repo=repo,
      releases=releases,
      logs=logs,
      bin=bin_dir,
      artifact=artifact,
      on_main=on_main,
      off_main=off_main,
      env=env)


@pytest.fixture
def world(tmp_path: pathlib.Path) -> World:
  built = _build_world(tmp_path)
  built.write_gh()
  return built


def _release(world: World, commit: str) -> subprocess.CompletedProcess[str]:
  return subprocess.run(
      ["bash", str(world.repo / "scripts" / "release.sh"), commit],
      env=world.env,
      capture_output=True,
      text=True,
      timeout=60,
      check=False)


def _assert_refused_and_wrote_nothing(world: World, proc: subprocess.CompletedProcess[str]) -> None:
  assert proc.returncode == 2, proc.stdout + proc.stderr
  assert not (world.releases / world.on_main).exists()
  assert not world.logs.exists(), f"the refusal wrote {list(world.logs.rglob('*'))}"


def test_preflight_refuses_an_argument_that_is_not_a_full_sha(world: World) -> None:
  proc = _release(world, "main")
  _assert_refused_and_wrote_nothing(world, proc)
  assert "40-character" in proc.stderr


def test_preflight_refuses_a_commit_that_is_not_on_origin_main(world: World) -> None:
  proc = _release(world, world.off_main)
  _assert_refused_and_wrote_nothing(world, proc)
  assert "is not on origin/main" in proc.stderr


def test_preflight_refuses_a_commit_whose_release_exists(world: World) -> None:
  existing = world.releases / world.on_main
  existing.mkdir()
  proc = _release(world, world.on_main)
  assert proc.returncode == 2, proc.stdout + proc.stderr
  assert "already exists" in proc.stderr
  assert list(existing.iterdir()) == []
  assert not world.logs.exists(), f"the refusal wrote {list(world.logs.rglob('*'))}"


def test_preflight_refuses_without_gh_authentication(world: World) -> None:
  world.write_gh(auth=False)
  proc = _release(world, world.on_main)
  _assert_refused_and_wrote_nothing(world, proc)
  assert "gh is not authenticated" in proc.stderr


@pytest.mark.parametrize(
    "behavior", [
        {
            "runs": [{
                "databaseId": 7,
                "event": "push",
                "headSha": "0" * 40,
                "conclusion": "success"
            }]
        },
        {
            "runs": [{
                "databaseId": 7,
                "event": "push",
                "headSha": "__on_main__",
                "conclusion": "success"
            }],
            "jobs":
                {
                    "7":
                        [
                            {
                                "name": "release-build",
                                "conclusion": "success"
                            }, {
                                "name": "rust-test",
                                "conclusion": "failure"
                            }
                        ]
                },
        },
        {
            "runs": [{
                "databaseId": 7,
                "event": "push",
                "headSha": "__on_main__",
                "conclusion": "failure"
            }]
        },
    ],
    ids=["no-run-for-the-commit", "rust-test-failed", "run-failed"])
def test_preflight_refuses_a_commit_without_both_rust_jobs_green(world: World, behavior: dict[str, object]) -> None:
  runs = behavior.get("runs")
  assert isinstance(runs, list)
  for run in runs:
    if run.get("headSha") == "__on_main__":
      run["headSha"] = world.on_main
  world.write_gh(**behavior)
  proc = _release(world, world.on_main)
  _assert_refused_and_wrote_nothing(world, proc)
  assert "CI has no successful push run" in proc.stderr


def test_a_failed_stage_exits_1_names_the_stage_and_installs_nothing(world: World) -> None:
  world.env["CHARLIEBOT_RELEASE_TRIAL"] = str(world.bin / "stage-fail")
  proc = _release(world, world.on_main)
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert "stage trial failed" in proc.stderr
  log = next(world.logs.glob("*/trial.log"))
  assert f"stage trial failed: {log}" in proc.stderr
  assert not (world.releases / world.on_main).exists()
  assert not list(world.releases.glob(f".{world.on_main}.*"))
  assert os.readlink(world.releases / "current") == "prior"


def test_a_failed_download_stage_exits_1_and_installs_nothing(world: World) -> None:
  world.write_gh(download_ok=False)
  proc = _release(world, world.on_main)
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert "stage download failed" in proc.stderr
  assert not (world.releases / world.on_main).exists()


def test_a_passing_run_installs_the_three_files_and_leaves_current(world: World) -> None:
  proc = _release(world, world.on_main)
  assert proc.returncode == 0, proc.stdout + proc.stderr
  release = world.releases / world.on_main
  assert sorted(path.name for path in release.iterdir()) == ["charliebot-server", "release.log", "sha256sums.txt"]
  subprocess.run(["sha256sum", "-c", "sha256sums.txt"], cwd=release, capture_output=True, text=True, check=True)
  release_log = (release / "release.log").read_text(encoding="utf-8")
  for stage in ("download", "trial", "rehearsal", "install"):
    assert f"stage={stage} verdict=pass" in release_log
  assert os.readlink(world.releases / "current") == "prior"
  assert f"scripts/switch-server.sh rust {world.on_main}" in proc.stdout


# --- The moved helpers' own checks. ---

CONSOLE_LOG = """\
2026-10-21 10:00:00 [info     ] session_cgroup_enabled         memory_max_mb=1024 swap_max_mb=0 tasks_max=256
2026-10-21 10:00:01 [info     ] session_cgroup_sweep_removed   count=2 names=['charliebot-sess-aaaaaaaa', 'charliebot-sess-bbbbbbbb']
"""
JSON_LOG = """\
{"event": "session_cgroup_sweep_removed", "count": 2, "names": ["charliebot-sess-aaaaaaaa", "charliebot-sess-cccccccc"]}
"""


@pytest.mark.parametrize(
    "log_text,expected_rc",
    [
        (CONSOLE_LOG, 0),
        (JSON_LOG, 1),
    ],
    ids=["console-own-names", "json-foreign-name"],
)
def test_sweep_check_reports_each_removed_name_against_the_home(
    tmp_path: pathlib.Path, log_text: str, expected_rc: int) -> None:
  sessions = tmp_path / "sessions"
  (sessions / "aaaaaaaa-1111-4111-8111-111111111111").mkdir(parents=True)
  (sessions / "bbbbbbbb-2222-4222-8222-222222222222").mkdir()
  log = tmp_path / "server.log"
  log.write_text(log_text, encoding="utf-8")

  proc = subprocess.run(
      ["python3", "-I", str(SWEEP_CHECK), str(log), str(sessions)], capture_output=True, text=True, check=False)

  assert proc.returncode == expected_rc, proc.stdout + proc.stderr
  if expected_rc == 0:
    assert "foreign=none" in proc.stdout
  else:
    assert "foreign=charliebot-sess-cccccccc" in proc.stdout


def test_trial_helper_self_test_passes() -> None:
  proc = subprocess.run(["bash", str(TRIAL), "--self-test"], capture_output=True, text=True, timeout=30, check=False)
  assert proc.returncode == 0, proc.stdout + proc.stderr
  assert "PASS self-test delegation error reports the server's git timeout text" in proc.stdout


def test_rehearsal_usage_refuses_a_missing_argument() -> None:
  proc = subprocess.run(["bash", str(REHEARSAL), "a" * 40], capture_output=True, text=True, timeout=30, check=False)
  assert proc.returncode == 2
  assert "usage: switch-rehearsal.sh <new commit> <new release dir>" in proc.stderr

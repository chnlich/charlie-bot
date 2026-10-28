import gzip
import os
import pathlib
import shutil
import subprocess

import pytest


@pytest.fixture
def leak_repo(tmp_path: pathlib.Path) -> tuple[pathlib.Path, dict[str, str]]:
  root = pathlib.Path(__file__).resolve().parents[1]
  repo = tmp_path / 'repo'
  repo.mkdir()
  shutil.copytree(root / 'tools', repo / 'tools', ignore=shutil.ignore_patterns('*'))
  shutil.copy2(root / 'tools/check-skills-host-leak.sh', repo / 'tools')
  shutil.copy2(root / 'tools/leak-patterns.sh', repo / 'tools')
  shutil.copytree(root / 'tools/git-hooks', repo / 'tools/git-hooks')
  patterns = tmp_path / 'patterns'
  patterns.write_text('secretmarker\n')
  env = os.environ.copy()
  env.update(
      {
          'EXTRA_PATTERNS_FILE': str(patterns),
          'GIT_CONFIG_NOSYSTEM': '1',
          'GIT_CONFIG_GLOBAL': '/dev/null',
          'GIT_AUTHOR_NAME': 'Example',
          'GIT_AUTHOR_EMAIL': '123+example@users.noreply.github.com',
          'GIT_COMMITTER_NAME': 'Example',
          'GIT_COMMITTER_EMAIL': '123+example@users.noreply.github.com',
      })
  subprocess.run(['git', 'init', '-q', str(repo)], env=env, check=True)
  subprocess.run(['git', 'config', 'core.hooksPath', 'tools/git-hooks'], cwd=repo, env=env, check=True)
  return repo, env


def _run(repo: pathlib.Path, env: dict[str, str], *args: str) -> subprocess.CompletedProcess[str]:
  return subprocess.run(args, cwd=repo, env=env, text=True, capture_output=True, check=False)


def test_hooks_check_staged_bytes_and_commit_fields(leak_repo: tuple[pathlib.Path, dict[str, str]]) -> None:
  repo, env = leak_repo
  path = repo / 'ordinary.bin'
  path.write_bytes(b'\0SECRETmarker\0')
  assert _run(repo, env, 'git', 'add', 'ordinary.bin').returncode == 0
  path.write_bytes(b'clean')
  result = _run(repo, env, 'git', 'commit', '-m', 'clean message')
  assert result.returncode != 0
  assert 'ordinary.bin' in result.stderr
  assert _run(repo, env, 'git', 'add', 'ordinary.bin').returncode == 0
  result = _run(repo, env, 'git', 'commit', '-m', 'SECRETmarker')
  assert result.returncode != 0
  assert 'commit message' in result.stderr
  assert 'SECRETmarker' not in result.stderr
  bad_env = env | {'GIT_AUTHOR_EMAIL': 'secretmarker@example.invalid'}
  result = _run(repo, bad_env, 'git', 'commit', '-m', 'clean message')
  assert result.returncode != 0
  assert 'author identity' in result.stderr
  assert bad_env['GIT_AUTHOR_EMAIL'] not in result.stderr
  bad_env = env | {'GIT_COMMITTER_EMAIL': 'secretmarker@example.invalid'}
  result = _run(repo, bad_env, 'git', 'commit', '-m', 'clean message')
  assert result.returncode != 0
  assert 'committer identity' in result.stderr
  assert _run(repo, env, 'git', 'commit', '-m', 'clean message').returncode == 0


def test_host_scan_covers_tracked_gzip_and_explicit_files(leak_repo: tuple[pathlib.Path, dict[str, str]]) -> None:
  repo, env = leak_repo
  path = repo / 'fixture.gz'
  path.write_bytes(gzip.compress(b'private SECRETmarker content', mtime=0))
  assert _run(repo, env, 'git', 'add', 'fixture.gz').returncode == 0
  assert _run(repo, env, 'tools/check-skills-host-leak.sh').returncode != 0
  assert _run(repo, env, 'tools/check-skills-host-leak.sh', 'fixture.gz').returncode != 0
  path.write_bytes(b'bad gzip')
  assert _run(repo, env, 'tools/check-skills-host-leak.sh', 'fixture.gz').returncode != 0

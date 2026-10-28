"""Tests for tools/improve/github-post.sh: the leak gate on a round's GitHub writes.

The fixtures mirror tests/test_host_leak_hooks.py: a fake gh on PATH records its
arguments, a temp pattern file holds one fictional term, and the fixture text
uses fictional leak shapes (/home/someone/, example.ts.net). The profile home is
a directory under tmp_path, passed in the subprocess environment only.
"""

import os
import pathlib
import subprocess

import pytest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
GITHUB_POST = REPO_ROOT / 'tools/improve/github-post.sh'
LEAK_SCANNER = REPO_ROOT / 'tools/check-skills-host-leak.sh'
PATTERN_TERM = 'fictionalmarker'
PATTERN_FILE_COMMENT = '# local leak patterns'
HOME_PATH_TEXT = 'the notes live under /home/someone/notes'
TAILNET_TEXT = 'measured on example.ts.net'
CLEAN_BODY = '## Evidence\nbefore 259 ms, after 130 ms (n=50).\n'


def _run(args: list[str],
         cwd: pathlib.Path,
         env: dict[str, str],
         stdin_text: str | None = None) -> subprocess.CompletedProcess[str]:
  return subprocess.run(args, cwd=cwd, env=env, input=stdin_text, text=True, capture_output=True, check=False)


def _git(repo: pathlib.Path, env: dict[str, str], *args: str) -> None:
  subprocess.run(['git', '-C', str(repo), *args], env=env, text=True, capture_output=True, check=True)


@pytest.fixture
def post_env(tmp_path: pathlib.Path) -> tuple[dict[str, str], pathlib.Path, pathlib.Path]:
  """Returns (env, gh call log, profile home). The home reaches subprocesses only."""
  bin_dir = tmp_path / 'bin'
  bin_dir.mkdir()
  gh_log = tmp_path / 'gh-calls.log'
  fake_gh = bin_dir / 'gh'
  fake_gh.write_text('#!/usr/bin/env bash\nprintf \'%s\\n\' "$*" >> "$GH_CALL_LOG"\n')
  fake_gh.chmod(0o755)
  home = tmp_path / 'charliebot-home'
  home.mkdir()
  (home / 'skills_leak_patterns.local.txt').write_text(f'{PATTERN_FILE_COMMENT}\n{PATTERN_TERM}\n')
  env = os.environ.copy()
  env.update(
      {
          'PATH': f'{bin_dir}{os.pathsep}{env["PATH"]}',
          'GH_CALL_LOG': str(gh_log),
          'CHARLIEBOT_HOME': str(home),
          'GIT_CONFIG_NOSYSTEM': '1',
          'GIT_CONFIG_GLOBAL': '/dev/null',
          'GIT_AUTHOR_NAME': 'Example',
          'GIT_AUTHOR_EMAIL': '123+example@users.noreply.github.com',
          'GIT_COMMITTER_NAME': 'Example',
          'GIT_COMMITTER_EMAIL': '123+example@users.noreply.github.com',
      })
  return env, gh_log, home


@pytest.fixture
def push_repo(tmp_path: pathlib.Path, post_env) -> tuple[pathlib.Path, dict[str, str]]:
  """A work clone with a bare repository as origin, one commit ahead of origin/main."""
  env, _, _ = post_env
  origin = tmp_path / 'origin.git'
  clone = tmp_path / 'clone'
  subprocess.run(['git', 'init', '--bare', '-q', str(origin)], env=env, check=True)
  subprocess.run(['git', 'init', '-q', '-b', 'main', str(clone)], env=env, check=True)
  _git(clone, env, 'remote', 'add', 'origin', str(origin))
  (clone / 'seed.txt').write_text('seed\n')
  _git(clone, env, 'add', 'seed.txt')
  _git(clone, env, 'commit', '-q', '-m', 'seed commit')
  _git(clone, env, 'push', '-q', 'origin', 'main')
  _git(clone, env, 'switch', '-c', 'improve/perf/topic-slug')
  (clone / 'change.txt').write_text('change\n')
  _git(clone, env, 'add', 'change.txt')
  _git(clone, env, 'commit', '-q', '-m', 'topic change')
  return clone, env


def _gh_log_lines(gh_log: pathlib.Path) -> list[str]:
  if not gh_log.exists():
    return []
  return [line for line in gh_log.read_text().splitlines() if line]


def test_pr_comment_refuses_home_path(tmp_path: pathlib.Path, post_env) -> None:
  env, gh_log, _ = post_env
  body = tmp_path / 'body.md'
  body.write_text(f'## Evidence\n{HOME_PATH_TEXT}\n')
  result = _run([str(GITHUB_POST), 'pr-comment', '--body-file', str(body), '123'], cwd=tmp_path, env=env)
  assert result.returncode == 1
  assert 'body' in result.stderr
  assert _gh_log_lines(gh_log) == []


def test_pr_comment_refuses_tailnet_host(tmp_path: pathlib.Path, post_env) -> None:
  env, gh_log, _ = post_env
  body = tmp_path / 'body.md'
  body.write_text(f'## Evidence\n{TAILNET_TEXT}\n')
  result = _run([str(GITHUB_POST), 'pr-comment', '--body-file', str(body), '123'], cwd=tmp_path, env=env)
  assert result.returncode == 1
  assert _gh_log_lines(gh_log) == []


def test_pr_comment_refuses_pattern_file_term(tmp_path: pathlib.Path, post_env) -> None:
  env, gh_log, _ = post_env
  body = tmp_path / 'body.md'
  body.write_text(f'## Evidence\nthe {PATTERN_TERM} value\n')
  result = _run([str(GITHUB_POST), 'pr-comment', '--body-file', str(body), '123'], cwd=tmp_path, env=env)
  assert result.returncode == 1
  assert _gh_log_lines(gh_log) == []


def test_pr_comment_sends_clean_body(tmp_path: pathlib.Path, post_env) -> None:
  env, gh_log, _ = post_env
  body = tmp_path / 'body.md'
  body.write_text(CLEAN_BODY)
  result = _run([str(GITHUB_POST), 'pr-comment', '--body-file', str(body), '123'], cwd=tmp_path, env=env)
  assert result.returncode == 0
  assert len(_gh_log_lines(gh_log)) == 1
  assert 'pr comment' in _gh_log_lines(gh_log)[0]
  assert '123' in _gh_log_lines(gh_log)[0]


def test_missing_pattern_file_exits_2(tmp_path: pathlib.Path, post_env) -> None:
  env, gh_log, home = post_env
  (home / 'skills_leak_patterns.local.txt').unlink()
  body = tmp_path / 'body.md'
  body.write_text(CLEAN_BODY)
  result = _run([str(GITHUB_POST), 'pr-comment', '--body-file', str(body), '123'], cwd=tmp_path, env=env)
  assert result.returncode == 2
  assert _gh_log_lines(gh_log) == []


def test_pr_create_checks_title_and_body(tmp_path: pathlib.Path, post_env) -> None:
  env, gh_log, _ = post_env
  body = tmp_path / 'body.md'
  body.write_text(CLEAN_BODY)
  leaky_title = f'fix {HOME_PATH_TEXT}'
  result = _run(
      [str(GITHUB_POST), 'pr-create', '--title', leaky_title, '--body-file',
       str(body), '--base', 'main'],
      cwd=tmp_path,
      env=env)
  assert result.returncode == 1
  assert 'title' in result.stderr
  assert _gh_log_lines(gh_log) == []
  result = _run(
      [str(GITHUB_POST), 'pr-create', '--title', 'clean topic', '--body-file',
       str(body), '--base', 'main'],
      cwd=tmp_path,
      env=env)
  assert result.returncode == 0
  assert 'pr create' in _gh_log_lines(gh_log)[0]


def test_pr_close_sends_comment_from_file(tmp_path: pathlib.Path, post_env) -> None:
  env, gh_log, _ = post_env
  comment = tmp_path / 'comment.txt'
  comment.write_text('improve-abandoned: topic — reason\n')
  result = _run(
      [str(GITHUB_POST), 'pr-close', '--comment-file',
       str(comment), '--delete-branch', '123'], cwd=tmp_path, env=env)
  assert result.returncode == 0
  line = _gh_log_lines(gh_log)[0]
  assert 'pr close' in line
  assert '--delete-branch' in line
  assert '--comment' in line
  assert 'improve-abandoned: topic' in line


def test_push_refuses_leaky_commit_message(push_repo) -> None:
  clone, env = push_repo
  (clone / 'bad.txt').write_text('bad\n')
  _git(clone, env, 'add', 'bad.txt')
  _git(clone, env, 'commit', '-q', '-m', f'follow {HOME_PATH_TEXT}')
  result = _run([str(GITHUB_POST), 'push', '-u', 'origin', 'improve/perf/topic-slug'], cwd=clone, env=env)
  assert result.returncode == 1
  assert 'commit' in result.stderr
  refs = subprocess.run(
      ['git', 'ls-remote', 'origin'], cwd=clone, env=env, text=True, capture_output=True, check=True).stdout
  assert 'improve/perf/topic-slug' not in refs


def test_push_refuses_leaky_branch_name(push_repo) -> None:
  clone, env = push_repo
  branch = f'improve/perf/{PATTERN_TERM}-topic'
  _git(clone, env, 'switch', '-c', branch)
  (clone / 'change2.txt').write_text('change2\n')
  _git(clone, env, 'add', 'change2.txt')
  _git(clone, env, 'commit', '-q', '-m', 'topic change two')
  result = _run([str(GITHUB_POST), 'push', '-u', 'origin', branch], cwd=clone, env=env)
  assert result.returncode == 1
  assert 'branch name' in result.stderr
  refs = subprocess.run(
      ['git', 'ls-remote', 'origin'], cwd=clone, env=env, text=True, capture_output=True, check=True).stdout
  assert branch not in refs


def test_push_updates_bare_repository(push_repo) -> None:
  clone, env = push_repo
  result = _run([str(GITHUB_POST), 'push', '-u', 'origin', 'improve/perf/topic-slug'], cwd=clone, env=env)
  assert result.returncode == 0
  head = subprocess.run(
      ['git', '-C', str(clone), 'rev-parse', 'HEAD'], env=env, text=True, capture_output=True,
      check=True).stdout.strip()
  refs = subprocess.run(
      ['git', 'ls-remote', 'origin', 'refs/heads/improve/perf/topic-slug'],
      cwd=clone,
      env=env,
      text=True,
      capture_output=True,
      check=True).stdout
  assert head in refs


def test_stdin_mode_exit_codes(tmp_path: pathlib.Path, post_env) -> None:
  env, _, home = post_env
  env['EXTRA_PATTERNS_FILE'] = str(home / 'skills_leak_patterns.local.txt')
  hit = _run([str(LEAK_SCANNER), '--stdin'], cwd=tmp_path, env=env, stdin_text=f'{HOME_PATH_TEXT}\n')
  assert hit.returncode == 1
  assert '1:' in hit.stdout
  term = _run([str(LEAK_SCANNER), '--stdin'], cwd=tmp_path, env=env, stdin_text=f'the {PATTERN_TERM} value\n')
  assert term.returncode == 1
  clean = _run([str(LEAK_SCANNER), '--stdin'], cwd=tmp_path, env=env, stdin_text='clean text\n')
  assert clean.returncode == 0
  assert PATTERN_FILE_COMMENT not in term.stdout

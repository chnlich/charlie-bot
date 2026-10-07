"""Tests for the memory store's PR flow (src/features/memory/memory_proposal.py).

Each case runs the ``charliebot memory proposal`` CLI against a temporary
charliebot home whose live store is a git repo shaped like the real one
(an ``entries/`` tree, a ``topics`` vocabulary, a git-ignored ``staging/``).
Every refusal must exit 1 with the reason on stderr and leave the live
checkout, the branch, and the worktree exactly as they were.
"""

import subprocess
from pathlib import Path

import pytest
from conftest import CLI_MEMORY_HOME_PATCH_TARGET, legacy_memory_entry_text, run_git
from conftest import write_memory_entry as _write_entry
from conftest import write_memory_topics as _write_topics

from src.features.memory import memory_proposal


def _build_store(home: Path) -> Path:
  """One committed store-shaped repo at <home>/memory, on branch main."""
  mem = home / "memory"
  _write_topics(mem)
  _write_entry(mem, "profile", "dark-mode")
  (mem / "staging").mkdir()
  (mem / ".gitignore").write_text("staging/\n", encoding="utf-8")
  run_git(mem, "init", "-q", "-b", "main")
  run_git(mem, "config", "user.email", "t@t.t")
  run_git(mem, "config", "user.name", "t")
  run_git(mem, "add", "-A")
  run_git(mem, "commit", "-qm", "base")
  return mem


@pytest.fixture()
def store(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """A temp charliebot home whose live store the CLI resolves as its own."""
  home = tmp_path / "home"
  home.mkdir()
  mem = _build_store(home)
  monkeypatch.setattr(CLI_MEMORY_HOME_PATCH_TARGET, lambda: home)
  return mem


def _run_cli(monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture, *argv: str):
  """Run the memory CLI; return (exit code, stdout, stderr)."""
  from src.features.memory import cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", *argv])
  try:
    cli.main()
  except SystemExit as e:
    code = e.code
  else:
    code = 0
  captured = capsys.readouterr()
  return code, captured.out, captured.err


def _fields(out: str) -> dict[str, str]:
  return dict(line.split(": ", 1) for line in out.strip().splitlines())


def _message_file(store: Path, message: str) -> Path:
  """A commit-message file outside both the store and its worktree: an untracked
  file inside the worktree would read as uncommitted work."""
  messages = store.parent / "messages"
  messages.mkdir(exist_ok=True)
  path = messages / f"{abs(hash(message)) % 10**10}.txt"
  path.write_text(message, encoding="utf-8")
  return path


def _entry_text(slug: str, body: str) -> str:
  return f"---\nscope: user\ntopic: profile\ntitle: {slug.replace('-', ' ').title()}\naudience: master, worker\n---\n{body}"


def _pr_commit(store: Path, rel_path: Path, text: str, message: str) -> str:
  """Write one file into the PR worktree and commit it through the module."""
  worktree = memory_proposal.proposal_worktree(store)
  target = worktree / rel_path
  target.parent.mkdir(parents=True, exist_ok=True)
  target.write_text(text, encoding="utf-8")
  return memory_proposal.commit(store, rel_path.as_posix(), _message_file(store, message))


def _live_commit(store: Path, rel_path: Path, text: str, message: str) -> None:
  """Commit one file on the live checkout (the base side)."""
  target = store / rel_path
  target.parent.mkdir(parents=True, exist_ok=True)
  target.write_text(text, encoding="utf-8")
  run_git(store, "add", rel_path.as_posix())
  run_git(store, "commit", "-qm", message)


# --- open ---------------------------------------------------------------------


def test_first_open_creates_branch_and_worktree(store: Path, monkeypatch, capsys) -> None:
  code, out, err = _run_cli(monkeypatch, capsys, "proposal", "open")
  assert code == 0, err
  fields = _fields(out)
  assert fields["ahead"] == "0"
  assert fields["base"] == "main"
  assert fields["dirty"] == "no"
  worktree = memory_proposal.proposal_worktree(store)
  assert Path(fields["worktree"]) == worktree
  assert worktree.is_dir()
  # The branch exists at base and the worktree checked it out.
  assert run_git(store, "rev-parse", "--verify", "-q",
                 "refs/heads/proposal").strip() == run_git(store, "rev-parse", "main").strip()
  assert run_git(worktree, "symbolic-ref", "--short", "HEAD").strip() == "proposal"


def test_open_with_commits_ahead_and_unchanged_base_keeps_them(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  _pr_commit(store, Path("entries/profile/new-entry.md"), _entry_text("new-entry", "body\n"), "add new")
  head_before = run_git(store, "rev-parse", "proposal").strip()
  code, out, _err = _run_cli(monkeypatch, capsys, "proposal", "open")
  assert code == 0
  fields = _fields(out)
  assert fields["ahead"] == "1"
  assert fields["head"] == head_before


def test_open_rebases_onto_moved_base(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  _pr_commit(store, Path("entries/profile/rebased.md"), _entry_text("rebased", "body\n"), "add rebased")
  pr_head = run_git(store, "rev-parse", "proposal").strip()
  # The base moves by a non-conflicting commit on the live checkout.
  _live_commit(store, Path("entries/profile/base-side.md"), _entry_text("base-side", "base body\n"), "advance base")
  new_base = run_git(store, "rev-parse", "main").strip()

  code, out, err = _run_cli(monkeypatch, capsys, "proposal", "open")
  assert code == 0, err
  fields = _fields(out)
  assert fields["ahead"] == "1"
  # The PR commit now sits on the new base.
  assert run_git(store, "rev-parse", "proposal~1").strip() == new_base
  assert run_git(store, "rev-parse", "proposal").strip() != pr_head


def test_open_conflicting_base_keeps_branch_at_old_sha(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  # The PR rewrites the first topics line; the base rewrites the same line.
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / "topics").write_text("profile resident PR edit\nworkflow resident\n", encoding="utf-8")
  memory_proposal.commit(store, "topics", _message_file(store, "edit topics on the PR"))
  pr_head = run_git(store, "rev-parse", "proposal").strip()
  (store / "topics").write_text("profile resident live edit\nworkflow resident\n", encoding="utf-8")
  run_git(store, "commit", "-qam", "conflicting base edit")

  code, _out, err = _run_cli(monkeypatch, capsys, "proposal", "open")
  assert code == 1
  assert "conflicted" in err
  assert run_git(store, "rev-parse", "proposal").strip() == pr_head
  # The rebase was aborted: no rebase left in progress, nothing staged.
  assert run_git(worktree, "rev-parse", "-q", "--verify", "REBASE_HEAD", check=False).strip() == ""
  assert not run_git(worktree, "status", "--porcelain").strip()


def test_open_refuses_uncommitted_live_change_and_touches_nothing(tmp_path: Path, monkeypatch, capsys) -> None:
  home = tmp_path / "home"
  home.mkdir()
  store = _build_store(home)
  monkeypatch.setattr(CLI_MEMORY_HOME_PATCH_TARGET, lambda: home)
  (store / "topics").write_text("profile resident\nworkflow resident\nhost resident\n", encoding="utf-8")
  code, _out, err = _run_cli(monkeypatch, capsys, "proposal", "open")
  assert code == 1
  assert "uncommitted" in err
  # Nothing was created: no worktree, no proposal branch.
  assert not memory_proposal.proposal_worktree(store).exists()
  assert run_git(store, "rev-parse", "--verify", "-q", "refs/heads/proposal", check=False).strip() == ""


def test_open_refuses_detached_live_checkout(tmp_path: Path, monkeypatch, capsys) -> None:
  home = tmp_path / "home"
  home.mkdir()
  store = _build_store(home)
  monkeypatch.setattr(CLI_MEMORY_HOME_PATCH_TARGET, lambda: home)
  head = run_git(store, "rev-parse", "HEAD").strip()
  run_git(store, "checkout", "-q", "--detach", head)
  code, _out, err = _run_cli(monkeypatch, capsys, "proposal", "open")
  assert code == 1
  assert "not on a branch" in err
  assert not memory_proposal.proposal_worktree(store).exists()


def test_open_foreign_worktree_refuses_and_creates_no_branch(store: Path, monkeypatch, capsys) -> None:
  """A directory at the worktree path that is not this repo's worktree refuses
  before anything is created: no proposal branch, the stranger repo untouched."""
  worktree = memory_proposal.proposal_worktree(store)
  worktree.mkdir()
  run_git(worktree, "init", "-q", "-b", "main")
  run_git(worktree, "config", "user.email", "t@t.t")
  run_git(worktree, "config", "user.name", "t")
  (worktree / "foreign.txt").write_text("mine\n", encoding="utf-8")
  run_git(worktree, "add", "-A")
  run_git(worktree, "commit", "-qm", "foreign")
  code, _out, err = _run_cli(monkeypatch, capsys, "proposal", "open")
  assert code == 1
  # The refusal fires on the worktree validation, before the branch is created.
  assert "is a worktree on 'main', not 'proposal'" in err
  assert run_git(store, "rev-parse", "--verify", "-q", "refs/heads/proposal", check=False).strip() == ""
  assert (worktree / "foreign.txt").read_text(encoding="utf-8") == "mine\n"


# --- commit -------------------------------------------------------------------


def test_commit_accepts_new_entry_file(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/new-entry.md")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
  (worktree / rel).write_text(_entry_text("new-entry", "body\n"), encoding="utf-8")
  code, out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file",
      str(_message_file(store, "admit profile/new-entry (New Entry)")))
  assert code == 0, err
  fields = _fields(out)
  assert fields["committed"] == run_git(store, "rev-parse", "proposal").strip()
  assert run_git(worktree, "ls-tree", "-r", "HEAD", "--name-only").splitlines() == [
      ".gitignore", "entries/profile/dark-mode.md", "entries/profile/new-entry.md", "topics"
  ]


def test_commit_accepts_existing_pr_entry_with_lines_appended(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/appended.md")
  text = _entry_text("appended", "first line\n")
  _pr_commit(store, rel, text, "first")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text(text + "second line\n", encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "second")))
  assert code == 0, err
  assert "second line" in (worktree / rel).read_text(encoding="utf-8")


def test_commit_resolves_a_relative_message_file_against_the_caller_cwd(
    store: Path, monkeypatch, capsys, tmp_path: Path) -> None:
  """A relative --message-file names a file in the caller's cwd, not the worktree's."""
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/relative-message.md")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
  (worktree / rel).write_text(_entry_text("relative-message", "body\n"), encoding="utf-8")
  caller_cwd = tmp_path / "caller"
  caller_cwd.mkdir()
  (caller_cwd / "message.txt").write_text("admit via a relative message file\n", encoding="utf-8")
  monkeypatch.chdir(caller_cwd)
  code, _out, err = _run_cli(monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", "message.txt")
  assert code == 0, err
  assert run_git(worktree, "log", "-1", "--format=%B").strip() == "admit via a relative message file"


def test_commit_refuses_reworded_pr_line_and_lists_it(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/reworded.md")
  text = _entry_text("reworded", "alpha bravo\n")
  _pr_commit(store, rel, text, "first")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text(text.replace("alpha bravo", "alpha CHARLIE"), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "reword")))
  assert code == 1
  # The refusal lists the line the PR added and the worktree lost.
  assert "alpha bravo" in err
  # Nothing was committed.
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 1


def test_commit_refuses_line_reworded_to_the_base_wording(store: Path, monkeypatch, capsys) -> None:
  """A PR-added line may not be swapped back to the base's own wording either."""
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/swap.md")
  base_text = _entry_text("swap", "original body\n")
  _live_commit(store, rel, base_text, "base side of swap")
  # The PR rewrites the base line; the commit is allowed (it is the PR's diff).
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text(_entry_text("swap", "reworded body\n"), encoding="utf-8")
  memory_proposal.commit(store, rel.as_posix(), _message_file(store, "pr rewrites the line"))
  # The worktree then restores the base's original wording: the PR-added line
  # is gone even though the replacement text is the base file's own line.
  (worktree / rel).write_text(base_text, encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "restore")))
  assert code == 1
  assert "reworded body" in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 1


def test_commit_counts_line_occurrences_as_a_multiset(store: Path, monkeypatch, capsys) -> None:
  """A line the PR added twice must survive twice: removing one occurrence refuses."""
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/doubled.md")
  text = _entry_text("doubled", "dup line\ndup line\n")
  _pr_commit(store, rel, text, "first")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text(_entry_text("doubled", "dup line\n"), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file",
      str(_message_file(store, "drop one")))
  assert code == 1
  assert "dup line" in err


def test_commit_refuses_deleted_pr_created_entry(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/doomed.md")
  _pr_commit(store, rel, _entry_text("doomed", "keep me\n"), "first")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).unlink()
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "delete")))
  assert code == 1
  assert "keep me" in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 1


def _long_sentence() -> str:
  """One 30-word sentence: over the 25-word cap the prose check enforces."""
  return (
      "The curator drafts each entry inside the proposal worktree so the diff against the base branch "
      "shows exactly the lines awaiting the user's approval before anything reaches the live store.")


def test_commit_refuses_a_new_sentence_over_25_words(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/long-sentence.md")
  sentence = _long_sentence()
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
  (worktree / rel).write_text(_entry_text("long-sentence", f"- {sentence}\n"), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "admit")))
  assert code == 1
  assert "prose check failed:" in err
  assert f"sentence over 25 words (30): {sentence}" in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 0


def test_commit_ignores_an_unchanged_long_sentence_in_an_old_entry(store: Path, monkeypatch, capsys) -> None:
  """A bullet the base already holds verbatim escapes the sentence checks, so old long
  sentences never block a commit that edits other lines of their entry."""
  rel = Path("entries/profile/legacy-long.md")
  base_text = _entry_text("legacy-long", f"- {_long_sentence()}\n")
  _live_commit(store, rel, base_text, "base holds the long sentence")
  _run_cli(monkeypatch, capsys, "proposal", "open")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text(base_text + "- Short added line.\n", encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "append")))
  assert code == 0, err
  assert "Short added line" in (worktree / rel).read_text(encoding="utf-8")


def test_commit_refuses_a_new_sentence_with_a_semicolon(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/semicolon.md")
  bullet = "The curator drafts the entry; the reviewer commits it."
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
  (worktree / rel).write_text(_entry_text("semicolon", f"- {bullet}\n"), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "admit")))
  assert code == 1
  assert f"semicolon joins clauses: {bullet}" in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 0


def test_commit_allows_a_semicolon_inside_backticks(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/backtick-semicolon.md")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
  (worktree / rel).write_text(_entry_text("backtick-semicolon", "- Run `a; b` twice.\n"), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "admit")))
  assert code == 0, err
  assert "prose check" not in err


def test_commit_refuses_a_long_sentence_in_the_lead(store: Path, monkeypatch, capsys) -> None:
  """The body lines before the first bullet form one unit that takes the sentence checks."""
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/long-lead.md")
  sentence = _long_sentence()
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
  (worktree / rel).write_text(_entry_text("long-lead", f"{sentence}\n- Short bullet stands.\n"), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "admit")))
  assert code == 1
  assert f"sentence over 25 words (30): {sentence}" in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 0


def test_commit_refuses_a_semicolon_in_the_lead(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/semicolon-lead.md")
  lead = "The curator drafts the entry; the reviewer commits it."
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
  (worktree / rel).write_text(_entry_text("semicolon-lead", f"{lead}\n- Short bullet stands.\n"), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "admit")))
  assert code == 1
  assert f"semicolon joins clauses: {lead}" in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 0


def test_commit_refuses_a_long_sentence_in_an_entry_without_bullets(store: Path, monkeypatch, capsys) -> None:
  """Without bullets the whole body is one unit, so a sentence wrapped across two lines
  is checked as the joined sentence."""
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/no-bullets.md")
  sentence = _long_sentence()
  first, second = sentence.split(" so the diff ")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
  (worktree / rel).write_text(_entry_text("no-bullets", f"{first}\nso the diff {second}\n"), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "admit")))
  assert code == 1
  assert f"sentence over 25 words (30): {sentence}" in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 0


def test_commit_ignores_an_unchanged_long_sentence_in_an_old_lead(store: Path, monkeypatch, capsys) -> None:
  rel = Path("entries/profile/legacy-lead.md")
  base_text = _entry_text("legacy-lead", f"{_long_sentence()}\n- Legacy bullet stands.\n")
  _live_commit(store, rel, base_text, "base lead holds the long sentence")
  _run_cli(monkeypatch, capsys, "proposal", "open")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text(base_text + "- Short added line.\n", encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "append")))
  assert code == 0, err
  assert "Short added line" in (worktree / rel).read_text(encoding="utf-8")


def test_commit_keeps_the_lead_apart_from_the_first_bullet(store: Path, monkeypatch, capsys) -> None:
  """The lead's last sentence ends without a period: joined with the first bullet it would
  run to 31 words, so the commit passes only while the two stay separate units."""
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/compliant-lead.md")
  body = (
      "Entries live under the store root. Each entry states one fact about the host and names the file that owns it\n"
      "- Run the store lint before each commit so a broken entry never reaches the live store.\n"
      "- Keep each bullet short.\n")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
  (worktree / rel).write_text(_entry_text("compliant-lead", body), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "admit")))
  assert code == 0, err
  assert "prose check" not in err


def test_commit_refuses_a_body_grown_past_12_lines(store: Path, monkeypatch, capsys) -> None:
  rel = Path("entries/profile/grown.md")
  base_text = _entry_text("grown", "".join(f"- Legacy line {i} stands.\n" for i in range(1, 12)))
  _live_commit(store, rel, base_text, "base body of 11 lines")
  _run_cli(monkeypatch, capsys, "proposal", "open")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text(base_text + "- New line one stands.\n- New line two stands.\n", encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "grow")))
  assert code == 1
  assert "body has 13 lines (limit 12)" in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 0


def test_commit_refuses_a_15_line_body_that_stays_at_15_lines(store: Path, monkeypatch, capsys) -> None:
  """The body limit binds every committed entry, so rewriting inside a 15-line body refuses
  even though the line count never grows."""
  rel = Path("entries/profile/rewritten-long.md")
  base_text = _entry_text("rewritten-long", "".join(f"- Legacy line {i} stands.\n" for i in range(1, 16)))
  _live_commit(store, rel, base_text, "base body of 15 lines")
  _run_cli(monkeypatch, capsys, "proposal", "open")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text(
      base_text.replace("- Legacy line 7 stands.\n", "- Line seven stands rewritten.\n"), encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "rewrite")))
  assert code == 1
  assert "body has 15 lines (limit 12)" in err
  assert "Line seven stands rewritten" not in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 0


def test_commit_replace_pr_lines_commits_and_lists_the_replaced_lines(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/replaced.md")
  text = _entry_text("replaced", "alpha bravo\n")
  _pr_commit(store, rel, text, "first")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text(text.replace("alpha bravo", "alpha CHARLIE"), encoding="utf-8")
  code, out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "replace")),
      "--replace-pr-lines")
  assert code == 0, err
  assert _fields(out)["committed"] == run_git(store, "rev-parse", "proposal").strip()
  message = subprocess.run(
      ["git", "log", "-1", "--format=%B"], cwd=str(worktree), capture_output=True, text=True, check=True).stdout
  # %B prints the stored message (one trailing newline) plus git log's entry terminator.
  assert message == "replace\nReplaced PR lines:\n  alpha bravo\n\n"


def test_commit_refuses_a_prose_violation_even_with_replace_pr_lines(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  rel = Path("entries/profile/flagged.md")
  text = _entry_text("flagged", "alpha bravo\n")
  _pr_commit(store, rel, text, "first")
  worktree = memory_proposal.proposal_worktree(store)
  rewritten = _entry_text("flagged", f"alpha CHARLIE\n- {_long_sentence()}\n")
  (worktree / rel).write_text(rewritten, encoding="utf-8")
  code, _out, err = _run_cli(
      monkeypatch, capsys, "proposal", "commit", rel.as_posix(), "--message-file", str(_message_file(store, "replace")),
      "--replace-pr-lines")
  assert code == 1
  assert "prose check failed:" in err
  assert "sentence over 25 words" in err
  assert len(run_git(store, "rev-list", "main..proposal").splitlines()) == 1


# --- land ---------------------------------------------------------------------


def _three_pr_commits(store: Path) -> list[str]:
  """Three PR commits, each adding one entry file; returns their SHAs oldest first."""
  return [
      _pr_commit(
          store, Path(f"entries/profile/numbered-{i}.md"), _entry_text(f"numbered-{i}", f"body {i}\n"),
          f"admit numbered {i}") for i in (1, 2, 3)
  ]


def test_land_middle_commit_moves_live_head_and_keeps_the_rest(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  _first, middle, _last = _three_pr_commits(store)
  old_base = run_git(store, "rev-parse", "main").strip()
  code, out, err = _run_cli(monkeypatch, capsys, "proposal", "land", middle)
  assert code == 0, err
  fields = _fields(out)
  assert fields["commits"] == "2"
  assert fields["remaining"] == "1"
  assert fields["landed"] == (
      f"{run_git(store, 'rev-parse', '--short', old_base).strip()}.."
      f"{run_git(store, 'rev-parse', '--short', middle).strip()}")
  # The live head is exactly the approved SHA, on the base branch.
  assert run_git(store, "rev-parse", "HEAD").strip() == middle
  assert run_git(store, "symbolic-ref", "--short", "HEAD").strip() == "main"
  # The last commit stays on the proposal branch for the next PR.
  assert run_git(store, "rev-list", "--count", f"{middle}..proposal").strip() == "1"
  run_git(store, "cat-file", "-e", "proposal:entries/profile/numbered-3.md")


def test_land_refuses_sha_not_on_proposal(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  _three_pr_commits(store)
  # A commit on the live checkout that the proposal branch never carried.
  _live_commit(store, Path("entries/profile/base-side.md"), _entry_text("base-side", "base body\n"), "live-side")
  live_head = run_git(store, "rev-parse", "HEAD").strip()
  code, _out, err = _run_cli(monkeypatch, capsys, "proposal", "land", live_head)
  assert code == 1
  assert "not on the 'proposal' branch" in err
  assert run_git(store, "rev-parse", "main").strip() == live_head


def test_land_refuses_dirty_live_checkout(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  _first, middle, _last = _three_pr_commits(store)
  live_head = run_git(store, "rev-parse", "HEAD").strip()
  (store / "topics").write_text("dirty edit\n", encoding="utf-8")
  code, _out, err = _run_cli(monkeypatch, capsys, "proposal", "land", middle)
  assert code == 1
  assert "not clean" in err
  assert run_git(store, "rev-parse", "HEAD").strip() == live_head


def test_land_refuses_tree_failing_lint(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  # A legacy (v1) entry: strict v2 lint flags 'created' in entries/.
  worktree = memory_proposal.proposal_worktree(store)
  rel = Path("entries/profile/legacy.md")
  (worktree / rel).write_text(legacy_memory_entry_text("profile", "legacy"), encoding="utf-8")
  memory_proposal.commit(store, rel.as_posix(), _message_file(store, "admit legacy"))
  head_before = run_git(store, "rev-parse", "HEAD").strip()
  pr_head = run_git(store, "rev-parse", "proposal").strip()
  code, _out, err = _run_cli(monkeypatch, capsys, "proposal", "land", pr_head)
  assert code == 1
  assert "store lint" in err
  assert run_git(store, "rev-parse", "HEAD").strip() == head_before


# --- status -------------------------------------------------------------------


def test_status_prints_every_field(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  _pr_commit(store, Path("entries/profile/statused.md"), _entry_text("statused", "body\n"), "add statused")
  code, out, err = _run_cli(monkeypatch, capsys, "proposal", "status")
  assert code == 0, err
  fields = _fields(out)
  head = run_git(store, "rev-parse", "proposal").strip()
  assert list(fields) == ["worktree", "base", "head", "ahead", "opened", "dirty", "diff_path"]
  assert Path(fields["worktree"]) == memory_proposal.proposal_worktree(store)
  assert fields["base"] == "main"
  assert fields["head"] == head
  assert fields["ahead"] == "1"
  # The opened date is the oldest ahead commit's committer date, YYYY-MM-DD.
  expected_date = run_git(store, "show", "--no-patch", "--date=short", "--format=%cd", head).strip()
  assert fields["opened"] == expected_date
  assert fields["dirty"] == "no"
  assert fields["diff_path"] == f"/diff?repo={store}&base=main&head={head}"


def test_status_tracks_worktree_dirty_state(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  _code, out, _err = _run_cli(monkeypatch, capsys, "proposal", "status")
  assert _fields(out)["dirty"] == "no"
  rel = Path("entries/profile/dirtying.md")
  worktree = memory_proposal.proposal_worktree(store)
  (worktree / rel).write_text("drafted, not committed\n", encoding="utf-8")
  _code, out, _err = _run_cli(monkeypatch, capsys, "proposal", "status")
  assert _fields(out)["dirty"] == "yes"


def test_status_dirty_dash_when_worktree_absent(store: Path, monkeypatch, capsys) -> None:
  _run_cli(monkeypatch, capsys, "proposal", "open")
  _pr_commit(store, Path("entries/profile/detached-worktree.md"), _entry_text("detached-worktree", "body\n"), "add")
  worktree = memory_proposal.proposal_worktree(store)
  run_git(store, "worktree", "remove", str(worktree))
  code, out, err = _run_cli(monkeypatch, capsys, "proposal", "status")
  assert code == 0, err
  fields = _fields(out)
  assert fields["dirty"] == "-"
  assert fields["ahead"] == "1"

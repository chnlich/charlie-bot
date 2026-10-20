"""Tests for the standalone Google account table script."""

from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import stat
import sys
import urllib.parse
from typing import Any, Self

import pytest
import yaml

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT_PATH = ROOT / "skills/google-oauth/scripts/google_account.py"
SPEC = importlib.util.spec_from_file_location("google_account_script", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
google_account = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = google_account
SPEC.loader.exec_module(google_account)


class FakeResponse:

  def __init__(self, payload: dict[str, Any]) -> None:
    self.payload = payload

  def __enter__(self) -> Self:
    return self

  def __exit__(self, *args: object) -> None:
    return None

  def read(self) -> bytes:
    return json.dumps(self.payload).encode("utf-8")


@pytest.fixture
def credentials_file(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
  home = tmp_path / "profile"
  home.mkdir()
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  path = home / "credentials.yaml"
  path.write_text(
      "google:\n"
      "  client_id: default-client\n"
      "  client_secret: default-secret\n"
      "  refresh_token: default-refresh\n",
      encoding="utf-8")
  return path


def stub_urlopen(
    monkeypatch: pytest.MonkeyPatch, payloads: list[dict[str, Any]], requests: list[Any] | None = None) -> None:

  def fake_urlopen(request: Any, timeout: int) -> FakeResponse:
    if requests is not None:
      requests.append(request)
    assert timeout == 30
    return FakeResponse(payloads.pop(0))

  monkeypatch.setattr(google_account.urllib.request, "urlopen", fake_urlopen)


def run_cli(args: list[str], capsys: pytest.CaptureFixture[str]) -> tuple[int, str, str]:
  exit_code = google_account.main(args)
  stdout, stderr = capsys.readouterr()
  return exit_code, stdout, stderr


def test_token_uses_default_account(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  requests: list[Any] = []
  stub_urlopen(monkeypatch, [{"access_token": "default-access"}], requests)

  exit_code, stdout, stderr = run_cli(["token"], capsys)

  assert exit_code == 0
  assert stdout == "default-access\n"
  assert stderr == ""
  fields = urllib.parse.parse_qs(requests[0].data.decode("utf-8"))
  assert fields["client_id"] == ["default-client"]
  assert fields["refresh_token"] == ["default-refresh"]


def test_token_uses_named_account(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("google_account_personal:\n  refresh_token: personal-refresh\n")
  requests: list[Any] = []
  stub_urlopen(monkeypatch, [{"access_token": "personal-access"}], requests)

  exit_code, stdout, stderr = run_cli(["token", "--account", "personal"], capsys)

  assert exit_code == 0
  assert stdout == "personal-access\n"
  assert stderr == ""
  fields = urllib.parse.parse_qs(requests[0].data.decode("utf-8"))
  assert fields["client_id"] == ["default-client"]
  assert fields["refresh_token"] == ["personal-refresh"]


def test_unknown_account_lists_names(credentials_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("google_account_personal:\n  refresh_token: personal-refresh\n")

  exit_code, stdout, stderr = run_cli(["token", "--account", "missing"], capsys)

  assert exit_code == 2
  assert stdout == ""
  assert "default" in stderr
  assert "Available accounts:" in stderr


def test_empty_account_name_is_rejected(credentials_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  exit_code, stdout, stderr = run_cli(["token", "--account", ""], capsys)

  assert exit_code == 2
  assert stdout == ""
  assert "Available accounts: default" in stderr


def test_duplicate_account_names_are_rejected(
    credentials_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  credentials_file.write_text(
      "google:\n"
      "  name: personal\n"
      "  client_id: client\n"
      "  client_secret: secret\n"
      "  refresh_token: default-refresh\n"
      "google_account_personal:\n"
      "  refresh_token: personal-refresh\n",
      encoding="utf-8")

  exit_code, stdout, stderr = run_cli(["list"], capsys)

  assert exit_code == 2
  assert stdout == ""
  assert "duplicate Google account name" in stderr


def test_pending_account_cannot_mint_token(credentials_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("google_account_personal:\n  description: personal mailbox\n")

  exit_code, stdout, stderr = run_cli(["token", "--account", "personal"], capsys)

  assert exit_code == 1
  assert stdout == ""
  assert "personal" in stderr
  assert "pending authorization" in stderr


def test_account_specific_client_is_used(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write(
        "google_account_personal:\n"
        "  client_id: personal-client\n"
        "  client_secret: personal-secret\n"
        "  refresh_token: personal-refresh\n")
  requests: list[Any] = []
  stub_urlopen(monkeypatch, [{"access_token": "personal-access"}], requests)

  exit_code, stdout, _ = run_cli(["token", "--account", "personal"], capsys)

  assert exit_code == 0
  fields = urllib.parse.parse_qs(requests[0].data.decode("utf-8"))
  assert fields["client_id"] == ["personal-client"]
  assert fields["client_secret"] == ["personal-secret"]
  assert "personal-secret" not in stdout


def test_consent_url_has_scopes_state_and_optional_login_hint(
    credentials_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  exit_code, stdout, stderr = run_cli(["consent-url", "personal", "--email", "person@example.invalid"], capsys)

  assert exit_code == 0
  assert stderr == ""
  parameters = urllib.parse.parse_qs(urllib.parse.urlsplit(stdout.strip()).query)
  assert parameters["response_type"] == ["code"]
  assert parameters["login_hint"] == ["person@example.invalid"]
  assert parameters["state"] == ["personal"]
  assert parameters["redirect_uri"] == ["http://localhost"]
  assert "https://www.googleapis.com/auth/calendar.readonly" in parameters["scope"][0].split()


def test_consent_url_omits_login_hint_without_email(
    credentials_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  exit_code, stdout, _ = run_cli(["consent-url", "personal"], capsys)

  assert exit_code == 0
  parameters = urllib.parse.parse_qs(urllib.parse.urlsplit(stdout.strip()).query)
  assert "login_hint" not in parameters


def add_responses(monkeypatch: pytest.MonkeyPatch) -> None:
  stub_urlopen(
      monkeypatch, [
          {
              "access_token": "temporary-access",
              "refresh_token": "new-refresh"
          },
          {
              "emailAddress": "person@example.invalid"
          },
      ])


def test_add_appends_section_without_changing_prior_bytes_or_mode(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  original = credentials_file.read_bytes()
  os.chmod(credentials_file, 0o640)
  add_responses(monkeypatch)

  exit_code, stdout, stderr = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=personal", "--description", "personal: mailbox"
      ], capsys)

  updated = credentials_file.read_bytes()
  parsed = yaml.safe_load(updated)
  assert exit_code == 0
  assert stdout == "personal: person@example.invalid\n"
  assert stderr == ""
  assert updated.startswith(original)
  assert parsed["google_account_personal"] == {
      "refresh_token": "new-refresh",
      "description": "personal: mailbox",
  }
  assert stat.S_IMODE(credentials_file.stat().st_mode) == 0o640
  assert not list(credentials_file.parent.glob(".credentials.yaml.*.tmp"))


def test_add_existing_section_replaces_only_refresh_token_line(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("\ngoogle_account_personal:\n  description: keep this\n  refresh_token: old-refresh\n")
  original_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  add_responses(monkeypatch)

  exit_code, _, _ = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=personal", "--description", "ignored description"
      ], capsys)

  updated_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  changes = [(before, after) for before, after in zip(original_lines, updated_lines, strict=True) if before != after]
  assert exit_code == 0
  assert changes == [("  refresh_token: old-refresh", "  refresh_token: new-refresh")]
  assert "  description: keep this" in updated_lines


def test_add_default_name_reauthorizes_google_section(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  contents = credentials_file.read_text(encoding="utf-8").replace("google:\n", "google:\n  name: work\n")
  credentials_file.write_text(contents, encoding="utf-8")
  original_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  add_responses(monkeypatch)

  exit_code, _, _ = run_cli(
      [
          "add", "work", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=work"
      ], capsys)

  updated_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  changes = [(before, after) for before, after in zip(original_lines, updated_lines, strict=True) if before != after]
  assert exit_code == 0
  assert changes == [("  refresh_token: default-refresh", "  refresh_token: new-refresh")]
  assert yaml.safe_load(credentials_file.read_text(encoding="utf-8"))["google"]["refresh_token"] == "new-refresh"


@pytest.mark.parametrize(
    "redirect_url, email, expected_error", [
        ("http://localhost/?code=auth-code&state=other", "person@example.invalid", "state"),
        ("http://localhost/?code=auth-code&state=personal", "wrong@example.invalid", "email mismatch"),
    ])
def test_failed_add_keeps_file_bytes_and_cleans_temporary_file(
    credentials_file: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    redirect_url: str,
    email: str,
    expected_error: str,
) -> None:
  original = credentials_file.read_bytes()
  payloads = [] if "state=other" in redirect_url else [
      {
          "access_token": "temporary-access",
          "refresh_token": "new-refresh"
      },
      {
          "emailAddress": "person@example.invalid"
      },
  ]
  if payloads:
    stub_urlopen(monkeypatch, payloads)

  exit_code, stdout, stderr = run_cli(["add", "personal", "--email", email, "--redirect-url", redirect_url], capsys)

  assert exit_code == 1
  assert stdout == ""
  assert expected_error in stderr
  assert credentials_file.read_bytes() == original
  assert not list(credentials_file.parent.glob(".credentials.yaml.*.tmp"))


def test_list_marks_pending_and_does_not_print_tokens(
    credentials_file: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("google_account_personal:\n  description: personal mailbox\n")

  exit_code, stdout, stderr = run_cli(["list"], capsys)

  assert exit_code == 0
  assert stderr == ""
  assert "default\tdefault\tready" in stdout
  assert "personal\t\tpending\tpersonal mailbox" in stdout
  assert "default-refresh" not in stdout


def test_list_check_reads_email_without_printing_tokens(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  stub_urlopen(
      monkeypatch, [
          {
              "access_token": "default-access"
          },
          {
              "emailAddress": "work@example.invalid"
          },
          {
              "access_token": "personal-access"
          },
          {
              "emailAddress": "person@example.invalid"
          },
      ])
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("google_account_personal:\n  refresh_token: personal-refresh\n")

  exit_code, stdout, stderr = run_cli(["list", "--check"], capsys)

  assert exit_code == 0
  assert stderr == ""
  assert "work@example.invalid" in stdout
  assert "person@example.invalid" in stdout
  assert "default-access" not in stdout
  assert "personal-access" not in stdout
  assert "default-refresh" not in stdout
  assert "personal-refresh" not in stdout


def test_list_check_keeps_pending_account_row_with_empty_address(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  stub_urlopen(monkeypatch, [{"access_token": "default-access"}, {"emailAddress": "work@example.invalid"}])
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("google_account_personal:\n  description: personal mailbox\n")

  exit_code, stdout, stderr = run_cli(["list", "--check"], capsys)

  assert exit_code == 0
  assert stderr == ""
  assert stdout == ("default\tdefault\tready\t\twork@example.invalid\n"
                    "personal\t\tpending\tpersonal mailbox\t\n")


def test_relative_profile_home_is_rejected(monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  monkeypatch.setenv("CHARLIEBOT_HOME", "relative/profile")

  exit_code, stdout, stderr = run_cli(["list"], capsys)

  assert exit_code == 2
  assert stdout == ""
  assert "CHARLIEBOT_HOME" in stderr

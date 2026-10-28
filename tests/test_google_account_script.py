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


def test_add_existing_section_keeps_description_without_flag(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("\ngoogle_account_personal:\n  description: keep this\n  refresh_token: old-refresh\n")
  original_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  add_responses(monkeypatch)

  exit_code, _, _ = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=personal"
      ], capsys)

  updated_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  changes = [(before, after) for before, after in zip(original_lines, updated_lines, strict=True) if before != after]
  assert exit_code == 0
  assert changes == [("  refresh_token: old-refresh", "  refresh_token: new-refresh")]
  assert "  description: keep this" in updated_lines


def test_add_pending_section_inserts_refresh_token_and_description(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  original = credentials_file.read_bytes()
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write(
        "\ngoogle_account_personal:\n"
        "  client_id: personal-client\n"
        "  client_secret: personal-secret\n"
        "\ngoogle_account_after:\n"
        "  refresh_token: after-refresh\n")
  requests: list[Any] = []
  stub_urlopen(
      monkeypatch, [
          {
              "access_token": "temporary-access",
              "refresh_token": "new-refresh"
          },
          {
              "emailAddress": "person@example.invalid"
          },
      ], requests)

  exit_code, stdout, stderr = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=personal", "--description", "personal mailbox"
      ], capsys)

  updated = credentials_file.read_bytes()
  assert exit_code == 0
  assert stdout == "personal: person@example.invalid\n"
  assert stderr == ""
  head, _, rest = updated.partition(b"google_account_personal:\n")
  section, _, tail = rest.partition(b"\ngoogle_account_after:\n")
  assert head == original + b"\n"
  assert section.splitlines() == [
      b"  client_id: personal-client",
      b"  client_secret: personal-secret",
      b"  refresh_token: new-refresh",
      b"  description: personal mailbox",
  ]
  assert tail == b"  refresh_token: after-refresh\n"
  fields = urllib.parse.parse_qs(requests[0].data.decode("utf-8"))
  assert fields["client_id"] == ["personal-client"]
  assert fields["client_secret"] == ["personal-secret"]


def test_add_existing_section_replaces_description_line(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("\ngoogle_account_personal:\n  description: old text\n  refresh_token: old-refresh\n")
  original_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  add_responses(monkeypatch)

  exit_code, _, _ = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=personal", "--description", "new: text"
      ], capsys)

  updated_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  changes = [(before, after) for before, after in zip(original_lines, updated_lines, strict=True) if before != after]
  assert exit_code == 0
  assert changes == [
      ("  description: old text", "  description: 'new: text'"),
      ("  refresh_token: old-refresh", "  refresh_token: new-refresh"),
  ]


def test_add_default_name_inserts_google_description(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  add_responses(monkeypatch)

  exit_code, _, _ = run_cli(
      [
          "add", "default", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=default", "--description", "work mailbox"
      ], capsys)

  updated_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  assert exit_code == 0
  assert updated_lines == [
      "google:",
      "  client_id: default-client",
      "  client_secret: default-secret",
      "  refresh_token: new-refresh",
      "  description: work mailbox",
  ]
  assert yaml.safe_load(credentials_file.read_text(encoding="utf-8"))["google"]["description"] == "work mailbox"


LONG_DESCRIPTION = (
    "Shared family mailbox for travel bookings, school notices, receipts, appointments and the tax paperwork of "
    "the household")


def test_add_long_description_on_pending_section_stays_on_one_line(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("\ngoogle_account_personal:\n  description: old text\n  refresh_token: old-refresh\n")
  original_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  add_responses(monkeypatch)

  exit_code, stdout, stderr = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=personal", "--description", LONG_DESCRIPTION
      ], capsys)

  updated_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  assert exit_code == 0
  assert stdout == "personal: person@example.invalid\n"
  assert stderr == ""
  changes = [(before, after) for before, after in zip(original_lines, updated_lines, strict=True) if before != after]
  assert len(updated_lines) == len(original_lines)
  assert changes == [
      ("  description: old text", f"  description: {LONG_DESCRIPTION}"),
      ("  refresh_token: old-refresh", "  refresh_token: new-refresh"),
  ]
  assert yaml.safe_load(
      credentials_file.read_text(encoding="utf-8"))["google_account_personal"]["description"] == LONG_DESCRIPTION


def test_add_long_description_on_default_name_stays_on_one_line(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  add_responses(monkeypatch)

  exit_code, _, stderr = run_cli(
      [
          "add", "default", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=default", "--description", LONG_DESCRIPTION
      ], capsys)

  updated_lines = credentials_file.read_text(encoding="utf-8").splitlines()
  assert exit_code == 0
  assert stderr == ""
  assert updated_lines == [
      "google:",
      "  client_id: default-client",
      "  client_secret: default-secret",
      "  refresh_token: new-refresh",
      f"  description: {LONG_DESCRIPTION}",
  ]
  assert yaml.safe_load(credentials_file.read_text(encoding="utf-8"))["google"]["description"] == LONG_DESCRIPTION


@pytest.mark.parametrize("inline_section", [
    "{client_id: personal-client, client_secret: personal-secret}",
    "{}",
])
def test_add_rejects_inline_section_without_changes(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
    inline_section: str) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write(f"\ngoogle_account_personal: {inline_section}\n")
  original = credentials_file.read_bytes()
  add_responses(monkeypatch)

  exit_code, stdout, stderr = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=personal", "--description", "personal mailbox"
      ], capsys)

  assert exit_code == 1
  assert stdout == ""
  assert "credentials.google_account_personal must be a block mapping" in stderr
  assert credentials_file.read_bytes() == original
  assert not list(credentials_file.parent.glob(".credentials.yaml.*.tmp"))


def test_add_duplicate_refresh_token_lines_fail_without_changes(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  with credentials_file.open("a", encoding="utf-8") as output:
    output.write("\ngoogle_account_personal:\n  refresh_token: one\n  refresh_token: two\n")
  original = credentials_file.read_bytes()
  add_responses(monkeypatch)

  exit_code, stdout, stderr = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost/?code=auth-code&state=personal"
      ], capsys)

  assert exit_code == 1
  assert stdout == ""
  assert "more than one refresh_token" in stderr
  assert credentials_file.read_bytes() == original
  assert not list(credentials_file.parent.glob(".credentials.yaml.*.tmp"))


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


def test_add_owner_writes_into_new_and_existing_sections(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  credentials_file.write_text(
      "google:\n"
      "  client_id: default-client\n"
      "  client_secret: default-secret\n"
      "  refresh_token: default-refresh\n"
      "  owner: avery\n"
      "google_account_existing:\n"
      "  description: existing\n",
      encoding="utf-8")
  stub_urlopen(
      monkeypatch, [
          {
              "access_token": "new-access",
              "refresh_token": "new-refresh"
          },
          {
              "emailAddress": "new@example.invalid"
          },
          {
              "access_token": "existing-access",
              "refresh_token": "existing-refresh"
          },
          {
              "emailAddress": "existing@example.invalid"
          },
      ])

  exit_code, _, stderr = run_cli(
      [
          "add", "fresh", "--email", "new@example.invalid", "--redirect-url", "http://localhost?state=fresh&code=c",
          "--owner", "blake"
      ], capsys)
  assert exit_code == 0, stderr
  exit_code, _, stderr = run_cli(
      [
          "add", "existing", "--email", "existing@example.invalid", "--redirect-url",
          "http://localhost?state=existing&code=c", "--owner", "carol"
      ], capsys)
  assert exit_code == 0, stderr

  data = yaml.safe_load(credentials_file.read_text(encoding="utf-8"))
  assert data["google_account_fresh"]["owner"] == "blake"
  assert data["google_account_existing"]["owner"] == "carol"
  assert data["google_account_existing"]["description"] == "existing"


def test_add_without_owner_exits_when_google_sets_owner(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  requests: list[Any] = []
  stub_urlopen(monkeypatch, [{"unexpected": "no call"}], requests)
  credentials_file.write_text(
      "google:\n"
      "  client_id: c\n"
      "  client_secret: s\n"
      "  refresh_token: r\n"
      "  owner: avery\n"
      "google_account_personal:\n"
      "  description: personal mailbox\n",
      encoding="utf-8")
  original = credentials_file.read_bytes()

  exit_code, stdout, stderr = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost?state=personal&code=c"
      ], capsys)

  assert exit_code == 2
  assert "--owner" in stderr
  assert stdout == ""
  assert requests == []
  assert credentials_file.read_bytes() == original
  assert not list(credentials_file.parent.glob(".credentials.yaml.*.tmp"))


def test_add_owner_without_google_owner_exits_before_network(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  requests: list[Any] = []
  stub_urlopen(monkeypatch, [{"unexpected": "no call"}], requests)
  original = credentials_file.read_bytes()

  exit_code, stdout, stderr = run_cli(
      [
          "add", "personal", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost?state=personal&code=c", "--owner", "blake"
      ], capsys)

  assert exit_code == 2
  assert "credentials.google_account_personal sets owner but credentials.google does not" in stderr
  assert stdout == ""
  assert requests == []
  assert credentials_file.read_bytes() == original


def test_add_leaving_person_without_primary_exits_before_network(
    credentials_file: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  requests: list[Any] = []
  stub_urlopen(monkeypatch, [{"unexpected": "no call"}], requests)
  credentials_file.write_text(
      "google:\n"
      "  client_id: c\n"
      "  client_secret: s\n"
      "  refresh_token: r\n"
      "  owner: avery\n"
      "google_account_blake-a:\n"
      "  refresh_token: ra\n"
      "  owner: blake\n",
      encoding="utf-8")
  original = credentials_file.read_bytes()

  exit_code, stdout, stderr = run_cli(
      [
          "add", "blake-b", "--email", "person@example.invalid", "--redirect-url",
          "http://localhost?state=blake-b&code=c", "--owner", "blake"
      ], capsys)

  assert exit_code == 2
  assert "person blake has several accounts and none sets primary: true" in stderr
  assert stdout == ""
  assert requests == []
  assert credentials_file.read_bytes() == original

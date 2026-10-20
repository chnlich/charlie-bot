"""Manage the Google account table in a CharlieBot profile's credentials file."""

from __future__ import annotations

import argparse
import copy
import dataclasses
import json
import os
import pathlib
import re
import stat
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Sequence
from typing import Any, cast

import yaml

_HOME_ENV = "CHARLIEBOT_HOME"
_CREDENTIALS_FILENAME = "credentials.yaml"
_GOOGLE_SECTION = "google"
_ACCOUNT_PREFIX = "google_account_"
_ACCOUNT_NAME = re.compile(r"[a-z][a-z0-9-]*")
_TOKEN_URL = "https://oauth2.googleapis.com/token"
_AUTH_URL = "https://accounts.google.com/o/oauth2/v2/auth"
_PROFILE_URL = "https://gmail.googleapis.com/gmail/v1/users/me/profile"
_SCOPES = " ".join(
    (
        "https://www.googleapis.com/auth/spreadsheets",
        "https://www.googleapis.com/auth/documents",
        "https://www.googleapis.com/auth/drive.file",
        "https://www.googleapis.com/auth/gmail.readonly",
        "https://www.googleapis.com/auth/calendar.readonly",
    ))


class CliError(Exception):
  """An error that maps to a user-facing command exit status."""

  def __init__(self, message: str, exit_code: int) -> None:
    super().__init__(message)
    self.message = message
    self.exit_code = exit_code


@dataclasses.dataclass(frozen=True)
class Account:
  name: str
  section: str
  is_default: bool
  values: dict[str, object]


@dataclasses.dataclass(frozen=True)
class AccountTable:
  path: pathlib.Path
  raw_bytes: bytes
  raw_text: str
  data: dict[str, object]
  accounts: list[Account]
  by_name: dict[str, Account]
  google: dict[str, object]


@dataclasses.dataclass(frozen=True)
class _BlockSectionEditPlan:
  replacements: dict[str, int]
  insert_line: int
  indentation: str


@dataclasses.dataclass(frozen=True)
class _FlowSectionEditPlan:
  replacements: dict[str, tuple[int, int]]
  insert_offset: int
  closing_offset: int
  has_entries: bool


def _profile_home() -> pathlib.Path:
  raw = os.environ.get(_HOME_ENV, "").strip()
  if raw and not raw.startswith(("~", "/")):
    raise CliError(f"{_HOME_ENV} must be an absolute path or start with '~'", 2)
  return pathlib.Path(raw).expanduser().resolve() if raw else pathlib.Path.home() / ".charliebot"


def _read_root_node(raw_text: str) -> yaml.Node | None:
  try:
    return yaml.compose(raw_text, Loader=yaml.SafeLoader)
  except yaml.YAMLError as error:
    raise CliError("credentials.yaml is not valid YAML", 2) from error


def _load_yaml(raw_text: str) -> dict[str, object]:
  try:
    loaded = yaml.safe_load(raw_text)
  except yaml.YAMLError as error:
    raise CliError("credentials.yaml is not valid YAML", 2) from error
  if not isinstance(loaded, dict):
    raise CliError("credentials.yaml must contain a mapping", 2)
  if any(not isinstance(key, str) for key in loaded):
    raise CliError("credentials.yaml has an invalid section name", 2)
  return cast(dict[str, object], loaded)


def _google_sections_in_node(node: yaml.Node | None) -> list[str]:
  if not isinstance(node, yaml.MappingNode):
    return []
  seen: set[str] = set()
  duplicates: list[str] = []
  for key_node, _ in node.value:
    if not isinstance(key_node, yaml.ScalarNode):
      continue
    name = key_node.value
    if name == _GOOGLE_SECTION or name.startswith(_ACCOUNT_PREFIX):
      if name in seen and name not in duplicates:
        duplicates.append(name)
      seen.add(name)
  return duplicates


def _google_values(data: dict[str, object], section_name: str) -> dict[str, object]:
  values = data.get(section_name)
  if not isinstance(values, dict):
    raise CliError(f"credentials.{section_name} must be a mapping", 2)
  if any(not isinstance(key, str) for key in values):
    raise CliError(f"credentials.{section_name} has an invalid key", 2)
  return cast(dict[str, object], values)


def _validate_client_pair(section_name: str, values: dict[str, object]) -> None:
  has_client_id = "client_id" in values
  has_client_secret = "client_secret" in values
  if has_client_id != has_client_secret:
    raise CliError(f"credentials.{section_name} must set client_id and client_secret together", 2)


def _validate_description(section_name: str, values: dict[str, object]) -> str:
  description = values.get("description", "")
  if description is None:
    return ""
  if not isinstance(description, str) or "\n" in description or "\r" in description:
    raise CliError(f"credentials.{section_name}.description must be one line", 2)
  return description


def _load_table() -> AccountTable:
  path = _profile_home() / _CREDENTIALS_FILENAME
  try:
    raw_bytes = path.read_bytes()
  except OSError as error:
    raise CliError(f"cannot read credentials file: {path}", 2) from error
  try:
    raw_text = raw_bytes.decode("utf-8")
  except UnicodeDecodeError as error:
    raise CliError("credentials.yaml must use UTF-8", 2) from error

  node = _read_root_node(raw_text)
  duplicates = _google_sections_in_node(node)
  if duplicates:
    raise CliError(f"credentials.{duplicates[0]} appears more than once", 2)
  data = _load_yaml(raw_text)
  if _GOOGLE_SECTION not in data:
    raise CliError("credentials.google is missing", 2)
  google = _google_values(data, _GOOGLE_SECTION)
  _validate_client_pair(_GOOGLE_SECTION, google)
  _validate_description(_GOOGLE_SECTION, google)

  default_name = google.get("name", "default")
  if not isinstance(default_name, str) or not _ACCOUNT_NAME.fullmatch(default_name):
    raise CliError("credentials.google.name must match [a-z][a-z0-9-]*", 2)

  accounts = [Account(default_name, _GOOGLE_SECTION, is_default=True, values=google)]
  for section_name in data:
    if not section_name.startswith(_ACCOUNT_PREFIX):
      continue
    name = section_name.removeprefix(_ACCOUNT_PREFIX)
    if not _ACCOUNT_NAME.fullmatch(name):
      raise CliError(f"credentials.{section_name} has an invalid account name", 2)
    values = _google_values(data, section_name)
    _validate_client_pair(section_name, values)
    _validate_description(section_name, values)
    refresh_token = values.get("refresh_token")
    if refresh_token is not None and not isinstance(refresh_token, str):
      raise CliError(f"credentials.{section_name}.refresh_token must be a string", 2)
    accounts.append(Account(name, section_name, is_default=False, values=values))

  by_name: dict[str, Account] = {}
  for account in accounts:
    if account.name in by_name:
      raise CliError(f"duplicate Google account name: {account.name}", 2)
    by_name[account.name] = account
  return AccountTable(path, raw_bytes, raw_text, data, accounts, by_name, google)


def _account_for(table: AccountTable, name: str | None) -> Account:
  if name is None:
    name = next(account.name for account in table.accounts if account.is_default)
  account = table.by_name.get(name)
  if account is None:
    names = ", ".join(account.name for account in table.accounts)
    raise CliError(f"unknown account. Available accounts: {names}", 2)
  return account


def _client_for(table: AccountTable, account: Account | None) -> tuple[str, str]:
  values = account.values if account is not None else table.google
  if "client_id" not in values:
    values = table.google
  client_id = values.get("client_id")
  client_secret = values.get("client_secret")
  if not isinstance(client_id, str) or not client_id.strip() or not isinstance(client_secret,
                                                                               str) or not client_secret.strip():
    account_name = account.name if account is not None else "new account"
    raise CliError(f"OAuth client is not configured for {account_name}", 1)
  return client_id, client_secret


def _post_form(fields: dict[str, str], operation: str) -> dict[str, Any]:
  request = urllib.request.Request(
      _TOKEN_URL,
      data=urllib.parse.urlencode(fields).encode("utf-8"),
      headers={"Content-Type": "application/x-www-form-urlencoded"},
      method="POST")
  return _open_json(request, operation)


def _open_json(request: urllib.request.Request, operation: str) -> dict[str, Any]:
  try:
    with urllib.request.urlopen(request, timeout=30) as response:
      body = response.read()
  except urllib.error.HTTPError as error:
    error.close()
    raise CliError(f"{operation} failed: HTTP {error.code}", 1) from None
  except (urllib.error.URLError, OSError):
    raise CliError(f"{operation} failed: network error", 1) from None
  try:
    payload = json.loads(body)
  except (json.JSONDecodeError, UnicodeDecodeError):
    raise CliError(f"{operation} returned invalid JSON", 1) from None
  if not isinstance(payload, dict):
    raise CliError(f"{operation} returned an invalid response", 1)
  if "error" in payload:
    raise CliError(f"{operation} failed: Google returned an error", 1)
  return cast(dict[str, Any], payload)


def _required_response_string(response: dict[str, Any], key: str, operation: str) -> str:
  value = response.get(key)
  if not isinstance(value, str) or not value.strip():
    raise CliError(f"{operation} did not return {key}", 1)
  return value


def _mint_access_token(table: AccountTable, account: Account) -> str:
  refresh_token = account.values.get("refresh_token")
  if not isinstance(refresh_token, str) or not refresh_token.strip():
    raise CliError(f"account {account.name} is pending authorization", 1)
  client_id, client_secret = _client_for(table, account)
  response = _post_form(
      {
          "client_id": client_id,
          "client_secret": client_secret,
          "refresh_token": refresh_token,
          "grant_type": "refresh_token",
      }, "Google token refresh")
  return _required_response_string(response, "access_token", "Google token refresh")


def _profile_email(access_token: str) -> str:
  request = urllib.request.Request(_PROFILE_URL, headers={"Authorization": f"Bearer {access_token}"})
  response = _open_json(request, "Gmail profile lookup")
  return _required_response_string(response, "emailAddress", "Gmail profile lookup")


def _consent_url(table: AccountTable, name: str, email: str | None) -> str:
  if not _ACCOUNT_NAME.fullmatch(name):
    raise CliError("account name must match [a-z][a-z0-9-]*", 2)
  if email is not None and not email.strip():
    raise CliError("email address must not be empty", 2)
  account = table.by_name.get(name)
  client_id, _ = _client_for(table, account)
  parameters = {
      "response_type": "code",
      "client_id": client_id,
      "redirect_uri": "http://localhost",
      "scope": _SCOPES,
      "access_type": "offline",
      "prompt": "consent select_account",
      "state": name,
  }
  if email is not None:
    parameters["login_hint"] = email.strip()
  return f"{_AUTH_URL}?{urllib.parse.urlencode(parameters)}"


def _redirect_code(redirect_url: str, name: str) -> str:
  try:
    query = urllib.parse.parse_qs(urllib.parse.urlsplit(redirect_url).query, keep_blank_values=True)
  except ValueError:
    raise CliError("redirect URL is invalid", 1) from None
  states = query.get("state", [])
  if len(states) != 1 or states[0] != name:
    raise CliError("redirect state does not match the account name", 1)
  codes = query.get("code", [])
  if len(codes) != 1 or not codes[0].strip():
    raise CliError("redirect URL does not contain an authorization code", 1)
  return codes[0]


def _section_edit_plan(
    raw_text: str, section_name: str, keys: Sequence[str]) -> _BlockSectionEditPlan | _FlowSectionEditPlan:
  """Locate each key's line in one section, plus the insert point and indentation."""
  try:
    node = yaml.compose(raw_text, Loader=yaml.SafeLoader)
  except yaml.YAMLError as error:
    raise CliError("credentials.yaml is not valid YAML", 1) from error
  if not isinstance(node, yaml.MappingNode):
    raise CliError("credentials.yaml must contain a mapping", 1)
  section_pairs = [
      (key_node, value_node)
      for key_node, value_node in node.value
      if isinstance(key_node, yaml.ScalarNode) and key_node.value == section_name
  ]
  if len(section_pairs) != 1 or not isinstance(section_pairs[0][1], yaml.MappingNode):
    raise CliError(f"credentials.{section_name} cannot be updated", 1)
  section_key, section_node = section_pairs[0]
  pairs = section_node.value
  replacement_nodes: dict[str, yaml.Node] = {}
  for key_node, value_node in pairs:
    if isinstance(key_node, yaml.ScalarNode) and key_node.value in keys:
      if key_node.value in replacement_nodes:
        raise CliError(f"credentials.{section_name} contains more than one {key_node.value} line", 1)
      replacement_nodes[key_node.value] = value_node
  if section_node.flow_style:
    replacements = {
        key: (value_node.start_mark.index, value_node.end_mark.index) for key, value_node in replacement_nodes.items()
    }
    closing_offset = section_node.end_mark.index - 1
    insert_offset = pairs[-1][1].end_mark.index if pairs else closing_offset
    return _FlowSectionEditPlan(replacements, insert_offset, closing_offset, bool(pairs))
  replacements = {key: value_node.start_mark.line for key, value_node in replacement_nodes.items()}
  if not pairs:
    indentation = " " * (section_key.start_mark.column + 2)
    return _BlockSectionEditPlan(replacements, section_key.start_mark.line + 1, indentation)
  first_line = pairs[0][0].start_mark.line
  first_text = raw_text.splitlines(keepends=True)[first_line]
  indentation = first_text[:len(first_text) - len(first_text.lstrip(" \t"))]
  last_value = pairs[-1][1]
  if isinstance(last_value, (yaml.MappingNode, yaml.SequenceNode)):
    block_offset = 1 if last_value.flow_style else 0
  else:
    block_offset = 0 if last_value.style in ("|", ">") else 1
  return _BlockSectionEditPlan(replacements, last_value.end_mark.line + block_offset, indentation)


def _flow_entry(key: str, value: str) -> str:
  rendered = yaml.safe_dump(
      {
          key: value
      }, sort_keys=False, allow_unicode=True, default_flow_style=True, width=1_000_000).strip()
  return rendered[1:-1]


def _updated_flow_section(raw_text: str, plan: _FlowSectionEditPlan, values: dict[str, str]) -> str:
  edits: list[tuple[int, int, str]] = []
  for key, (start, end) in plan.replacements.items():
    _, separator, rendered_value = _flow_entry(key, values[key]).partition(":")
    if not separator:
      raise AssertionError("flow entry did not contain a key separator")
    edits.append((start, end, rendered_value.lstrip()))

  inserted = [key for key in values if key not in plan.replacements]
  if inserted:
    entries = ", ".join(_flow_entry(key, values[key]) for key in inserted)
    if not plan.has_entries:
      insertion_offset = plan.closing_offset
      insertion = entries
    else:
      suffix = raw_text[plan.insert_offset:plan.closing_offset]
      whitespace = len(suffix) - len(suffix.lstrip())
      if whitespace < len(suffix) and suffix[whitespace] == ",":
        insertion_offset = plan.insert_offset + whitespace + 1
        insertion = f" {entries}"
      else:
        insertion_offset = plan.insert_offset
        insertion = f", {entries}"
    edits.append((insertion_offset, insertion_offset, insertion))

  for start, end, replacement in sorted(edits, key=lambda edit: edit[0], reverse=True):
    raw_text = raw_text[:start] + replacement + raw_text[end:]
  return raw_text


def _parse_for_write(raw_bytes: bytes) -> dict[str, object]:
  try:
    loaded = yaml.safe_load(raw_bytes.decode("utf-8"))
  except (UnicodeDecodeError, yaml.YAMLError) as error:
    raise CliError("temporary credentials file is not valid YAML", 1) from error
  if not isinstance(loaded, dict):
    raise CliError("temporary credentials file must contain a mapping", 1)
  return cast(dict[str, object], loaded)


def _updated_content(
    table: AccountTable,
    account: Account,
    refresh_token: str,
    description: str | None,
) -> tuple[bytes, dict[str, object]]:
  write_values: dict[str, str] = {"refresh_token": refresh_token}
  if description is not None:
    write_values["description"] = description
  assert all(value.strip() for value in write_values.values())

  expected = copy.deepcopy(table.data)
  expected_values = dict(write_values)
  if account.section in table.data:
    cast(dict[str, object], expected[account.section]).update(write_values)
    edit_plan = _section_edit_plan(table.raw_text, account.section, tuple(write_values))
    if isinstance(edit_plan, _FlowSectionEditPlan):
      updated_text = _updated_flow_section(table.raw_text, edit_plan, write_values)
      return updated_text.encode("utf-8"), expected
    lines = table.raw_text.splitlines(keepends=True)
    for key, line_number in edit_plan.replacements.items():
      current_line = lines[line_number]
      line_ending = "\r\n" if current_line.endswith("\r\n") else "\n" if current_line.endswith("\n") else ""
      replacement = yaml.safe_dump({key: write_values[key]}, sort_keys=False).splitlines()[0]
      lines[line_number] = f"{edit_plan.indentation}{replacement}{line_ending}"
    inserted = [key for key in write_values if key not in edit_plan.replacements]
    if inserted:
      following = lines[edit_plan.insert_line] if edit_plan.insert_line < len(lines) else lines[-1]
      line_ending = "\r\n" if following.endswith("\r\n") else "\n"
      if edit_plan.insert_line >= len(lines) and not lines[-1].endswith(("\n", "\r")):
        lines[-1] += "\n"
      for offset, key in enumerate(inserted):
        entry = yaml.safe_dump({key: write_values[key]}, sort_keys=False).splitlines()[0]
        lines.insert(edit_plan.insert_line + offset, f"{edit_plan.indentation}{entry}{line_ending}")
    return "".join(lines).encode("utf-8"), expected

  expected[account.section] = expected_values
  appended = yaml.safe_dump({account.section: expected_values}, sort_keys=False, allow_unicode=True).encode("utf-8")
  separator = b"" if not table.raw_bytes or table.raw_bytes.endswith((b"\n", b"\r")) else b"\n"
  return table.raw_bytes + separator + appended, expected


def _write_refresh_token(table: AccountTable, account: Account, refresh_token: str, description: str | None) -> None:
  new_bytes, expected = _updated_content(table, account, refresh_token, description)
  try:
    original_mode = stat.S_IMODE(table.path.stat().st_mode)
  except OSError:
    raise CliError("cannot read credentials file permissions", 1) from None
  try:
    file_descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{_CREDENTIALS_FILENAME}.", suffix=".tmp", dir=table.path.parent)
  except OSError:
    raise CliError("cannot create temporary credentials file", 1) from None
  temporary_path = pathlib.Path(temporary_name)
  try:
    os.fchmod(file_descriptor, original_mode)
    stream = os.fdopen(file_descriptor, "wb")
    file_descriptor = -1
    with stream:
      stream.write(new_bytes)
    parsed_temporary = _parse_for_write(temporary_path.read_bytes())
    if parsed_temporary != expected:
      raise CliError("temporary credentials file did not match the expected change", 1)
    os.replace(temporary_path, table.path)
  except OSError:
    raise CliError("credentials file update failed", 1) from None
  finally:
    try:
      if file_descriptor >= 0:
        os.close(file_descriptor)
    finally:
      if temporary_path.exists():
        try:
          temporary_path.unlink()
        except OSError:
          raise CliError("could not remove temporary credentials file", 1) from None


def _add_account(table: AccountTable, name: str, email: str, redirect_url: str, description: str | None) -> str:
  if not _ACCOUNT_NAME.fullmatch(name):
    raise CliError("account name must match [a-z][a-z0-9-]*", 2)
  if not email.strip():
    raise CliError("email address must not be empty", 2)
  if description is not None and (not description.strip() or "\n" in description or "\r" in description):
    raise CliError("description must be one line", 2)
  account = table.by_name.get(name)
  if account is None:
    account = Account(name, f"{_ACCOUNT_PREFIX}{name}", is_default=False, values={})
  code = _redirect_code(redirect_url, name)
  client_id, client_secret = _client_for(table, account)
  response = _post_form(
      {
          "client_id": client_id,
          "client_secret": client_secret,
          "code": code,
          "grant_type": "authorization_code",
          "redirect_uri": "http://localhost",
      }, "Google authorization-code exchange")
  refresh_token = _required_response_string(response, "refresh_token", "Google authorization-code exchange")
  access_token = _required_response_string(response, "access_token", "Google authorization-code exchange")
  actual_email = _profile_email(access_token)
  if actual_email.casefold() != email.strip().casefold():
    raise CliError(f"email mismatch: Google returned {actual_email}", 1)
  _write_refresh_token(table, account, refresh_token, description)
  return actual_email


def _parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(prog="google_account.py")
  subparsers = parser.add_subparsers(dest="command", required=True)
  token_parser = subparsers.add_parser("token")
  token_parser.add_argument("--account")
  list_parser = subparsers.add_parser("list")
  list_parser.add_argument("--check", action="store_true")
  consent_parser = subparsers.add_parser("consent-url")
  consent_parser.add_argument("name")
  consent_parser.add_argument("--email")
  add_parser = subparsers.add_parser("add")
  add_parser.add_argument("name")
  add_parser.add_argument("--email", required=True)
  add_parser.add_argument("--redirect-url", required=True)
  add_parser.add_argument("--description")
  return parser


def _is_pending(account: Account) -> bool:
  refresh_token = account.values.get("refresh_token")
  return not isinstance(refresh_token, str) or not refresh_token.strip()


def _run_list(table: AccountTable, check: bool) -> int:
  checked_emails: dict[str, str] = {}
  if check:
    for account in table.accounts:
      if _is_pending(account):
        continue
      try:
        checked_emails[account.name] = _profile_email(_mint_access_token(table, account))
      except CliError as error:
        raise CliError(f"account {account.name}: {error.message}", 1) from error
  for account in table.accounts:
    pending = _is_pending(account)
    fields = [account.name, "default" if account.is_default else "", "pending" if pending else "ready"]
    fields.append(_validate_description(account.section, account.values))
    if check:
      fields.append("" if pending else checked_emails[account.name])
    print("\t".join(fields))
  return 0


def main(argv: Sequence[str] | None = None) -> int:
  """Run one Google account table command and return its process exit status."""
  args = _parser().parse_args(argv)
  try:
    table = _load_table()
    if args.command == "token":
      account = _account_for(table, args.account)
      print(_mint_access_token(table, account))
      return 0
    if args.command == "list":
      return _run_list(table, args.check)
    if args.command == "consent-url":
      print(_consent_url(table, args.name, args.email))
      return 0
    if args.command == "add":
      email = _add_account(table, args.name, args.email, args.redirect_url, args.description)
      print(f"{args.name}: {email}")
      return 0
    raise AssertionError(f"unhandled command: {args.command}")
  except CliError as error:
    print(error.message, file=sys.stderr)
    return error.exit_code


if __name__ == "__main__":
  sys.exit(main())

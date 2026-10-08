"""The metadata slot registry (src/infra/metadata_slots.py): packages own top-level keys of the metadata files.

The keys never move on disk: a file the base writer produced loads and saves byte-identically, a key no
owner registers survives load and save, and a create request carries only registered keys.
"""

import json
import pathlib
import threading
import time

import conftest
import pytest
from pydantic import BaseModel, ValidationError

from src.app import registrations
from src.infra import metadata_slot_registration, metadata_slots, models
from src.runtime import sessions, task_sessions
from src.runtime.session_store import TRANSIENT_METADATA_FIELDS, SessionStore

DATA = pathlib.Path(__file__).parent / "data"


class ProbeSessionFields(BaseModel):
  probe_note: str | None = None
  probe_count: int = 0


class ProbeThreadFields(BaseModel):
  probe_tag: str | None = None


class FieldOfTheMetadataModel(BaseModel):
  group: str | None = None


class FieldOfAnotherOwner(BaseModel):
  probe_note: str | None = None


class LateSessionFields(BaseModel):
  late_note: str | None = None


def _probe(name: str) -> str:
  return f"{__name__}:{name}"


def _resolve_pending() -> None:
  """Any ``metadata_slots`` call resolves every pending registration; this one reads nothing else."""
  metadata_slots.check_registered(metadata_slots.ON_SESSION, {})


@pytest.fixture
def probe_slots(monkeypatch: pytest.MonkeyPatch) -> None:
  """A private registry holding the production owners plus a probe owner on both files."""
  monkeypatch.setattr(metadata_slot_registration, "_registered", list(metadata_slot_registration.registered()))
  monkeypatch.setattr(metadata_slots, "_slots", {on: dict(owners) for on, owners in metadata_slots._slots.items()})
  monkeypatch.setattr(metadata_slots, "_resolved", metadata_slots._resolved)  # the count of registrations _slots covers
  metadata_slot_registration.register_metadata_fields("probe", _probe("ProbeSessionFields"), after="group")
  metadata_slot_registration.register_metadata_fields("probe", _probe("ProbeThreadFields"), on="thread")


@pytest.mark.usefixtures("probe_slots")
class TestRegistration:

  def test_a_field_named_like_a_field_of_the_metadata_model_raises_at_the_first_resolution(self) -> None:
    metadata_slot_registration.register_metadata_fields("greedy", _probe("FieldOfTheMetadataModel"))
    with pytest.raises(ValueError, match="collides with a field of SessionMetadata"):
      _resolve_pending()

  def test_a_field_named_like_another_owners_field_raises_at_the_first_resolution(self) -> None:
    metadata_slot_registration.register_metadata_fields("copycat", _probe("FieldOfAnotherOwner"))
    with pytest.raises(ValueError, match="collides with a field of 'probe'"):
      _resolve_pending()

  def test_a_second_registration_of_one_owner_raises_at_registration(self) -> None:
    with pytest.raises(ValueError, match="already registered"):
      metadata_slot_registration.register_metadata_fields("probe", _probe("ProbeSessionFields"))

  def test_the_after_field_must_be_declared_by_the_metadata_model_at_the_first_resolution(self) -> None:
    metadata_slot_registration.register_metadata_fields("lost", _probe("FieldOfAnotherOwner"), after="no_such_field")
    with pytest.raises(ValueError, match="not a declared field"):
      _resolve_pending()

  @pytest.mark.parametrize(
      ("model", "on", "message"), [
          ("not-a-module-class-string", "session", "not a 'module:Class' string"),
          ("tests.test_metadata_slots:ProbeSessionFields", "elsewhere", "on must be one of"),
      ])
  def test_a_malformed_model_string_or_an_unknown_file_raises_at_registration(
      self, model: str, on: str, message: str) -> None:
    with pytest.raises(ValueError, match=message):
      metadata_slot_registration.register_metadata_fields("bad", model, on=on)

  def test_a_registration_that_cannot_resolve_raises_on_every_later_call(self) -> None:
    metadata_slot_registration.register_metadata_fields("greedy", _probe("FieldOfTheMetadataModel"))
    for _ in range(2):
      with pytest.raises(ValueError, match="collides with a field of SessionMetadata"):
        _resolve_pending()


@pytest.mark.usefixtures("probe_slots")
def test_a_registration_made_after_a_resolution_resolves_on_the_next_call() -> None:
  _resolve_pending()
  metadata_slot_registration.register_metadata_fields("late", _probe("LateSessionFields"))

  metadata_slots.check_registered(metadata_slots.ON_SESSION, {"late_note": "kept"})


@pytest.mark.usefixtures("probe_slots")
def test_concurrent_first_calls_resolve_each_registration_once(monkeypatch: pytest.MonkeyPatch) -> None:
  pending = len(metadata_slot_registration.registered()) - metadata_slots._resolved
  resolved: list[str] = []
  resolve = metadata_slots._resolve

  def slow_resolve(registration: metadata_slot_registration.Registration) -> object:
    resolved.append(registration.owner)
    time.sleep(0.05)  # holds the first caller inside the resolution while the others arrive
    return resolve(registration)

  monkeypatch.setattr(metadata_slots, "_resolve", slow_resolve)
  start = threading.Barrier(4)

  def first_call() -> None:
    start.wait()
    _resolve_pending()

  threads = [threading.Thread(target=first_call) for _ in range(4)]
  for thread in threads:
    thread.start()
  for thread in threads:
    thread.join()

  assert len(resolved) == pending


@pytest.mark.usefixtures("probe_slots")
def test_fields_of_and_set_fields_validate_types() -> None:
  meta = models.SessionMetadata(profile="manager", name="s")
  assert metadata_slots.fields_of(meta, "probe") == ProbeSessionFields()

  metadata_slots.set_fields(meta, "probe", probe_note="hello", probe_count=3)
  assert metadata_slots.fields_of(meta, "probe") == ProbeSessionFields(probe_note="hello", probe_count=3)

  with pytest.raises(ValidationError):
    metadata_slots.set_fields(meta, "probe", probe_count="many")
  with pytest.raises(ValueError, match="no fields"):
    metadata_slots.set_fields(meta, "probe", not_a_field=1)
  assert metadata_slots.fields_of(meta, "probe").probe_count == 3, "a refused write changes nothing"

  # A stored value of the wrong type still loads; reading it through the owner's view is what fails.
  loaded = models.SessionMetadata.model_validate_json('{"name": "s", "profile": "manager", "probe_count": "many"}')
  with pytest.raises(ValidationError):
    metadata_slots.fields_of(loaded, "probe")

  thread = models.ThreadMetadata(session_id="s", description="d")
  metadata_slots.set_fields(thread, "probe", probe_tag="t")
  assert metadata_slots.fields_of(thread, "probe") == ProbeThreadFields(probe_tag="t")


@pytest.mark.usefixtures("probe_slots")
def test_an_unregistered_key_survives_load_and_save() -> None:
  raw = {"id": "a", "name": "n", "profile": "manager", "left_by_a_deleted_package": {"nested": [1, 2]}}
  meta = models.SessionMetadata.model_validate_json(json.dumps(raw))
  thread = models.ThreadMetadata.model_validate_json(
      json.dumps({
          "session_id": "s",
          "description": "d",
          "left_by_a_deleted_package": 7
      }))

  assert json.loads(meta.model_dump_json())["left_by_a_deleted_package"] == {"nested": [1, 2]}
  assert json.loads(thread.model_dump_json())["left_by_a_deleted_package"] == 7


@pytest.mark.usefixtures("probe_slots")
def test_registered_keys_save_after_their_declared_neighbour_with_defaults_filled() -> None:
  meta = models.SessionMetadata(profile="manager", name="s")
  metadata_slots.set_fields(meta, "probe", probe_note="hello")
  meta.stray = "kept"  # a key no owner registers goes last
  keys = list(json.loads(meta.model_dump_json()))

  assert keys[keys.index("group") + 1:keys.index("group") + 3] == ["probe_note", "probe_count"]
  assert keys[-1] == "stray"
  assert json.loads(meta.model_dump_json())["probe_count"] == 0


def test_task_node_metadata_saves_byte_identically() -> None:
  """The manager's Claude account label stays in its registered slot when written back."""
  written = (DATA / "session_metadata_v2_manager.json").read_text()

  saved = models.SessionMetadata.model_validate_json(written).model_dump_json(
      indent=2, exclude=TRANSIENT_METADATA_FIELDS)

  assert saved == written


@pytest.fixture
def create_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, probe_slots: None):
  cfg = conftest.build_two_backend_cfg(tmp_path)
  session_mgr = sessions.SessionManager(cfg, SessionStore(cfg))
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  conftest.bind_deps_managers(monkeypatch, tree, session_mgr)
  return cfg, session_mgr


@pytest.mark.asyncio
async def test_a_create_request_sets_registered_fields_and_refuses_unknown_keys(create_env) -> None:
  cfg, session_mgr = create_env

  with conftest.make_sessions_client(cfg, session_mgr) as client:
    created = client.post("/api/sessions/", json={"name": "probed", "probe_note": "hello", "probe_count": 2})
    unknown = client.post("/api/sessions/", json={"name": "stray", "no_such_key": 1})
    mistyped = client.post("/api/sessions/", json={"name": "mistyped", "probe_count": "many"})

  assert created.status_code == 200
  assert created.json()["probe_note"] == "hello"
  stored = await session_mgr.store.read_metadata_fresh(created.json()["id"])
  assert metadata_slots.fields_of(stored, "probe") == ProbeSessionFields(probe_note="hello", probe_count=2)
  assert unknown.status_code == 422
  assert mistyped.status_code == 422

  legacy = await conftest.create_root_session(
      session_mgr, models.CreateSessionRequest(name="legacy", probe_note="direct"))
  assert metadata_slots.fields_of(legacy, "probe").probe_note == "direct"


def test_every_package_registration_resolves() -> None:
  """A collision or an undeclared ``after`` in any real package fails here, naming the package's field."""
  registrations.register_all()

  _resolve_pending()

  registered = {(registration.on, registration.owner) for registration in metadata_slot_registration.registered()}
  assert registered == {(on, owner) for on, owners in metadata_slots._slots.items() for owner in owners}

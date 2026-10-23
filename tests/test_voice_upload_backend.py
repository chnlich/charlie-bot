"""The upload endpoint's transcription: the recording persists first, then the selected online backend transcribes it.

Each test drives upload_voice_recording directly (the devices tests' pattern) against a fake live backend
registered in the transcription registry and a temporary sessions dir. The contract under test: the recording
is on disk before any backend runs; the backend receives it in worklet-sized chunks and its final text is the
response; a backend failure is an error that leaves the WAV for the browser's retry; an unknown or unavailable
selection is a 400 that also keeps the WAV; an absent field selects voice.default_backend.
"""

import json
import pathlib
import types
from collections.abc import AsyncIterator, Sequence

import conftest
import fastapi
import httpx
import pytest
from structlog import testing

from src.features.voice import api as voice
from src.features.voice.transcription import base
from src.features.voice.transcription import registry as transcription_registry
from src.infra import config

SESSION_ID = "session-a"
BACKEND_ID = "fake-live"
FINAL_TEXT = "synthetic final words"
# One chunk and a 10-sample tail: the chunking contract in one recording.
SAMPLES = base.VOICE_CHUNK_SAMPLES + 10


class _Fake:
  """The fake backend's switches and the calls it received."""

  def __init__(self) -> None:
    self.calls: list[dict] = []
    self.unavailable: str | None = None
    self.fail = False

  def build(self, cfg: config.CharlieBotConfig, **_: object) -> base.TranscriptionBackend:
    return _FakeBackend(self, cfg)


class _FakeBackend(base.TranscriptionBackend):

  id = BACKEND_ID
  label = "Fake live backend"
  live_partials = True

  def __init__(self, owner: _Fake, cfg: config.CharlieBotConfig) -> None:
    self._owner = owner
    self._cfg = cfg

  def unavailable_reason(self) -> str | None:
    return self._owner.unavailable

  async def transcribe(
      self,
      audio: AsyncIterator[bytes],
      *,
      vocabulary: Sequence[str],
      languages: Sequence[str],
  ) -> AsyncIterator[base.TranscriptEvent]:
    chunks = [chunk async for chunk in audio]
    self._owner.calls.append(
        {
            "chunks": chunks,
            "vocabulary": list(vocabulary),
            "languages": list(languages),
            # The WAVs on disk at transcribe time: the persist-before-transcribe proof.
            "wavs_on_disk": sorted(path.name for path in self._cfg.sessions_dir.rglob("*.wav")),
        })
    if self._owner.fail:
      raise RuntimeError("fake transcription failure")
    yield base.TranscriptEvent(kind="final", text=FINAL_TEXT)


@pytest.fixture
def cfg(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> config.CharlieBotConfig:
  cfg = config.CharlieBotConfig(
      charliebot_home=tmp_path / "home", voice={
          "vocabulary": ["CharlieBot"],
          "languages": ["zh"]
      })
  monkeypatch.setattr(voice, "config", types.SimpleNamespace(get_config=lambda: cfg))
  return cfg


@pytest.fixture
def fake(cfg: config.CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> _Fake:
  owner = _Fake()
  monkeypatch.setitem(transcription_registry._FACTORIES, BACKEND_ID, owner.build)
  return owner


async def _post(backend: str | None, samples: int = 160) -> tuple[object, list[dict]]:
  with testing.capture_logs() as logs:
    response = await voice.upload_voice_recording(
        SESSION_ID, audio=conftest.voice_upload_file(conftest.wav_body(samples)), backend=backend, devices=None)
  return response, logs


def _voice_dir(cfg: config.CharlieBotConfig) -> pathlib.Path:
  return cfg.sessions_dir / SESSION_ID / "voice"


@pytest.mark.asyncio
async def test_a_live_backend_id_returns_its_final_text_after_the_recording_is_on_disk(
    cfg: config.CharlieBotConfig, fake: _Fake) -> None:
  response, logs = await _post(BACKEND_ID, samples=SAMPLES)

  assert response.body == f'{{"text":"{FINAL_TEXT}"}}'.encode()
  (call,) = fake.calls
  assert [len(chunk) for chunk in call["chunks"]] == [base.VOICE_CHUNK_SAMPLES * 2, 20]
  assert b"".join(call["chunks"]) == b"\x00\x00" * SAMPLES
  assert call["vocabulary"] == ["CharlieBot"]  # the config's hints ride along
  assert call["languages"] == ["zh"]
  # The recording was persisted before the backend ran, and the transcript now sits next to it.
  assert len(call["wavs_on_disk"]) == 1
  assert sorted(path.name for path in _voice_dir(cfg).glob("*.wav")) == call["wavs_on_disk"]
  assert (_voice_dir(cfg) / call["wavs_on_disk"][0]).with_suffix(".txt").read_text(encoding="utf-8") == FINAL_TEXT
  transcribed = [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert len(transcribed) == 1
  assert transcribed[0]["backend"] == BACKEND_ID  # the producer of the text
  assert transcribed[0]["selected_backend"] == BACKEND_ID


@pytest.mark.asyncio
async def test_a_transcription_failure_returns_the_error_and_leaves_the_wav_on_disk(
    cfg: config.CharlieBotConfig, fake: _Fake) -> None:
  fake.fail = True

  response, logs = await _post(BACKEND_ID)

  assert response.status_code == 500
  error = json.loads(response.body)["error"]
  assert error.startswith("voice transcription failed")
  assert "fake transcription failure" in error
  # The recording survived the failure; no text was published.
  assert len(list(_voice_dir(cfg).glob("*.wav"))) == 1
  assert not list(_voice_dir(cfg).glob("*.txt"))
  assert not [entry for entry in logs if entry["event"] == "voice_transcribed"]


@pytest.mark.asyncio
async def test_an_unavailable_selection_is_a_400_that_keeps_the_recording(
    cfg: config.CharlieBotConfig, fake: _Fake) -> None:
  fake.unavailable = "needs fake.api_key"

  response, logs = await _post(BACKEND_ID)

  assert response.status_code == 400
  assert json.loads(response.body)["error"] == f"{BACKEND_ID} is unavailable: needs fake.api_key"
  assert fake.calls == []
  assert len(list(_voice_dir(cfg).glob("*.wav"))) == 1
  assert not [entry for entry in logs if entry["event"] == "voice_transcribed"]


@pytest.mark.asyncio
async def test_an_unknown_backend_id_is_a_400_that_keeps_the_recording(
    cfg: config.CharlieBotConfig, fake: _Fake) -> None:
  response, _ = await _post("does-not-exist")

  assert response.status_code == 400
  assert "does-not-exist" in json.loads(response.body)["error"]
  assert fake.calls == []
  assert len(list(_voice_dir(cfg).glob("*.wav"))) == 1


@pytest.mark.asyncio
async def test_an_absent_backend_field_selects_the_default_backend(cfg: config.CharlieBotConfig, fake: _Fake) -> None:
  cfg.voice.default_backend = BACKEND_ID

  response, logs = await _post(None)

  assert response.body == f'{{"text":"{FINAL_TEXT}"}}'.encode()
  assert len(fake.calls) == 1
  transcribed = [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert transcribed[0]["backend"] == BACKEND_ID
  assert transcribed[0]["selected_backend"] is None


@pytest.mark.asyncio
async def test_the_confirm_probe_endpoint_no_longer_exists(cfg: config.CharlieBotConfig) -> None:
  app = fastapi.FastAPI()
  app.include_router(voice.router, prefix="/api/voice")

  async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://test") as client:
    response = await client.post(f"/api/voice/{SESSION_ID}/confirm", content=conftest.wav_body())

  assert response.status_code == 404
  assert not _voice_dir(cfg).exists()

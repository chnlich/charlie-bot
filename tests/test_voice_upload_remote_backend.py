"""The upload endpoint's cloud dispatch: the selected non-live backend decodes the upload.

Each test drives upload_voice_recording directly (the devices tests' pattern)
against a fake non-live backend registered in the transcription registry and a
temporary sessions dir. The contract under test: the selected backend receives
the whole PCM only after the recording is on disk, a failure leaves the WAV
there for the browser's retry, an unavailable selection is an error naming the
reason and never a local fallback, and every other backend value - absent,
unknown, a live backend, the local id - keeps the local decode.
"""

import io
import json
import wave
from collections.abc import AsyncIterator, Sequence
from pathlib import Path

import pytest
from fastapi import UploadFile
from structlog.testing import capture_logs

from src.agents.transcription import registry as transcription_registry
from src.agents.transcription.base import TranscriptEvent, TranscriptionBackend
from src.api import voice
from src.core.config import CharlieBotConfig

SESSION_ID = "session-a"
BACKEND_ID = "fake-remote"
REMOTE_TEXT = "synthetic remote words"
LOCAL_TEXT = "synthetic decoded words"


def _wav_body(samples: int = 160) -> bytes:
  buffer = io.BytesIO()
  with wave.open(buffer, "wb") as writer:
    writer.setnchannels(1)
    writer.setsampwidth(2)
    writer.setframerate(16_000)
    writer.writeframes(b"\x00\x00" * samples)
  return buffer.getvalue()


def _upload_file(body: bytes) -> UploadFile:
  return UploadFile(file=io.BytesIO(body), filename="recording.wav")


async def _pcm_frames(audio: AsyncIterator[bytes]) -> bytes:
  pcm = bytearray()
  async for chunk in audio:
    pcm.extend(chunk)
  return bytes(pcm)


class _FakeRemote:
  """The fake non-live backend's calls, plus the failure the test scripts."""

  def __init__(self) -> None:
    self.calls: list[dict] = []
    self.unavailable: str | None = None
    self.fail = False

  def build(self, cfg: CharlieBotConfig, **_: object) -> TranscriptionBackend:
    return _FakeRemoteBackend(self, cfg)


class _FakeRemoteBackend(TranscriptionBackend):

  id = BACKEND_ID
  label = "Fake remote backend"
  live_partials = False

  def __init__(self, owner: _FakeRemote, cfg: CharlieBotConfig) -> None:
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
  ) -> AsyncIterator[TranscriptEvent]:
    self._owner.calls.append(
        {
            "pcm": await _pcm_frames(audio),
            "vocabulary": list(vocabulary),
            "languages": list(languages),
            # The WAVs on disk at decode time: the persist-before-decode proof.
            "wavs_on_disk": sorted(path.name for path in self._cfg.sessions_dir.rglob("*.wav")),
        })
    if self._owner.fail:
      raise RuntimeError("fake transcription failure")
    yield TranscriptEvent(kind="final", text=REMOTE_TEXT)


async def _forbidden_bundle() -> object:
  raise AssertionError("the cloud path must not acquire the local speech bundle")


async def _local_bundle() -> object:
  return object()


async def _local_decode(session_id: str, bundle: object, pcm_bytes: bytes) -> str:
  return LOCAL_TEXT


@pytest.fixture
def cfg(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> CharlieBotConfig:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", voice={"vocabulary": ["CharlieBot"], "languages": ["zh"]})
  monkeypatch.setattr(voice, "get_config", lambda: cfg)
  return cfg


@pytest.fixture
def remote(cfg: CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> _FakeRemote:
  owner = _FakeRemote()
  monkeypatch.setitem(transcription_registry._FACTORIES, BACKEND_ID, owner.build)
  return owner


async def _post(backend: str | None) -> tuple[object, list[dict]]:
  with capture_logs() as logs:
    response = await voice.upload_voice_recording(
        SESSION_ID, audio=_upload_file(_wav_body()), backend=backend, devices=None)
  return response, logs


def _voice_dir(cfg: CharlieBotConfig) -> Path:
  return cfg.sessions_dir / SESSION_ID / "voice"


# --- The cloud path ------------------------------------------------------------


@pytest.mark.asyncio
async def test_the_selected_backend_decodes_after_the_recording_is_on_disk(
    cfg: CharlieBotConfig, remote: _FakeRemote, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(voice, "_speech_bundle", _forbidden_bundle)

  response, logs = await _post(BACKEND_ID)

  assert response.body == f'{{"text":"{REMOTE_TEXT}"}}'.encode()
  (call,) = remote.calls
  assert call["pcm"] == b"\x00\x00" * 160  # the whole upload's PCM, one chunk
  assert call["vocabulary"] == ["CharlieBot"]  # the config's hints ride along
  assert call["languages"] == ["zh"]
  # The recording was persisted before the backend ran, and the transcript now
  # sits next to it.
  assert len(call["wavs_on_disk"]) == 1
  assert sorted(path.name for path in _voice_dir(cfg).glob("*.wav")) == call["wavs_on_disk"]
  assert (_voice_dir(cfg) / call["wavs_on_disk"][0]).with_suffix(".txt").read_text(encoding="utf-8") == REMOTE_TEXT
  transcribed = [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert len(transcribed) == 1
  assert transcribed[0]["backend"] == BACKEND_ID  # the producer of the text
  assert transcribed[0]["selected_backend"] == BACKEND_ID


@pytest.mark.asyncio
async def test_a_transcription_failure_leaves_the_wav_on_disk(cfg: CharlieBotConfig, remote: _FakeRemote) -> None:
  remote.fail = True

  response, logs = await _post(BACKEND_ID)

  assert response.status_code == 500
  assert "voice transcription failed" in json.loads(response.body)["error"]
  assert "fake transcription failure" in json.loads(response.body)["error"]
  # The recording survived the failure; no text was published.
  assert len(list(_voice_dir(cfg).glob("*.wav"))) == 1
  assert not list(_voice_dir(cfg).glob("*.txt"))
  assert not [entry for entry in logs if entry["event"] == "voice_transcribed"]


@pytest.mark.asyncio
async def test_an_unavailable_selection_is_an_error_without_a_local_fallback(
    cfg: CharlieBotConfig, remote: _FakeRemote) -> None:
  remote.unavailable = "needs aigw.api_key"

  response, logs = await _post(BACKEND_ID)

  assert response.status_code == 400
  assert json.loads(response.body)["error"] == f"{BACKEND_ID} is unavailable: needs aigw.api_key"
  # Nothing decoded, nothing persisted.
  assert remote.calls == []
  assert not _voice_dir(cfg).exists()
  assert not [entry for entry in logs if entry["event"] == "voice_transcribed"]


# --- The local path keeps its cases ---------------------------------------------


@pytest.mark.asyncio
@pytest.mark.parametrize("backend", ["gemini", "does-not-exist", "local", None])
async def test_live_unknown_local_and_absent_ids_keep_the_local_path(
    cfg: CharlieBotConfig, remote: _FakeRemote, monkeypatch: pytest.MonkeyPatch, backend: str | None) -> None:
  monkeypatch.setattr(voice, "_speech_bundle", _local_bundle)
  monkeypatch.setattr(voice, "_transcribe_with_bundle", _local_decode)

  response, logs = await _post(backend)

  assert response.body == f'{{"text":"{LOCAL_TEXT}"}}'.encode()
  assert remote.calls == []
  transcribed = [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert transcribed[0]["backend"] == "local"
  assert transcribed[0]["selected_backend"] == backend

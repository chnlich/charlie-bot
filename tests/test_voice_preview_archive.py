"""The preview relay's archive: it keeps the audio it forwarded and publishes the pair first.

Each test drives src/api/voice.py's relay handler against a fake live backend
and a temporary sessions dir; the fake websocket scripts the browser's frames
and records the server's. The contract under test: the recording+text pair is
on disk before the final frame goes out, a failed or unwanted final leaves no
files behind, and the kept audio never exceeds the transcriber's sample cap.
"""

import asyncio
import json
import wave
from collections.abc import AsyncIterator
from pathlib import Path

import pytest
from conftest import _async_wait_for
from structlog.testing import capture_logs

from src.agents.transcription import registry as transcription_registry
from src.agents.transcription.base import TranscriptEvent, TranscriptionBackend
from src.api import voice
from src.core.config import CharlieBotConfig

SESSION_ID = "session-a"
BACKEND_ID = "fake-live"
FINAL_TEXT = "synthetic final words"
FRAME_A = bytes(range(256)) * 8  # 2048 bytes = 1024 PCM16 samples
FRAME_B = b"\x07\x00" * 2048  # 4096 bytes = 2048 PCM16 samples
_END_FRAME = {"type": "end"}
# The end frame's device object: the five keys voice_transcribed logs.
DEVICES = {
    "input_device": "Test Microphone",
    "capture_settings":
        {
            "echoCancellation": True,
            "noiseSuppression": True,
            "autoGainControl": True,
            "sampleRate": 16000
        },
    "output_device": "Test Speakers",
    "communications_output_device": None,
    "device_error": None,
}
_END_FRAME_WITH_DEVICES = {"type": "end", "devices": DEVICES}
_DISCONNECT = {"type": "websocket.disconnect"}
# Parks the browser's next receive until the test releases it, so the test
# decides which side of a server-side race the handler sees first.
_HOLD = object()


def _voice_dir(tmp_path: Path) -> Path:
  return tmp_path / "home" / "sessions" / SESSION_ID / "voice"


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(charliebot_home=tmp_path / "home")


def _read_wav_pcm(path: Path) -> bytes:
  with wave.open(str(path), "rb") as reader:
    assert (reader.getnchannels(), reader.getsampwidth(), reader.getframerate()) == (1, 2, 16_000)
    return reader.readframes(reader.getnframes())


class FakePreviewSocket:
  """ASGI-shaped websocket double: scripted receives, recorded JSON sends."""

  def __init__(self, messages: list[object], *, hang_when_script_ends: bool = False) -> None:
    self._messages = list(messages)
    self._hang_when_script_ends = hang_when_script_ends
    self.release = asyncio.Event()
    self.sent: list[dict] = []
    self.send_hook: object = None  # set by a test that must observe a send moment
    self.closed = False

  async def accept(self) -> None:
    return None

  async def receive(self) -> dict:
    while True:
      if not self._messages:
        if self._hang_when_script_ends:
          await asyncio.Event().wait()  # the browser stays connected, silent
        return dict(_DISCONNECT)
      message = self._messages.pop(0)
      if message is _HOLD:
        await self.release.wait()
        continue
      # ASGI's receive shape: binary rides ``bytes``, a control frame rides
      # ``text`` as its JSON string, and only the disconnect is its own type.
      if isinstance(message, bytes):
        return {"type": "websocket.receive", "bytes": message}
      assert isinstance(message, dict)
      if message.get("type") == "websocket.disconnect":
        return dict(message)
      return {"type": "websocket.receive", "text": json.dumps(message)}

  async def send_json(self, payload: dict) -> None:
    if self.send_hook is not None:
      self.send_hook(payload)
    self.sent.append(payload)

  async def close(self) -> None:
    self.closed = True


class FakeLiveBackend(TranscriptionBackend):
  """A live backend whose final the test scripts: gated after the audio, or early mid-audio."""

  id = BACKEND_ID
  label = "Fake live backend"
  live_partials = True

  def __init__(
      self,
      final_text: str,
      *,
      partial_text: str | None = None,
      gate: asyncio.Event | None = None,
      early_after_chunks: int | None = None,
  ) -> None:
    self.final_text = final_text
    self._partial_text = partial_text
    self._gate = gate
    self._early_after_chunks = early_after_chunks
    self.closed = False

  async def transcribe(
      self,
      audio: AsyncIterator[bytes],
      *,
      vocabulary: list[str],
      languages: list[str],
  ) -> AsyncIterator[TranscriptEvent]:
    try:
      seen = 0
      async for _chunk in audio:
        seen += 1
        if self._early_after_chunks is not None and seen >= self._early_after_chunks:
          yield TranscriptEvent(kind="final", text=self.final_text)
          return
      if self._gate is not None:
        await self._gate.wait()
      if self._partial_text is not None:
        yield TranscriptEvent(kind="partial", text=self._partial_text)
      yield TranscriptEvent(kind="final", text=self.final_text)
    finally:
      self.closed = True  # the streamer's close of this iterator ends the backend session


def _start_relay(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    socket: FakePreviewSocket,
    backend: FakeLiveBackend,
) -> asyncio.Task:
  """Run the relay handler as a task against the fake socket, backend, and sessions dir."""
  monkeypatch.setattr(voice, "get_config", lambda: _build_cfg(tmp_path))
  # The registry is the relay's only backend seam; setitem puts the fake behind
  # the id and monkeypatch removes it again after the test.
  monkeypatch.setitem(transcription_registry._FACTORIES, BACKEND_ID, lambda _cfg: backend)
  return asyncio.create_task(voice.voice_preview_relay(socket, SESSION_ID, BACKEND_ID))


@pytest.mark.asyncio
async def test_final_after_end_archives_the_pair_then_pushes_the_final(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  socket = FakePreviewSocket([FRAME_A, FRAME_B, _END_FRAME], hang_when_script_ends=True)
  backend = FakeLiveBackend(FINAL_TEXT, partial_text="synthetic partial")
  dirs_at_final: list[list[str]] = []

  def hook(payload: dict) -> None:
    if payload.get("type") == "final":
      dirs_at_final.append(sorted(path.name for path in _voice_dir(tmp_path).glob("*")))

  socket.send_hook = hook
  relay = _start_relay(monkeypatch, tmp_path, socket, backend)
  with capture_logs() as logs:
    await asyncio.wait_for(relay, timeout=2)

  assert [frame["type"] for frame in socket.sent] == ["partial", "final"]
  assert backend.closed  # a finished relay leaves the backend session closed too
  wavs = list(_voice_dir(tmp_path).glob("*.wav"))
  assert len(wavs) == 1
  assert _read_wav_pcm(wavs[0]) == FRAME_A + FRAME_B
  assert wavs[0].with_suffix(".txt").read_text(encoding="utf-8") == FINAL_TEXT
  # The pair was already on disk in the moment the final frame went out.
  expected_pair = sorted([wavs[0].name, wavs[0].with_suffix(".txt").name])
  assert dirs_at_final == [expected_pair]

  transcribed = [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert len(transcribed) == 1
  assert transcribed[0]["session_id"] == SESSION_ID
  assert transcribed[0]["audio_path"] == str(wavs[0])
  assert transcribed[0]["audio_bytes_size"] == len(FRAME_A) + len(FRAME_B)
  assert transcribed[0]["transcription_length"] == len(FINAL_TEXT)
  assert transcribed[0]["transcription_preview"] == FINAL_TEXT
  assert transcribed[0]["backend"] == BACKEND_ID
  assert transcribed[0]["selected_backend"] == BACKEND_ID
  # A bare end frame is the pre-devices page still open across a restart: all
  # five device fields log as None.
  assert transcribed[0]["input_device"] is None
  assert transcribed[0]["capture_settings"] is None
  assert transcribed[0]["output_device"] is None
  assert transcribed[0]["communications_output_device"] is None
  assert transcribed[0]["device_error"] is None


@pytest.mark.asyncio
async def test_an_end_frame_with_devices_logs_the_five_device_fields(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  socket = FakePreviewSocket([FRAME_A, _END_FRAME_WITH_DEVICES], hang_when_script_ends=True)
  backend = FakeLiveBackend(FINAL_TEXT)
  relay = _start_relay(monkeypatch, tmp_path, socket, backend)
  with capture_logs() as logs:
    await asyncio.wait_for(relay, timeout=2)

  transcribed = [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert len(transcribed) == 1
  assert transcribed[0]["input_device"] == "Test Microphone"
  assert transcribed[0]["capture_settings"] == DEVICES["capture_settings"]
  assert transcribed[0]["output_device"] == "Test Speakers"
  assert transcribed[0]["communications_output_device"] is None
  assert transcribed[0]["device_error"] is None
  # The device fields joined the log line; the existing fields are unchanged.
  assert transcribed[0]["session_id"] == SESSION_ID
  assert transcribed[0]["audio_bytes_size"] == len(FRAME_A)
  assert transcribed[0]["backend"] == BACKEND_ID
  assert transcribed[0]["selected_backend"] == BACKEND_ID


@pytest.mark.asyncio
async def test_an_end_frame_with_a_missing_device_key_takes_the_protocol_error_path(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  short = {key: value for key, value in DEVICES.items() if key != "output_device"}
  await _assert_devices_protocol_error(monkeypatch, tmp_path, short)


@pytest.mark.asyncio
async def test_an_end_frame_with_an_extra_device_key_takes_the_protocol_error_path(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  extra = {**DEVICES, "microphone_label": "one key too many"}
  await _assert_devices_protocol_error(monkeypatch, tmp_path, extra)


async def _assert_devices_protocol_error(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    devices: dict,
) -> None:
  """A devices object without exactly the five keys ends the preview as the
  protocol error it is: one error frame, no archive, no log line."""
  socket = FakePreviewSocket([FRAME_A, {"type": "end", "devices": devices}])
  backend = FakeLiveBackend(FINAL_TEXT)
  relay = _start_relay(monkeypatch, tmp_path, socket, backend)
  with capture_logs() as logs:
    await asyncio.wait_for(relay, timeout=2)

  assert [frame["type"] for frame in socket.sent] == ["error"]
  assert "devices" in socket.sent[0]["message"]
  assert not [entry for entry in logs if entry["event"] == "voice_transcribed"]
  assert list(_voice_dir(tmp_path).glob("*")) == []


@pytest.mark.asyncio
async def test_an_archive_failure_pushes_an_error_and_leaves_no_files(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  socket = FakePreviewSocket([FRAME_A, _END_FRAME], hang_when_script_ends=True)
  backend = FakeLiveBackend(FINAL_TEXT)

  def failing_write(path: Path, pcm_bytes: bytes) -> None:
    raise OSError("synthetic write failure")

  monkeypatch.setattr(voice, "_write_wav", failing_write)
  relay = _start_relay(monkeypatch, tmp_path, socket, backend)
  with capture_logs() as logs:
    await asyncio.wait_for(relay, timeout=2)

  assert [frame["type"] for frame in socket.sent] == ["error"]
  assert socket.sent[0]["message"] == "recording archive failed"
  failed = [entry for entry in logs if entry["event"] == "voice_preview_archive_failed"]
  assert len(failed) == 1
  assert not [entry for entry in logs if entry["event"] == "voice_transcribed"]
  # No .wav, no .txt, no temporary file survived the failure.
  assert list(_voice_dir(tmp_path).glob("*")) == []


@pytest.mark.asyncio
async def test_a_final_before_the_end_frame_is_a_failure_with_no_archive(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  # The browser is parked mid-recording when the backend's final lands, so the
  # queue is unsealed and the final is early.
  socket = FakePreviewSocket([FRAME_A, _HOLD, _END_FRAME])
  backend = FakeLiveBackend(FINAL_TEXT, early_after_chunks=1)
  relay = _start_relay(monkeypatch, tmp_path, socket, backend)
  await _async_wait_for(lambda: socket.sent, 1.0, "the preview relay never sent its first frame")

  assert [frame["type"] for frame in socket.sent] == ["error"]
  assert "end frame" in socket.sent[0]["message"]

  socket.release.set()
  await asyncio.wait_for(relay, timeout=2)

  assert backend.closed  # the failure closed the backend iterator
  assert list(_voice_dir(tmp_path).glob("*")) == []


@pytest.mark.asyncio
async def test_a_disconnect_after_the_end_frame_cancels_the_backend_and_archives_nothing(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  gate = asyncio.Event()  # never set: the backend never reaches its final
  socket = FakePreviewSocket([FRAME_A, _END_FRAME])  # the script then disconnects
  backend = FakeLiveBackend(FINAL_TEXT, gate=gate)
  relay = _start_relay(monkeypatch, tmp_path, socket, backend)
  await asyncio.wait_for(relay, timeout=2)

  assert socket.sent == []
  assert backend.closed
  assert list(_voice_dir(tmp_path).glob("*")) == []


@pytest.mark.asyncio
async def test_frames_past_the_cap_are_cut_sample_exactly(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  from src.agents import transcriber

  # Three 2048-sample frames against a 3072-sample cap: the second frame is cut
  # in half (2048 of its samples kept), the third does not survive at all.
  monkeypatch.setattr(transcriber, "MAX_RECORDING_SAMPLES", 3072)
  socket = FakePreviewSocket([FRAME_B, FRAME_B, FRAME_B, _END_FRAME], hang_when_script_ends=True)
  backend = FakeLiveBackend(FINAL_TEXT)
  relay = _start_relay(monkeypatch, tmp_path, socket, backend)
  await asyncio.wait_for(relay, timeout=2)

  wavs = list(_voice_dir(tmp_path).glob("*.wav"))
  assert len(wavs) == 1
  assert _read_wav_pcm(wavs[0]) == FRAME_B + FRAME_B[:2048]

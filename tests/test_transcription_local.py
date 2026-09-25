"""Local transcription backend tests: stub-bundle parity with the upload endpoint's decode.

The style follows tests/test_voice_engine.py: a stub bundle and monkeypatched
transcriber seams, no models, no network. The parity claim under test: the
backend's final text is transcribe_pcm_offline's output for the exact bytes the
chunk stream carried, decoded on the bundle the upload endpoint uses.
"""

from __future__ import annotations

import asyncio
import threading
from collections.abc import AsyncIterator

import pytest
from conftest import stub_speech_bundle

from src.agents import transcriber
from src.agents.transcription.local import LocalTranscriptionBackend
from src.core.config import CharlieBotConfig


async def _collect(backend: LocalTranscriptionBackend, chunks: list[bytes], **kwargs) -> list:

  async def feed() -> AsyncIterator[bytes]:
    for chunk in chunks:
      yield chunk

  return [event async for event in backend.transcribe(feed(), **kwargs)]


def test_transcribe_drains_audio_and_decodes_on_the_resident_bundle(monkeypatch: pytest.MonkeyPatch) -> None:
  """The chunk stream becomes one transcribe_pcm_offline call on get_transcription_bundle's
  bundle — the upload endpoint's exact decode shape; exactly one final comes out."""
  cfg = CharlieBotConfig()
  bundle = stub_speech_bundle("sherpa", "test")
  acquired: list[CharlieBotConfig] = []
  decoded: list[tuple[object, bytes]] = []

  def fake_get_bundle(received_cfg: CharlieBotConfig) -> object:
    acquired.append(received_cfg)
    return bundle

  def fake_offline(received_bundle: object, pcm_bytes: bytes) -> str:
    decoded.append((received_bundle, pcm_bytes))
    return "hello world"

  monkeypatch.setattr(transcriber, "get_transcription_bundle", fake_get_bundle)
  monkeypatch.setattr(transcriber, "transcribe_pcm_offline", fake_offline)

  events = asyncio.run(
      _collect(LocalTranscriptionBackend(cfg), [b"abcd", b"efgh"], vocabulary=["CharlieBot"], languages=["zh"]))

  assert acquired == [cfg]
  assert [(event.kind, event.text) for event in events] == [("final", "hello world")]
  # The decode saw the resident bundle and the chunk stream's exact concatenation:
  # the same bytes the upload endpoint hands transcribe_pcm_offline today.
  assert decoded == [(bundle, b"abcdefgh")]


def test_transcribe_runs_offline_decode_off_the_event_loop(monkeypatch: pytest.MonkeyPatch) -> None:
  """The decode is a worker-thread call, so a paced caller is never blocked by it."""
  cfg = CharlieBotConfig()
  monkeypatch.setattr(transcriber, "get_transcription_bundle", lambda _cfg: stub_speech_bundle("sherpa", "test"))

  def fake_offline(_bundle: object, _pcm: bytes) -> str:
    assert threading.current_thread() is not threading.main_thread()
    return "threaded"

  monkeypatch.setattr(transcriber, "transcribe_pcm_offline", fake_offline)

  events = asyncio.run(_collect(LocalTranscriptionBackend(cfg), [b"abcd"], vocabulary=[], languages=[]))
  assert [event.text for event in events] == ["threaded"]


def test_hotwords_build_a_dedicated_sherpa_bundle_once(monkeypatch: pytest.MonkeyPatch) -> None:
  """The hotwords variant never touches the resident bundle: it builds, warms, and reuses
  its own sherpa bundle carrying the hotwords string."""
  cfg = CharlieBotConfig()
  paths = transcriber.voice_model_paths(cfg)
  built: list[str] = []
  warmed: list[object] = []
  resident_probes: list[CharlieBotConfig] = []

  decoded: list[object] = []

  def fake_create(received_paths: object, hotwords: str = "") -> object:
    built.append(hotwords)
    return stub_speech_bundle("sherpa", "test")

  def fake_offline(received_bundle: object, _pcm: bytes) -> str:
    decoded.append(received_bundle)
    return "hotword text"

  def fail_resident(cfg: CharlieBotConfig) -> object:
    resident_probes.append(cfg)
    raise AssertionError("the resident bundle must not be used by the hotwords variant")

  monkeypatch.setattr(transcriber, "_ensure_sherpa_paths_cached", lambda _cfg: paths)
  monkeypatch.setattr(transcriber, "create_sherpa_bundle", fake_create)
  monkeypatch.setattr(transcriber, "warm_up_bundle", warmed.append)
  monkeypatch.setattr(transcriber, "transcribe_pcm_offline", fake_offline)
  monkeypatch.setattr(transcriber, "get_transcription_bundle", fail_resident)

  backend = LocalTranscriptionBackend(cfg, hotwords="CharlieBot")
  for _ in range(2):
    events = asyncio.run(_collect(backend, [b"abcd"], vocabulary=[], languages=[]))
    assert [event.text for event in events] == ["hotword text"]

  assert built == ["CharlieBot"]
  assert len(warmed) == 1
  # Both decodes ran on the one dedicated bundle instance.
  assert decoded[0] is decoded[1]
  assert resident_probes == []


def test_label_names_the_configured_engine() -> None:
  assert LocalTranscriptionBackend(CharlieBotConfig()).label == "Local (sherpa)"
  gpu_cfg = CharlieBotConfig(voice={"engine": "qwen3_hf"})
  assert LocalTranscriptionBackend(gpu_cfg).label == "Local (qwen3_hf)"

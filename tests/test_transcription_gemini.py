# -*- coding: utf-8 -*-
"""The Gemini Live backend: one transcript normalizer behind both yield sites.

The Live API streams interims character-split with spaces, and finals
occasionally arrive in that same form, there with ASCII punctuation around the
marks. These tests pin each normalization rule and drive transcribe against a
fake websocket (connect and the credential lookup patched) to prove both event
kinds come out normalized. Every sentence here is synthetic.
"""

import json
import pathlib
from collections.abc import AsyncIterator
from typing import Self

import pytest

from src.features.voice.transcription import gemini
from src.infra import config, credentials

SETUP_REPLY = {"setupComplete": {}}
SPACED_INTERIM = "请 查 一 下 任 务"
SPACED_FINAL = "任 务 完 成 了 , 还 要 继 续 ?"
NORMALIZED_INTERIM = "请查一下任务"
NORMALIZED_FINAL = "任务完成了，还要继续？"

# --- The normalization rules, one test per rule ------------------------------


def test_spaces_between_two_cjk_characters_go() -> None:
  assert gemini._normalize_transcript("请 帮 我 查 一 下") == "请帮我查一下"


def test_spaces_before_a_clause_mark_go_when_the_mark_ends_the_clause() -> None:
  # The mark is followed by the text's end; it stays ASCII because the text
  # before it is Latin.
  assert gemini._normalize_transcript("等 一 下 ok ?") == "等一下 ok?"
  # A run of spaces before the mark goes with it.
  assert gemini._normalize_transcript("好 了  ?") == "好了？"
  # A mark followed by more non-ASCII text counts as clause-ending too; here
  # the comma ends up directly after CJK text, so rule 4 converts it as well.
  assert gemini._normalize_transcript("第 一 , 下 一 个") == "第一，下一个"
  # A fullwidth mark with a surviving space (the mark preceded by Latin) loses it.
  assert gemini._normalize_transcript("ok ， 好") == "ok，好"


def test_a_path_keeps_its_space_because_the_dot_is_not_clause_ending() -> None:
  assert gemini._normalize_transcript("看 一 下 cd ./dir") == "看一下 cd ./dir"


def test_one_space_between_an_ascii_mark_and_non_ascii_text_goes() -> None:
  assert gemini._normalize_transcript("ok , 然后") == "ok,然后"
  # The same for the dot, which has no fullwidth form and so stays ASCII.
  assert gemini._normalize_transcript("第 一 行 . 结 束") == "第一行.结束"
  # A space after a mark before Latin text stays.
  assert gemini._normalize_transcript("ok, jabra") == "ok, jabra"


def test_an_ascii_mark_directly_after_cjk_text_becomes_fullwidth() -> None:
  assert gemini._normalize_transcript("好 了 吗 ?") == "好了吗？"
  assert gemini._normalize_transcript("用 冒 号 : 分 隔") == "用冒号：分隔"
  # A mark beside Latin text stays ASCII.
  assert gemini._normalize_transcript("值 是 1 : 2") == "值是 1: 2"
  # The dot has no fullwidth form: it stays ASCII even beside CJK text.
  assert gemini._normalize_transcript("下 一 行 .") == "下一行."


def test_a_letter_spaced_sentence_with_ascii_punctuation_becomes_the_normal_form() -> None:
  assert gemini._normalize_transcript(SPACED_FINAL) == NORMALIZED_FINAL


def test_spaces_between_latin_and_cjk_text_stay() -> None:
  assert gemini._normalize_transcript("cron job 的 session") == "cron job 的 session"


def test_already_normal_text_is_returned_unchanged() -> None:
  for text in (
      "cron job 的 session",
      "今天不错，明天再见！",
      "看一下 cd ./dir",
      "ok, jabra",
      "跑 3.5 公里",
      "第一行.结束",
      "",
  ):
    assert gemini._normalize_transcript(text) == text


# --- Both yield sites normalize ----------------------------------------------


class _FakeSocket:
  """The websocket transcribe talks to: a scripted setup reply and server frames."""

  def __init__(self, frames: list[dict]) -> None:
    self._frames = list(frames)
    self.sent: list[dict] = []
    self.closed = False

  async def __aenter__(self) -> Self:
    return self

  async def __aexit__(self, *exc_info: object) -> bool:
    self.closed = True
    return False

  async def send(self, raw: str) -> None:
    self.sent.append(json.loads(raw))

  async def recv(self) -> str:
    return json.dumps(self._frames.pop(0))

  def __aiter__(self) -> Self:
    return self

  async def __anext__(self) -> str:
    if not self._frames:
      raise StopAsyncIteration
    return json.dumps(self._frames.pop(0))


async def _one_chunk_audio() -> AsyncIterator[bytes]:
  yield b"\x00\x00" * 160


@pytest.mark.asyncio
async def test_transcribe_normalizes_both_the_spaced_interim_and_the_spaced_final(
    monkeypatch: pytest.MonkeyPatch,) -> None:
  socket = _FakeSocket(
      [
          SETUP_REPLY,
          {
              "serverContent": {
                  "interimInputTranscription": {
                      "text": SPACED_INTERIM
                  }
              }
          },
          {
              "serverContent": {
                  "inputTranscription": {
                      "text": SPACED_FINAL
                  }
              }
          },
      ])

  def fake_connect(url: str, **kwargs: object) -> _FakeSocket:
    return socket

  monkeypatch.setattr("websockets.asyncio.client.connect", fake_connect)
  monkeypatch.setattr(
      "src.infra.credentials.get_credentials",
      lambda: credentials.Credentials(
          path=pathlib.Path("/tmp/fake-credentials.yaml"), sections={"gemini": {
              "api_key": "test-key"
          }}),
  )

  backend = gemini.GeminiTranscriptionBackend(config.CharlieBotConfig(charliebot_home=pathlib.Path("/tmp/fake-home")))
  events = [event async for event in backend.transcribe(_one_chunk_audio(), vocabulary=[], languages=["zh"])]

  assert [(event.kind, event.text) for event in events] == [
      ("partial", NORMALIZED_INTERIM),
      ("final", NORMALIZED_FINAL),
  ]
  assert socket.closed  # the backend session closed with the transcription

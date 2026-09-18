---
name: voice-notes
description: Handle incoming voice and audio messages such as Slack voice notes. A model with native audio input hears the clip directly; everything else decodes on-box, so the reply works from the spoken content.
---

# Voice Message Handling

Read audio messages from their spoken content. A Slack voice note arrives as
an empty-text message carrying one hosted `audio/m4a` file. Route in this
order:

## 1. Native audio input (preferred)

A conversation model that declares audio input hears the clip directly:
wording and prosody reach the reasoning turn in one request. Backend lineups
change, so take the deployment's declared modality over memory.

## 2. On-box decode

[scripts/decode_audio.py](scripts/decode_audio.py) turns any audio file into
the transcript on stdout: sherpa-onnx Qwen3-ASR 0.6B int8 + Silero VAD on
CPU, models cached under `<charliebot_home>/models` with pinned sha256 (GPU
hosts select `voice.engine=qwen3_hf`). Run
`uv run --with av python3 <skill-dir>/scripts/decode_audio.py <file>`;
8.8 s of speech decodes in ~2.7 s on the reference CPU.

## 3. Outside the checkout

A host without the repo decodes with `uvx --with faster-whisper python3`
(`small`, CPU). On Chinese clips the on-box Qwen3-ASR reads English
loanwords better ("Skill" versus "Scale") and stays the reference decode
where the checkout is present.

## Slack specifics

Fetch the hosted file through `url_private_download` with the app bearer;
the on-box decode returns in seconds while Slack's `files.info`
transcription lags by minutes. Voice content is PII: redact before quoting
outside the thread.

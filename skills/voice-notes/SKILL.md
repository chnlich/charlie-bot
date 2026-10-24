---
name: voice-notes
description: Handle Slack voice notes and other audio messages: hear the clip natively when the model can, otherwise transcribe it through the online transcription backend.
---

# Voice Message Handling

Read audio messages from their spoken content. A Slack voice note arrives as
an empty-text message carrying one hosted `audio/m4a` file. Route in this
order:

## 1. Native audio input (preferred)

A conversation model that declares audio input hears the clip directly:
wording and prosody reach the reasoning turn in one request. Backend lineups
change, so take the deployment's declared modality over memory.

## 2. Online transcription

Run this command from the repo root to transcribe one file:

    uv run --with av python3 <skill-dir>/scripts/decode_audio.py <file>

Purpose: uv runs the script in the checkout's environment, and `--with av` adds
the reader for any audio container.

The script sends the whole clip through the online transcription backend that
the voice upload uses. It uses `voice.default_backend` from the profile's config.
The backend's key comes from `credentials.yaml`. The transcript goes to stdout.

When the command fails or the checkout is missing, report that to the user and do not
guess the words.

Purpose: a guessed transcript answers words the user never said.

## Slack specifics

When Slack hosts the file, fetch it through `url_private_download` with the app bearer.
Purpose: the script needs the audio file to transcribe it.

Use the online transcription result.
Purpose: Slack's `files.info` transcript can take minutes.

Before you quote voice content outside the thread, redact it.
Purpose: voice content is PII.

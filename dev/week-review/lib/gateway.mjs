// The internal LLM gateway: its OpenAI-compatible model list and chat completions. The team service key is read from the
// named entry of credentials.yaml and is never printed. Each call builds its own request; no client object is shared.

import fs from 'node:fs';

const REQUEST_TIMEOUT_MS = 300000;

function unquote(value) {
  return value.replace(/^(["'])(.*)\1$/, '$2');
}

// The `api_key` under a top-level entry, from a credentials file of nested `name:` / `  api_key:` lines.
export function readCredential(credentialsPath, name) {
  const lines = fs.readFileSync(credentialsPath, 'utf8').split('\n');
  const header = lines.findIndex((line) => line.trimEnd() === `${name}:`);
  if (header < 0) throw new Error(`credentials file has no entry ${name}`);
  for (let index = header + 1; index < lines.length; index += 1) {
    const line = lines[index];
    if (line.trim() === '') continue;
    if (!/^\s/.test(line)) break;
    const match = /^\s+api_key:\s*(.*?)\s*$/.exec(line);
    if (match && match[1] !== '') return unquote(match[1]);
  }
  throw new Error(`credentials entry ${name} has no api_key`);
}

export async function listModels({ base, key }) {
  const response = await fetch(`${base}/v1/models`, {
    headers: { Authorization: `Bearer ${key}` },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  if (!response.ok) throw new Error(`GET ${base}/v1/models answered HTTP ${response.status}`);
  const body = await response.json();
  return body.data.map((model) => model.id);
}

// One chat completion with the judge input as the user message. Returns the answer text.
export async function chatCompletion({ base, key, model, input, where }) {
  const response = await fetch(`${base}/v1/chat/completions`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ model, messages: [{ role: 'user', content: input }] }),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  if (!response.ok) {
    const detail = (await response.text()).slice(0, 300);
    throw new Error(`${where}: gateway answered HTTP ${response.status}: ${detail}`);
  }
  const body = await response.json();
  const content = body.choices?.[0]?.message?.content;
  if (typeof content !== 'string') throw new Error(`${where}: gateway answer has no message content`);
  return content;
}

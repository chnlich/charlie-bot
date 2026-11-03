// The judge: one model call per packet, the recovered judging instructions in front of the packet, and a mechanical
// check of the answer's shape. A judgment that does not parse or does not answer the packet's messages fails the run.

import { spawn } from 'node:child_process';
import os from 'node:os';

export const VERDICTS = ['needed', 'mixed', 'unrelated'];
export const KINDS = ['other_line_status', 'other_line_decision', 'background', 'duplicate', 'other'];

export const SEPARATOR = '\n\n---\n\n';
const CLAUDE_TIMEOUT_MS = 600000;
const CLAUDE_TOOLS_OFF = 'Bash Edit Write Read Glob Grep WebFetch WebSearch Task';

// The text the model reads: the instructions, then the packet. `ids` names the messages to judge when the packet holds
// more than one answer's worth; a retry judges only the messages the first answer skipped.
export function judgeInput({ prompt, packetText, ids = null }) {
  let head = prompt;
  if (ids !== null) {
    head += `\n\nJudge only these PARENT_MESSAGE ids, and output one object for each of them:\n${ids.map((id) => `- ${id}`).join('\n')}`;
  }
  return `${head}${SEPARATOR}${packetText}`;
}

// The answer array from a model's result text. The last fenced ```json block wins, since a model may correct an
// earlier block; without one, the array runs from the first `[` to the last `]`.
export function parseAnswers(resultText, where) {
  const blocks = [...resultText.matchAll(/```json\n([\s\S]*?)```/g)];
  let body;
  if (blocks.length > 0) {
    body = blocks[blocks.length - 1][1];
  } else {
    const open = resultText.indexOf('[');
    const close = resultText.lastIndexOf(']');
    if (open < 0 || close < open) throw new Error(`${where}: the result holds no JSON array`);
    body = resultText.slice(open, close + 1);
  }
  try {
    return JSON.parse(body);
  } catch (error) {
    if (!(error instanceof SyntaxError)) throw error;
    throw new Error(`${where}: the answer is not JSON: ${error.message}`);
  }
}

// The answers for the expected ids, and the ids of the answers outside them. A model that mistypes an id leaves its
// message unanswered, so the caller judges that message again and records the mistyped id.
export function splitAnswers(answers, expectedIds) {
  const expected = new Set(expectedIds);
  const kept = answers.filter((answer) => expected.has(answer?.id));
  const dropped = answers.filter((answer) => !expected.has(answer?.id)).map((answer) => answer?.id ?? null);
  return { kept, dropped };
}

// Fails unless `answers` holds exactly one well-formed object per expected id.
export function checkAnswers(answers, expectedIds, where) {
  if (!Array.isArray(answers)) throw new Error(`${where}: the answer is not an array`);
  const expected = new Set(expectedIds);
  const seen = new Set();
  for (const answer of answers) {
    const id = answer?.id;
    if (typeof id !== 'string' || !expected.has(id)) throw new Error(`${where}: answer for unknown id ${JSON.stringify(id)}`);
    if (seen.has(id)) throw new Error(`${where}: duplicate answer for ${id}`);
    seen.add(id);
    if (!VERDICTS.includes(answer.verdict)) throw new Error(`${where}: ${id} has verdict ${JSON.stringify(answer.verdict)}`);
    if (!Number.isInteger(answer.off_goal_percent) || answer.off_goal_percent < 0 || answer.off_goal_percent > 100) {
      throw new Error(`${where}: ${id} has off_goal_percent ${JSON.stringify(answer.off_goal_percent)}`);
    }
    if (!Array.isArray(answer.off_goal_kinds) || answer.off_goal_kinds.some((kind) => !KINDS.includes(kind))) {
      throw new Error(`${where}: ${id} has off_goal_kinds ${JSON.stringify(answer.off_goal_kinds)}`);
    }
    if (typeof answer.off_goal_quote !== 'string' || typeof answer.reason !== 'string') {
      throw new Error(`${where}: ${id} lacks a string off_goal_quote or reason`);
    }
  }
  const missing = expectedIds.filter((id) => !seen.has(id));
  if (missing.length > 0) throw new Error(`${where}: no answer for ${missing.length} message(s), first ${missing[0]}`);
  return answers;
}

// Runs `claude -p` on the input, with the invocation the recovered review used. Returns the result text.
export function callClaude({ model, input, where }) {
  const args = ['-p', '--model', model, '--output-format', 'json', '--dangerously-skip-permissions', '--disallowedTools', CLAUDE_TOOLS_OFF];
  const env = { ...process.env, CLAUDE_CONFIG_DIR: `${os.homedir()}/.claude` };
  return new Promise((resolve, reject) => {
    const child = spawn('claude', args, { env, stdio: ['pipe', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    const timer = setTimeout(() => child.kill('SIGKILL'), CLAUDE_TIMEOUT_MS);
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
    });
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    child.on('error', (error) => {
      clearTimeout(timer);
      reject(new Error(`${where}: cannot start claude: ${error.message}`));
    });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      if (code !== 0) {
        reject(new Error(`${where}: claude exited with ${code ?? signal}: ${stderr.slice(-400)}`));
        return;
      }
      let envelope;
      try {
        envelope = JSON.parse(stdout);
      } catch (error) {
        if (!(error instanceof SyntaxError)) throw error;
        reject(new Error(`${where}: claude printed no JSON envelope: ${error.message}`));
        return;
      }
      if (envelope.is_error) reject(new Error(`${where}: claude reported an error: ${String(envelope.result).slice(0, 400)}`));
      else resolve(String(envelope.result));
    });
    child.stdin.end(input);
  });
}

// Runs `tasks` (functions returning promises) with at most `limit` in flight. The first failure stops the pool from
// starting further tasks and rejects the run.
export async function runPool(tasks, limit) {
  let next = 0;
  let failed = false;
  const worker = async () => {
    while (!failed && next < tasks.length) {
      const index = next;
      next += 1;
      try {
        await tasks[index]();
      } catch (error) {
        failed = true;
        throw error;
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, tasks.length) }, worker));
}

// The local files that one message points to, merged into the message for the judge.
//
// A message names files as `~/...`, as an absolute path under the home directory or `/tmp`, or as `artifacts/...`
// relative to the session that holds the artifact. Each named path ends in one state:
//   read       the file is text; its text, capped at FILE_CAP characters, joins the message
//   truncated  the text is longer than the cap; the first FILE_CAP characters join the message
//   own        the file lies in the receiving child's own session; the name alone joins the message
//   missing    the path does not exist now
//   directory  the path is a directory
//   binary     the file is not UTF-8 text

import fs from 'node:fs';
import path from 'node:path';

export const FILE_CAP = 30000;

function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// The same pattern the recovered review used, with the home directory taken from the running user.
export function pathPattern(home) {
  return new RegExp(
    `(?:/absolute_filepath)?((?:~|${escapeRegExp(home)}|/tmp)/[!#-&+-:=?-Z^_a-z~]+|(?<![\\w/.~-])artifacts/[\\w.\\-]+)`,
    'g',
  );
}

function codePoints(text) {
  return Array.from(text);
}

// The text a judge reads for an HTML file: tags, styles and scripts reduced to whitespace-separated text.
export function htmlText(html) {
  return html
    .replace(/<style.*?<\/style>|<script.*?<\/script>/gs, '')
    .replace(/<[^>]+>/g, ' ')
    .replace(/[ \t]+/g, ' ')
    .replace(/\n\s*\n+/g, '\n');
}

// The file's text with newlines normalized the way text-mode file reads do, or null when the bytes are not UTF-8.
function readText(filePath) {
  const bytes = fs.readFileSync(filePath);
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes).replace(/\r\n?/g, '\n');
  } catch (error) {
    if (error.code !== 'ERR_ENCODING_INVALID_ENCODED_DATA') throw error;
    return null;
  }
}

// `root` is the sessions directory; `sender` and `child` are the parent and child session ids of the message.
export function mergeFiles({ text, sender, child, root, home }) {
  const files = [];
  const seen = new Set();
  const childDir = path.join(root, child) + path.sep;
  for (const match of text.matchAll(pathPattern(home))) {
    const raw = match[1].replace(/[.:,]+$/, '');
    let filePath = raw.startsWith('~') ? home + raw.slice(1) : raw;
    if (filePath.startsWith('artifacts/')) {
      const candidates = [path.join(root, sender, filePath), path.join(root, child, filePath)];
      filePath = candidates.find((candidate) => fs.existsSync(candidate)) ?? candidates[0];
    }
    filePath = filePath.replace(/:\d+(-\d+)?$/, '');
    if (seen.has(filePath)) continue;
    seen.add(filePath);
    if (filePath.startsWith(childDir)) {
      files.push({ path: raw, state: 'own' });
      continue;
    }
    if (!fs.existsSync(filePath)) {
      files.push({ path: raw, state: 'missing' });
      continue;
    }
    if (fs.statSync(filePath).isDirectory()) {
      files.push({ path: raw, state: 'directory' });
      continue;
    }
    let body = readText(filePath);
    if (body === null) {
      files.push({ path: raw, state: 'binary' });
      continue;
    }
    if (filePath.endsWith('.html')) body = htmlText(body);
    const chars = codePoints(body);
    const truncated = chars.length > FILE_CAP;
    files.push({
      path: raw,
      state: truncated ? 'truncated' : 'read',
      chars: chars.length,
      text: chars.slice(0, FILE_CAP).join(''),
    });
  }
  return files;
}

export function isMerged(file) {
  return file.state === 'read' || file.state === 'truncated';
}

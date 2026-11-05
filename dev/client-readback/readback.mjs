// The client-timing readback: per device, what the web client waited on over a time window of
// CharlieBot server logs. Every device's page reports its waits to `POST /api/diag/client-events`,
// and the server writes each one as one `diag_client` log line; this script summarizes those lines
// per device, so a poor-network fix gets its before and after numbers from real devices.
//
//   node dev/client-readback/readback.mjs --from <UTC ISO> --to <UTC ISO> [--device <address>] <log>...
//
// The window selects lines by the log's own timestamp. Without --device every device that sent an
// event gets a section, under the browser address the server logged (`forwarded_for`).
//
// Per device the sections are:
//   switch      completed session switches: elapsed p50/p90/p99 and the count over 5 s
//   send        click-to-echo waits: p50/p90
//   socket      network-caused disconnects: count, and the reconnect-wait p50/p90/max
//   traffic     received bytes per tab-hour by endpoint (a tab-hour is the tab-time the traffic
//               events' windows cover)
//   page loads  the count of each load cause
//
// A percentile is the sorted sample's `floor(p * (n - 1))` element, the convention the research
// scripts of the poor-network line use.

import fs from 'node:fs';

// The fields each kind's log line carries, in the order the server writes them after `diag_client`.
const FIELDS = {
  switch: ['kind', 'phase', 'from_session', 'to_session', 'generation', 'winner_generation', 'elapsed_ms',
    'error', 'client_ts', 'request_start', 'response_start', 'response_end', 'transfer_size', 'forwarded_for'],
  send: ['kind', 'request_id', 'click_to_response_ms', 'click_to_echo_ms', 'retries', 'client_ts', 'forwarded_for'],
  socket: ['kind', 'disconnect_at', 'cause', 'close_code', 'reconnect_at', 'reconnect_count', 'catchup_at',
    'catchup_bytes', 'client_ts', 'forwarded_for'],
  page_load: ['kind', 'response_end_ms', 'dom_content_loaded_ms', 'transfer_size', 'cause', 'client_ts',
    'forwarded_for'],
  traffic: ['kind', 'window_s', 'endpoints', 'visibility_state', 'client_ts', 'forwarded_for'],
};

const TIMESTAMP_SPAN_MS = 24; // `2026-11-05 12:16:56.557Z`, the log line's first 24 characters
const SLOW_SWITCH_MS = 5000;

function usage(message) {
  if (message) console.error(message);
  console.error('usage: node dev/client-readback/readback.mjs --from <UTC ISO> --to <UTC ISO> ' +
    '[--device <address>] <log>...');
  process.exit(2);
}

function parseArgs(argv) {
  const args = {from: null, to: null, device: null, logs: []};
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    if (arg === '--from' || arg === '--to' || arg === '--device') {
      const value = argv[index + 1];
      if (value === undefined) usage(`${arg} takes a value`);
      args[arg.slice(2)] = value;
      index++;
    } else if (arg.startsWith('--from=') || arg.startsWith('--to=') || arg.startsWith('--device=')) {
      const [name, value] = arg.split('=', 2);
      if (!value) usage(`${name} takes a value`);
      args[name.slice(2)] = value;
    } else if (arg.startsWith('--')) {
      usage(`unknown flag: ${arg}`);
    } else {
      args.logs.push(arg);
    }
  }
  if (!args.from || !args.to) usage('a time window is required: --from <UTC ISO> --to <UTC ISO>');
  if (args.logs.length === 0) usage('at least one log file is required');
  const from = Date.parse(args.from);
  const to = Date.parse(args.to);
  if (Number.isNaN(from) || Number.isNaN(to)) usage('the window bounds must parse as dates');
  if (to <= from) usage('--to must be after --from');
  const windowText = `${args.from}..${args.to}`;
  return {...args, from, to, windowText};
}

function parseTimestamp(line) {
  return Date.parse(line.slice(0, TIMESTAMP_SPAN_MS).replace(' ', 'T'));
}

function isFiniteNumber(value) {
  return value !== null && value !== '' && Number.isFinite(Number(value));
}

// The log's value form is bare, or a quote wrapping `text_repr::str_repr` (the console renderer
// quotes a value holding whitespace, `=`, a quote or a control character). The endpoints JSON of a
// traffic row arrives quoted, so the parser decodes the wrapper before anything reads the value.
function unlogValue(value) {
  if (value.length >= 2) {
    const quote = value[0];
    if ((quote === "'" || quote === '"') && value.endsWith(quote)) {
      return value.slice(1, -1).replace(/\\(.)/g, (_, char) =>
        (char === 'n' ? '\n' : char === 't' ? '\t' : char === 'r' ? '\r' : char));
    }
  }
  return value;
}

// The `diag_client` events of one log file, as `{at, device, kind, fields}`. A line whose kind is
// unknown, or whose fields the line's own kind does not carry, is skipped.
function parseLog(file) {
  const events = [];
  const lines = fs.readFileSync(file, 'utf8').split('\n');
  for (const line of lines) {
    const marker = line.indexOf('diag_client');
    if (marker === -1) continue;
    const at = parseTimestamp(line);
    if (Number.isNaN(at)) continue;
    // The console renderer pads the message column, so the fields start after the padding.
    const rest = line.slice(marker + 'diag_client'.length).trim();
    const kindMatch = /^kind=(\S+)/.exec(rest);
    const names = kindMatch && FIELDS[kindMatch[1]];
    if (!names) continue;
    // Values run to the next ` name=` boundary of the kind's own field list; a free-text value
    // (a switch's error) survives whole unless it names a field of this kind.
    const boundaries = [];
    let searchFrom = 0;
    for (const name of names) {
      const at = rest.indexOf(` ${name}=`, searchFrom);
      if (at === -1) continue;
      boundaries.push({name, at});
      searchFrom = at + name.length + 2;
    }
    for (let index = 0; index < boundaries.length; index++) {
      boundaries[index].valueEnd = index + 1 < boundaries.length ? boundaries[index + 1].at : rest.length;
    }
    const fields = {};
    for (const boundary of boundaries) {
      const key = boundary.name;
      const value = unlogValue(rest.slice(boundary.at + key.length + 2, boundary.valueEnd).trim());
      if (fields[key] === undefined) fields[key] = value;
    }
    const device = fields.forwarded_for;
    if (!device) continue;
    events.push({at, device, kind: kindMatch[1], fields});
  }
  return events;
}

function percentile(sorted, p) {
  if (sorted.length === 0) return null;
  return sorted[Math.floor(p * (sorted.length - 1))];
}

function percentileText(sorted, unit = 'ms') {
  if (sorted.length === 0) return 'n=0';
  return `n=${sorted.length} p50=${percentile(sorted, 0.5)}${unit} p90=${percentile(sorted, 0.9)}${unit}`;
}

function summarizeDevice(events) {
  const byKind = {switch: [], send: [], socket: [], page_load: [], traffic: []};
  for (const event of events) byKind[event.kind].push(event);

  const completedElapsed = byKind.switch
    .filter((event) => event.fields.phase === 'completed' && isFiniteNumber(event.fields.elapsed_ms))
    .map((event) => Number(event.fields.elapsed_ms))
    .sort((a, b) => a - b);
  const overSlow = completedElapsed.filter((elapsed) => elapsed > SLOW_SWITCH_MS).length;

  const echoWaits = byKind.send
    .filter((event) => isFiniteNumber(event.fields.click_to_echo_ms))
    .map((event) => Number(event.fields.click_to_echo_ms))
    .sort((a, b) => a - b);

  const disconnects = byKind.socket.filter((event) => event.fields.cause === 'network');
  const reconnectWaits = disconnects
    .filter((event) => event.fields.reconnect_at && event.fields.reconnect_at !== 'null')
    .map((event) => (Date.parse(event.fields.reconnect_at) - Date.parse(event.fields.disconnect_at)) / 1000)
    .filter((seconds) => Number.isFinite(seconds) && seconds >= 0)
    .sort((a, b) => a - b);

  let tabSeconds = 0;
  const endpointBytes = new Map();
  for (const event of byKind.traffic) {
    const windowS = Number(event.fields.window_s);
    if (!Number.isFinite(windowS)) continue;
    tabSeconds += windowS;
    let endpoints;
    try {
      endpoints = JSON.parse(event.fields.endpoints);
    } catch {
      continue;
    }
    for (const row of endpoints) {
      if (!row || typeof row.endpoint !== 'string' || !Number.isFinite(row.transfer_size)) continue;
      endpointBytes.set(row.endpoint, (endpointBytes.get(row.endpoint) || 0) + row.transfer_size);
    }
  }

  const causes = {};
  for (const event of byKind.page_load) {
    causes[event.fields.cause] = (causes[event.fields.cause] || 0) + 1;
  }

  return {completedElapsed, overSlow, echoWaits, disconnects, reconnectWaits, tabSeconds, endpointBytes, causes};
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const events = args.logs.flatMap((log) => parseLog(log)).filter((event) => event.at >= args.from && event.at < args.to)
    .sort((a, b) => a.at - b.at);
  const devices = [...new Set(events.map((event) => event.device))]
    .filter((device) => args.device === null || device === args.device)
    .sort();

  if (devices.length === 0) {
    console.log(`no client events in ${args.windowText}${args.device ? ` for ${args.device}` : ''}`);
    return;
  }


  for (const device of devices) {
    const deviceEvents = events.filter((event) => event.device === device);
    const summary = summarizeDevice(deviceEvents);
    console.log(`== ${device}: ${deviceEvents.length} event${deviceEvents.length === 1 ? '' : 's'}`);

    console.log('  switch completed ' + percentileText(summary.completedElapsed) +
      (summary.completedElapsed.length
        ? ` p99=${percentile(summary.completedElapsed, 0.99)}ms`
        : '') +
      ` over-5s=${summary.overSlow}`);
    console.log('  send click-to-echo ' + percentileText(summary.echoWaits));

    const waits = summary.reconnectWaits.map((seconds) => seconds.toFixed(1));
    console.log(`  socket network disconnects n=${summary.disconnects.length}` +
      (waits.length
        ? ` reconnect-wait p50=${percentile(waits, 0.5)}s p90=${percentile(waits, 0.9)}s max=${waits[waits.length - 1]}s`
        : ' reconnect-wait n=0'));

    const tabHours = summary.tabSeconds / 3600;
    if (tabHours > 0) {
      console.log(`  traffic received bytes per tab-hour (tab-time ${tabHours.toFixed(2)} h):`);
      for (const [endpoint, bytes] of [...summary.endpointBytes].sort((a, b) => b[1] - a[1])) {
        console.log(`    ${endpoint} ${Math.round(bytes / tabHours)}`);
      }
    } else {
      console.log('  traffic none');
    }

    const causes = Object.entries(summary.causes).map(([cause, count]) => `${cause}=${count}`).join(' ');
    console.log('  page loads ' + (causes || 'none'));
  }
}

main();

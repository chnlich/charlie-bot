// Contract test for the client-timing readback. The script runs for real on a fixture log that
// carries every kind, and the numbers each section must print are asserted. Every address and id
// here is synthetic.

import assert from 'node:assert/strict';
import {execFileSync} from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import {fileURLToPath} from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const READBACK = path.join(HERE, '..', '..', 'dev', 'client-readback', 'readback.mjs');

// A diag_client line the way the server writes it: the log timestamp, the
// location, the message and the fields in declaration order, then the
// browser's forwarded address. The synthetic device address is from the
// 100.64.0.0/10 shared-address range.
function line(stamp, fields) {
  return `${stamp} [info     ] diag/api.rs:120                diag_client                    ${fields}`;
}

const SWITCH_COMPLETED = 'kind=switch phase=completed from_session=0f0e1d2c-3b4a-5968-7789-aabbccddeeff ' +
  'to_session=1f2e3d4c-5b6a-7988-8799-bbccddeeff00 generation=7 winner_generation=6 elapsed_ms=1500 error=null ' +
  'client_ts=2026-02-13T10:00:01Z request_start=100 response_start=400 response_end=1500 transfer_size=26592 ' +
  'forwarded_for=100.64.0.8';

const LOG_LINES = [
  // Switches: completed 1000, 2000, 3000 ms, plus one at 6000 ms (over 5 s),
  // and a superseded phase the switch numbers ignore.
  line('2026-02-13 09:00:00.100Z', 'kind=switch phase=superseded from_session=null to_session=1f2e3d4c-5b6a-7988-8799-bbccddeeff00 ' +
    'generation=5 winner_generation=6 elapsed_ms=2000 error=null client_ts=2026-02-13T09:00:00Z request_start=null ' +
    'response_start=null response_end=null transfer_size=null forwarded_for=100.64.0.8'),
  line('2026-02-13 09:01:00.100Z', SWITCH_COMPLETED.replace('elapsed_ms=1500', 'elapsed_ms=1000')),
  line('2026-02-13 09:02:00.100Z', SWITCH_COMPLETED.replace('elapsed_ms=1500', 'elapsed_ms=2000')),
  line('2026-02-13 09:03:00.100Z', SWITCH_COMPLETED.replace('elapsed_ms=1500', 'elapsed_ms=3000')),
  line('2026-02-13 09:04:00.100Z', SWITCH_COMPLETED.replace('elapsed_ms=1500', 'elapsed_ms=6000')),

  // Sends: echo waits 500 and 1500 ms.
  line('2026-02-13 09:05:00.100Z', 'kind=send request_id=2c3d4e5f-6a7b-8901-abcd-eeff00112233 ' +
    'click_to_response_ms=300 click_to_echo_ms=500 retries=1 client_ts=2026-02-13T09:05:00Z forwarded_for=100.64.0.8'),
  line('2026-02-13 09:06:00.100Z', 'kind=send request_id=3d4e5f6a-7b8c-9012-bcde-eff001122334 ' +
    'click_to_response_ms=400 click_to_echo_ms=1500 retries=0 client_ts=2026-02-13T09:06:00Z forwarded_for=100.64.0.8'),

  // Sockets: one network episode with a 2.0 s reconnect wait, one switch
  // episode the socket numbers ignore, one network episode that never
  // reconnected.
  line('2026-02-13 09:07:00.100Z', 'kind=socket disconnect_at=2026-02-13T09:06:40Z cause=network close_code=1006 ' +
    'reconnect_at=2026-02-13T09:06:42Z reconnect_count=1 catchup_at=2026-02-13T09:06:43Z catchup_bytes=80860 ' +
    'client_ts=2026-02-13T09:07:00Z forwarded_for=100.64.0.8'),
  line('2026-02-13 09:08:00.100Z', 'kind=socket disconnect_at=2026-02-13T09:07:40Z cause=switch close_code=null ' +
    'reconnect_at=2026-02-13T09:07:41Z reconnect_count=1 catchup_at=2026-02-13T09:07:42Z catchup_bytes=300 ' +
    'client_ts=2026-02-13T09:08:00Z forwarded_for=100.64.0.8'),
  line('2026-02-13 09:09:00.100Z', 'kind=socket disconnect_at=2026-02-13T09:08:40Z cause=network close_code=1006 ' +
    'reconnect_at=null reconnect_count=null catchup_at=null catchup_bytes=null client_ts=2026-02-13T09:09:00Z ' +
    'forwarded_for=100.64.0.8'),

  // Traffic: two windows of one minute each (0.0333 tab-hours), /api/sessions/
  // at 120000 bytes total and the concurrency poll at 120 bytes total.
  line('2026-02-13 09:10:00.100Z', 'kind=traffic window_s=60 ' +
    'endpoints=[{"endpoint":"/api/sessions/","transfer_size":60000,"count":1},' +
    '{"endpoint":"/api/concurrency/now","transfer_size":73,"count":12}] visibility_state=visible ' +
    'client_ts=2026-02-13T09:10:00Z forwarded_for=100.64.0.8'),
  line('2026-02-13 09:11:00.100Z', 'kind=traffic window_s=60 ' +
    'endpoints=[{"endpoint":"/api/sessions/","transfer_size":60000,"count":1},' +
    '{"endpoint":"/api/concurrency/now","transfer_size":47,"count":12}] visibility_state=hidden ' +
    'client_ts=2026-02-13T09:11:00Z forwarded_for=100.64.0.8'),

  // Page loads: one fresh open, one bootstrap failure.
  line('2026-02-13 09:12:00.100Z', 'kind=page_load response_end_ms=33000 dom_content_loaded_ms=34500 ' +
    'transfer_size=415115 cause=open client_ts=2026-02-13T09:12:00Z forwarded_for=100.64.0.8'),
  line('2026-02-13 09:13:00.100Z', 'kind=page_load response_end_ms=32000 dom_content_loaded_ms=33500 ' +
    'transfer_size=415115 cause=bootstrap_failure client_ts=2026-02-13T09:13:00Z forwarded_for=100.64.0.8'),

  // A second device with one event, outside neither window nor kind coverage.
  line('2026-02-13 09:14:00.100Z', 'kind=page_load response_end_ms=1000 dom_content_loaded_ms=2000 ' +
    'transfer_size=0 cause=open client_ts=2026-02-13T09:14:00Z forwarded_for=100.64.0.9'),

  // Outside the window: dropped.
  line('2026-02-14 09:00:00.100Z', 'kind=page_load response_end_ms=1 dom_content_loaded_ms=1 transfer_size=0 ' +
    'cause=open client_ts=2026-02-14T09:00:00Z forwarded_for=100.64.0.8'),

  // A non-diag line: dropped.
  '2026-02-13 09:15:00.100Z [info     ] server/request_log.rs:58       http_request                   ' +
    'method=GET path=/api/sessions/ status=200 duration_ms=1 client=127.0.0.1 forwarded_for=100.64.0.8',
];

test('the readback prints every section per device on the fixture log', () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'client-readback-test-'));
  try {
    const log = path.join(scratch, 'server.log');
    fs.writeFileSync(log, LOG_LINES.join('\n') + '\n');

    const output = execFileSync(process.execPath, [READBACK,
      '--from', '2026-02-13T09:00:00Z', '--to', '2026-02-13T10:00:00Z', log], {encoding: 'utf8'});

    // Percentiles are the sorted sample's floor(p * (n - 1)) element: the
    // completed switches [1000, 2000, 3000, 6000] give p50=2000, p90=3000,
    // p99=6000; the echo waits [500, 1500] give p50=p90=500.
    assert.match(output, /== 100\.64\.0\.8: 14 events/);
    assert.match(output, /switch completed n=4 p50=2000ms p90=3000ms p99=3000ms/);
    assert.match(output, /over-5s=1/);
    assert.match(output, /send click-to-echo n=2 p50=500ms p90=500ms/);
    assert.match(output, /socket network disconnects n=2 reconnect-wait p50=2\.0s p90=2\.0s max=2\.0s/);
    assert.match(output, /traffic received bytes per tab-hour \(tab-time 0\.03 h\):/);
    assert.match(output, /\/api\/sessions\/ 3600000/);
    assert.match(output, /\/api\/concurrency\/now 3600/);
    assert.match(output, /page loads open=1 bootstrap_failure=1/);
    assert.match(output, /== 100\.64\.0\.9: 1 event/);
    assert.doesNotMatch(output, /superseded|cause=switch/,
        'the socket numbers count only network causes and the switch numbers only completed phases');
  } finally {
    fs.rmSync(scratch, {recursive: true, force: true});
  }
});

test('the device flag keeps one device and an empty window says so', () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'client-readback-test-'));
  try {
    const log = path.join(scratch, 'server.log');
    fs.writeFileSync(log, LOG_LINES.join('\n') + '\n');

    const output = execFileSync(process.execPath, [READBACK,
      '--from', '2026-02-13T09:00:00Z', '--to', '2026-02-13T10:00:00Z', '--device', '100.64.0.9', log],
    {encoding: 'utf8'});
    assert.match(output, /== 100\.64\.0\.9: 1 event/);
    assert.doesNotMatch(output, /100\.64\.0\.8/);

    const empty = execFileSync(process.execPath, [READBACK,
      '--from', '2026-01-01T00:00:00Z', '--to', '2026-01-01T01:00:00Z', log], {encoding: 'utf8'});
    assert.match(empty, /no client events/);
  } finally {
    fs.rmSync(scratch, {recursive: true, force: true});
  }
});

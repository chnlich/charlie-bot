const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const test = require("node:test");

const template = readFileSync(path.join(__dirname, "../features/voice/templates/voice/recordings.html"), "utf8");

test("recent recordings keeps all values in six merged cells", () => {
  const headers = [...template.matchAll(/<th>([^<]*)<\/th>/g)].map((match) => match[1]);
  assert.deepEqual(headers, ["Recorded", "Peak / RMS", "Input device", "Transcript or failure", "Run", "Playback"]);

  const row = template.match(/{% for row in recordings %}\s*<tr>([\s\S]*?)<\/tr>/)?.[1];
  assert.ok(row, "recording row template exists");
  assert.equal([...row.matchAll(/<td\b/g)].length, 6);

  for (const value of [
    "row.recorded_at",
    "row.session_name",
    "row.session_id",
    "row.digital_silence",
    "row.peak_dbfs",
    "row.rms_dbfs",
    "row.input_device",
    "row.failure",
    "row.transcript",
    "row.startup_error",
    "row.backend",
    "row.reached",
    "row.end",
    "row.mic_permission",
    "row.duration_s",
    "row.audio_url",
  ]) {
    assert.ok(row.includes(value), `${value} stays in the row`);
  }

  assert.match(row, /data-l="Recorded"[\s\S]*?row\.session_name[\s\S]*?row\.session_id/);
  assert.match(row, /data-l="Run"[\s\S]*?row\.backend[\s\S]*?row\.reached[\s\S]*?row\.end/);
  assert.match(row, /data-l="Playback"[\s\S]*?row\.duration_s[\s\S]*?row\.audio_url/);
  assert.match(template, /colspan="6"/);
});

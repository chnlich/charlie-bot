const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const test = require("node:test");

const template = readFileSync(path.join(__dirname, "../features/voice/templates/voice/recordings.html"), "utf8");

test("recent recordings keeps all values in four merged cells", () => {
  const headers = [...template.matchAll(/<th>([^<]*)<\/th>/g)].map((match) => match[1]);
  assert.deepEqual(headers, ["Recorded", "Transcript or failure", "Run / Input device", "Playback / Peak / RMS"]);

  const row = template.match(/{% for row in recordings %}\s*<tr>([\s\S]*?)<\/tr>/)?.[1];
  assert.ok(row, "recording row template exists");
  assert.equal([...row.matchAll(/<td\b/g)].length, 4);

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
  assert.match(row, /data-l="Transcript or failure"[\s\S]*?row\.failure[\s\S]*?row\.transcript[\s\S]*?row\.startup_error/);
  assert.match(
    row,
    /data-l="Run \/ Input device"[\s\S]*?row\.backend[\s\S]*?row\.reached[\s\S]*?row\.end[\s\S]*?row\.input_device/,
  );
  assert.match(
    row,
    /data-l="Playback \/ Peak \/ RMS"[\s\S]*?row\.duration_s[\s\S]*?row\.audio_url[\s\S]*?row\.digital_silence[\s\S]*?row\.peak_dbfs/,
  );
  assert.match(template, /colspan="4"/);
});

// ---------------------------------------------------------------------------
// Voice input: record locally, then use the selected backend's live relay or upload
// ---------------------------------------------------------------------------
// One recording = one run object owning its mic stream, capture graph, local PCM
// buffer, and at most one in-flight upload request. The page-level click handler
// starts the mic request. This module claims the run slot when the module arrives.
// Every asynchronous continuation checks that the slot still belongs to it.
//
// The recording itself never touches the persistence path. The worklet's 16 kHz
// PCM chunks append to a local buffer, and the level bar plus elapsed timer are
// computed from those chunks client-side. On release the whole buffer assembles
// into a WAV and uploads once. The server persists it and returns the full text.
// Every failure keeps the buffer and offers retry. Only success or an explicit
// cancel discards it.
//
// The backend comes from the caret menu beside the microphone (the choices ride
// the page as VOICE_BACKENDS). With a backend whose livePartials is true the
// same chunks also stream to the server's preview relay (/ws/voice/{session}).
// That relay forwards the backend's partials while speaking and keeps the
// audio. On stop the run waits for the relay's final inside a budget that grows
// with the recording. The budget is 2 s plus 20 ms per recorded second, because
// the backend needs longer to finalize longer recordings. Receiving the final
// means the relay has already archived the recording: the words go in and the
// run ends with no upload. A live recording that ends with no final never uploads
// on its own. The run enters the stop state, where the buffer stays, and the hint
// names the backend and the reason. One click on the voice button starts the
// upload whose transcript owns the text. The budget expiry itself is not a
// failure:
// the relay stays open, so a final arriving before that click still wins and
// ends the run with no upload. A relay failure (an error frame, a lost socket,
// a failed end send) never starts an upload either. During recording the reason
// lands in the hint at once and the recording continues. At stop the run enters
// the stop state. On a non-live backend the whole buffer assembles
// into a WAV and uploads once, right after stop. The server then archives the
// recording and returns the full text. No request goes out mid-recording.
let activeVoiceRun = null;

const VOICE_SAMPLE_RATE = 16000;
const VOICE_CHUNK_SAMPLES = 2048; // 128 ms of audio per worklet chunk
const VOICE_MAX_SAMPLES = 5 * 60 * VOICE_SAMPLE_RATE; // the server's recording cap
// The upload-phase budget: past it the request aborts into the retry state. The
// decode wait after the body is sent is unbounded but visible ("Decoding...").
const VOICE_UPLOAD_TIMEOUT_MS = 60 * 1000;
// The longest wait for a suspended audio engine to start after both startup
// branches settle. The resume promise can stay pending forever, so this timer
// ends the wait.
const VOICE_CONTEXT_RESUME_BOUND_MS = 2 * 1000;
// The send gate's message while a transcription window (upload + decode) is open.
const VOICE_TRANSCRIBE_IN_FLIGHT_MESSAGE = 'Please wait for the voice transcription to finish';
// The preview relay's final budget after the end frame: a base plus 20 ms per
// recorded second. The backend needs longer to finalize longer recordings, so
// the budget grows with the run. A fixed wait is either too short for long
// recordings or too long for short ones.
const VOICE_RELAY_FINAL_BUDGET_BASE_MS = 2 * 1000;
const VOICE_RELAY_FINAL_BUDGET_PER_SECOND_MS = 20;
// WebSocket readyState OPEN: relay chunks go out only on an open socket. The
// chunks captured before that ride the open event's replay.
const VOICE_WS_OPEN = 1;
// The states `run.phase` takes. A live recording enters the stop state when it
// ends with no final. An upload that fails enters the retry state with its
// buffer kept. Every read and write of the phase names a member here.
const VOICE_RUN_PHASE = {
  IDLE: 'idle',
  RECORDING: 'recording',
  STOP: 'stop',
  UPLOADING: 'uploading',
  DECODING: 'decoding',
  RETRY: 'retry',
};
// The states the voice button shows: amber while the engine arms, red once
// sound arrives, and slate for the idle button between runs.
const VOICE_BUTTON_STATE = {
  STARTING: 'starting',
  RECORDING: 'recording',
  IDLE: 'idle',
};

const VOICE_HINT_LISTENING = 'Speak now';
const VOICE_HINT_WAITING = 'Preparing microphone…';
const VOICE_HINT_MIC_SILENT = 'Microphone went silent for 2 s, recording stopped';
const VOICE_SILENCE_THRESHOLD = 32768 * Math.pow(10, -75 / 20);
const VOICE_SILENCE_WARMUP_CHUNKS = 2;
const VOICE_SILENCE_CHUNKS = 16;
const VOICE_HINT_UPLOADING = 'Uploading...';
const VOICE_HINT_DECODING = 'Decoding...';
const VOICE_HINT_RETRY = 'Upload failed — click to retry';

const VOICE_OVERLAY_CLASS = 'absolute left-0 right-0 bottom-full mb-2 rounded-lg border border-blue-500/40 bg-slate-800 px-3 py-2 text-sm text-slate-100 shadow-lg';
// Not-yet-final words from the preview relay: grey, above the level bar.
const VOICE_PARTIAL_CLASS = 'voice-partial-text text-sm text-slate-400 whitespace-pre-wrap break-words max-h-28 overflow-y-auto';
// The caret's dropdown, anchored above the mic group the template renders. voice.css sizes it to its longest row.
const VOICE_MENU_CLASS = 'absolute bottom-full right-0 mb-2 rounded-lg border border-slate-600 bg-slate-800 py-1 shadow-lg z-50';
// The menu sits on slate-800. There, slate-500 holds 3.07:1, under the 4.5:1 floor for the title's
// 12 px and the unavailable rows' 14 px. A slate-400 label holds 5.7:1 and still reads dimmer than
// the enabled rows' slate-200.
const VOICE_MENU_TITLE_CLASS = 'px-3 py-1.5 text-xs text-slate-400';
const VOICE_MENU_ITEM_CLASS = 'flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm';
const VOICE_MENU_ITEM_ENABLED_CLASS = ' text-slate-200 hover:bg-slate-700';
// An unavailable backend stays readable: its label names the config key the host must provide.
const VOICE_MENU_ITEM_DISABLED_CLASS = ' text-slate-400 cursor-not-allowed';
const VOICE_MENU_CHECK_CLASS = 'w-4 shrink-0 text-green-400';

const VOICE_WORKLET_SOURCE = `
class VoiceCaptureProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.inputRate = options.processorOptions.inputSampleRate;
    this.outputRate = ${VOICE_SAMPLE_RATE};
    this.ratio = this.inputRate / this.outputRate;
    this.sourceRemainder = new Float32Array(0);
    this.sourcePosition = 0;
    this.outputSamples = [];
    this.chunkSamples = options.processorOptions.chunkSamples;
    this.port.onmessage = (event) => {
      if (event.data && event.data.type === 'flush') {
        this.flush(event.data.id);
      }
    };
  }

  process(inputs) {
    const input = inputs[0];
    if (!input || !input[0] || input[0].length === 0) return true;
    this.consume(input[0]);
    return true;
  }

  consume(channel) {
    const merged = new Float32Array(this.sourceRemainder.length + channel.length);
    merged.set(this.sourceRemainder, 0);
    merged.set(channel, this.sourceRemainder.length);

    while (this.sourcePosition + 1 < merged.length) {
      const index = Math.floor(this.sourcePosition);
      const fraction = this.sourcePosition - index;
      const sample = merged[index] + (merged[index + 1] - merged[index]) * fraction;
      this.outputSamples.push(this.toInt16(sample));
      if (this.outputSamples.length >= this.chunkSamples) {
        this.sendChunk(this.chunkSamples);
      }
      this.sourcePosition += this.ratio;
    }

    // The read position can step past the buffer's end by less than one input
    // sample. That carry belongs to the next buffer's first sample. Capping the
    // consumed count at the buffer length keeps it, so the output count stays
    // the input duration at 16 kHz.
    const consumed = Math.min(Math.floor(this.sourcePosition), merged.length);
    this.sourceRemainder = merged.slice(consumed);
    this.sourcePosition -= consumed;
  }

  toInt16(sample) {
    const clipped = Math.max(-1, Math.min(1, sample));
    return clipped < 0 ? Math.round(clipped * 32768) : Math.round(clipped * 32767);
  }

  sendChunk(count) {
    const values = this.outputSamples.splice(0, count);
    const pcm = new Int16Array(values.length);
    for (let i = 0; i < values.length; i++) pcm[i] = values[i];
    this.port.postMessage({type: 'pcm', buffer: pcm.buffer}, [pcm.buffer]);
  }

  flush(id) {
    if (this.outputSamples.length > 0) {
      this.sendChunk(this.outputSamples.length);
    }
    this.port.postMessage({type: 'flushed', id});
  }
}

registerProcessor('voice-capture', VoiceCaptureProcessor);
`;

// ---------------------------------------------------------------------------
// The caret menu
// ---------------------------------------------------------------------------
// The choices ride the page as VOICE_BACKENDS (id, label, livePartials,
// unavailableReason) with the server's VOICE_DEFAULT_BACKEND. The user's pick
// lives in localStorage. The relay, the upload's backend field, and the mic's
// tooltip all read the same selection. None of them compares
// against a backend id — the livePartials attribute decides everything. The
// page's voice-events part owns the selection and the tooltip, so both exist
// before this file loads.

let voiceMenu = null;
let voiceMenuRows = null;

function ensureVoiceBackendMenu() {
  if (voiceMenu) return voiceMenu;
  const caret = document.getElementById('voice-backend-btn');
  if (!caret || !caret.parentElement) return null;
  const menu = document.createElement('div');
  menu.id = 'voice-backend-menu';
  menu.className = VOICE_MENU_CLASS;
  menu.classList.add('hidden'); // the caret's first click opens it
  const title = document.createElement('div');
  title.className = VOICE_MENU_TITLE_CLASS;
  title.textContent = 'Voice backend';
  menu.appendChild(title);
  voiceMenuRows = VOICE_BACKENDS.map((entry) => {
    const row = document.createElement('button');
    row.type = 'button';
    const unavailable = !!entry.unavailableReason;
    row.className = VOICE_MENU_ITEM_CLASS + (unavailable ? VOICE_MENU_ITEM_DISABLED_CLASS : VOICE_MENU_ITEM_ENABLED_CLASS);
    const check = document.createElement('span');
    check.className = VOICE_MENU_CHECK_CLASS;
    const labelSpan = document.createElement('span');
    labelSpan.className = 'voice-menu-label';
    // An unconfigured backend stays visible but unselectable, naming the key it
    // needs (never its value).
    labelSpan.textContent = unavailable ? entry.label + ' · ' + entry.unavailableReason : entry.label;
    row.appendChild(check);
    row.appendChild(labelSpan);
    if (unavailable) {
      row.disabled = true;
      row.title = entry.unavailableReason;
    } else {
      row.onclick = () => selectVoiceBackend(entry.id);
    }
    menu.appendChild(row);
    return {entry, row, check};
  });
  caret.parentElement.appendChild(menu);
  voiceMenu = menu;
  renderVoiceBackendMenu();
  return menu;
}

function renderVoiceBackendMenu() {
  if (!voiceMenuRows) return;
  const selected = selectedVoiceBackend();
  for (const {entry, check} of voiceMenuRows) {
    check.textContent = selected && entry.id === selected.id ? '✓' : '';
  }
  updateVoiceMicTitle();
}

function selectVoiceBackend(id) {
  localStorage.setItem(VOICE_BACKEND_STORAGE_KEY, id);
  closeVoiceBackendMenu();
  renderVoiceBackendMenu();
}

function toggleVoiceBackendMenu() {
  const menu = ensureVoiceBackendMenu();
  if (!menu) return;
  if (menu.classList.contains('hidden')) openVoiceBackendMenu();
  else closeVoiceBackendMenu();
}

function openVoiceBackendMenu() {
  const menu = ensureVoiceBackendMenu();
  if (!menu) return;
  menu.classList.remove('hidden');
  renderVoiceBackendMenu();
  positionVoiceBackendMenu();
}

function positionVoiceBackendMenu() {
  // The menu is right-aligned to the mic group. Where the group's right edge
  // leaves too little room for the menu, a negative right offset moves the
  // menu's right edge past the group. Its left edge then sits at the margin.
  const margin = 8;
  voiceMenu.style.left = '';
  voiceMenu.style.right = '';
  const rect = voiceMenu.getBoundingClientRect();
  if (rect.left < margin) {
    voiceMenu.style.left = 'auto';
    voiceMenu.style.right = (rect.left - margin) + 'px';
  } else if (rect.right > window.innerWidth - margin) {
    voiceMenu.style.left = 'auto';
    voiceMenu.style.right = (rect.right - (window.innerWidth - margin)) + 'px';
  }
}

function closeVoiceBackendMenu() {
  if (voiceMenu) voiceMenu.classList.add('hidden');
}

function updateVoiceCaret() {
  // One invariant: the backend cannot change while a run owns the pipeline —
  // recording, upload, and retry all belong to the backend that was chosen.
  const caret = document.getElementById('voice-backend-btn');
  if (!caret) return;
  caret.disabled = !!activeVoiceRun;
}

async function toggleVoice(startRequest) {
  const run = activeVoiceRun;
  if (!run) {
    await startRecording(startRequest);
    return;
  }
  // Clicks during arming or finalizing are ignored, not queued. A recording
  // converts a click into stop, an in-flight upload into a cancel, a failed
  // upload into a retry, and a stopped live recording into the upload. A stop
  // is idempotent: the recording flag clears before the first await.
  if (run.recording) await stopRecording(run, 'stop');
  else if (run.phase === VOICE_RUN_PHASE.UPLOADING || run.phase === VOICE_RUN_PHASE.DECODING) cancelVoiceUpload(run);
  else if (run.phase === VOICE_RUN_PHASE.RETRY) startUpload(run);
  else if (run.phase === VOICE_RUN_PHASE.STOP) {
    // The stop state's one click: the relay closes first, so a final still in
    // flight cannot land beside or after the upload's transcript. The form
    // keeps the live backend id. The server transcribes with that backend.
    closeVoiceRelay(run);
    startUpload(run);
  }
}

// The one construction site of a recording's run object: every phase below
// mutates the fields it initializes, so a field the pipeline needs starts here.
function newVoiceRun(sessionId, backend, startRequest) {
  return {
    sessionId,
    // The run id the browser makes at the click. The startup report, the relay
    // socket and the upload all carry it, so the server keeps every file of
    // this recording under one stem. That stem is set in the voice crate's
    // `recordings.rs`.
    runId: newVoiceRunId(),
    startRequest,
    stream: null,
    audioContext: null,
    sourceNode: null,
    workletNode: null,
    recording: false,
    stopping: false,
    signalReady: false,
    quietWarmupChunks: 0,
    quietChunks: 0,
    lastPartial: '',
    phase: VOICE_RUN_PHASE.IDLE,
    pcmChunks: [],
    totalSamples: 0,
    level: 0,
    xhr: null,
    requestInFlight: false,
    uploadTimedOut: false,
    uploadTimer: null,
    flushId: 0,
    flushResolvers: new Map(),
    ui: null,
    backend,
    devicesPromise: null,
    devices: null,
    startup: newVoiceStartup(startRequest),
    relaySocket: null,
    relayFinalResolve: null,
    relayWaitTimer: null,
    relayFailure: null,
  };
}

// The run id of one recording: eight lowercase hex characters, the suffix the
// server keeps every file of the run under. One draw of Math.random is enough —
// the id is not a secret and only has to stay unique among the recordings of
// one session.
function newVoiceRunId() {
  return Math.floor(Math.random() * 2 ** 32).toString(16).padStart(8, '0');
}

async function startRecording(startRequest) {
  if (!startRequest) throw new Error('The voice button click did not provide its microphone request');
  const run = newVoiceRun(startRequest.sessionId, startRequest.backend, startRequest);
  activeVoiceRun = run;
  voiceInputBusy = true;
  run.stream = startRequest.stream;
  run.audioContext = startRequest.audioContext;
  run.startup.micOpenMs = startRequest.micOpenMs;
  run.startup.micPermission = startRequest.micPermission;
  // Amber from the click on: the mic is not delivering sound yet, so the
  // user must not speak. Red is the speak cue and lights at the first
  // chunk with a non-zero sample.
  setVoiceButtonState(VOICE_BUTTON_STATE.STARTING);
  updateVoiceCaret();
  if (run.backend.livePartials) openVoiceRelay(run);
  run.ui = ensureVoiceOverlay();
  showVoiceHint(run, VOICE_HINT_WAITING);

  const micBranch = tagVoiceStartupError(run, 'mic', (async () => {
    const result = await startRequest.micResultPromise;
    if (result.error) throw result.error;
    if (result.cancelled) return;
    run.stream = result.stream;
    run.startup.micOpenMs = startRequest.micOpenMs;
    // The recording's device facts: collection starts here, at recording
    // start, without delaying it — the end frame and the upload await the
    // settled object instead. The startup report reads the value only once
    // it exists.
    run.devicesPromise = collectVoiceDevices(run).then((devices) => {
      run.devices = devices;
      return devices;
    });
  })());
  const graphBranch = tagVoiceStartupError(run, 'graph', (async () => {
    const workletUrl = URL.createObjectURL(new Blob([VOICE_WORKLET_SOURCE], {type: 'application/javascript'}));
    try {
      await run.audioContext.audioWorklet.addModule(workletUrl);
    } finally {
      URL.revokeObjectURL(workletUrl);
    }
    run.startup.graphReadyMs = voiceStartupElapsed(run);
  })());

  try {
    await Promise.all([micBranch, graphBranch]);
    if (activeVoiceRun !== run) {
      sendVoiceStartup(run, 'left');
      abortVoiceRun(run);
      return;
    }

    // Both branches have settled: record the engine's state, then resume it once
    // if it is still suspended. A timed-out resume throws into the catch below.
    run.startup.audioContextSettledState = run.audioContext.state;
    if (run.audioContext.state === 'running') {
      run.startup.audioContextResume = 'not_needed';
    } else {
      await tagVoiceStartupError(run, 'graph', resumeVoiceContext(run));
    }

    run.sourceNode = run.audioContext.createMediaStreamSource(run.stream);
    run.workletNode = new AudioWorkletNode(run.audioContext, 'voice-capture', {
      numberOfInputs: 1,
      numberOfOutputs: 0,
      processorOptions: {
        inputSampleRate: run.audioContext.sampleRate,
        chunkSamples: VOICE_CHUNK_SAMPLES,
      },
    });
    run.workletNode.port.onmessage = (event) => handleVoiceWorkletMessage(run, event);
    run.sourceNode.connect(run.workletNode);

    run.recording = true;
    run.phase = VOICE_RUN_PHASE.RECORDING;
    // Both startup steps are done. The button now waits for the first chunk.
    run.startup.reached = 'no_frames';
    updateVoiceLeaveGuard();
    // Connected is not yet sounding: a just-opened device can deliver
    // seconds of digital zero, so the button stays amber and the hint waits
    // for the first non-zero sample.
    showVoiceHint(run, VOICE_HINT_WAITING);
    updateVoiceIndicator(run);
  } catch (err) {
    // The first rejection goes out now: the other branch may never settle, and
    // the wait below would hold the report forever.
    sendVoiceStartup(run, 'startup_error');
    if (activeVoiceRun !== run) {
      sendVoiceStartup(run, 'left');
      abortVoiceRun(run);
      return;
    }
    console.error('Voice input failed:', err);
    showToast('Voice input failed: ' + err.message, true);
    releaseVoiceRun(run, 'left');
  }
}

// Resumes a suspended audio engine for at most VOICE_CONTEXT_RESUME_BOUND_MS.
// The engine's state after the wait decides the outcome, so a resume that never
// settles and one that settles with the engine still suspended both time out.
async function resumeVoiceContext(run) {
  const context = run.audioContext;
  let timer = null;
  let stateChangeHandler = null;
  const running = new Promise((resolve) => {
    stateChangeHandler = () => {
      if (context.state === 'running') resolve();
    };
    context.addEventListener('statechange', stateChangeHandler);
    if (context.state === 'running') resolve();
  });
  const bound = new Promise((resolve) => {
    timer = setTimeout(resolve, VOICE_CONTEXT_RESUME_BOUND_MS);
  });
  try {
    context.resume().catch((err) => console.error('Voice audio engine resume failed:', err));
    await Promise.race([running, bound]);
  } finally {
    clearTimeout(timer);
    context.removeEventListener('statechange', stateChangeHandler);
  }
  if (context.state !== 'running') {
    run.startup.audioContextResume = 'timed_out';
    throw new Error('the audio engine did not start');
  }
  run.startup.audioContextResume = 'started';
}

// --- Recording devices ------------------------------------------------------
// One devices object per recording, collected when the mic opens. It lists
// which microphone captured it, the audio processing the track actually ran
// with, and the output devices outside sound could have bled in from. Exactly
// five keys — the server's voice_transcribed log schema.
async function collectVoiceDevices(run) {
  const track = run.stream.getAudioTracks()[0];
  const devices = {
    input_device: track.label,
    capture_settings: voiceCaptureSettings(track.getSettings()),
    output_device: null,
    communications_output_device: null,
    device_error: null,
  };
  try {
    const listed = await navigator.mediaDevices.enumerateDevices();
    devices.output_device = voiceOutputDeviceLabel(listed, 'default');
    devices.communications_output_device = voiceOutputDeviceLabel(listed, 'communications');
  } catch (err) {
    // Enumeration failing never stops the recording: the two output fields
    // stay null and the error rides the devices object into the server log.
    console.error('Voice device enumeration failed:', err);
    devices.device_error = err && err.message ? err.message : String(err);
  }
  return devices;
}

// The track's effective audio processing, with unreported settings as null so
// the object JSON-roundtrips with all four keys present.
function voiceCaptureSettings(settings) {
  const picked = {};
  for (const key of ['echoCancellation', 'noiseSuppression', 'autoGainControl', 'sampleRate']) {
    picked[key] = settings[key] === undefined ? null : settings[key];
  }
  return picked;
}

// The label of the audiooutput entry with this deviceId, null when there is
// none — 'communications' exists only on Windows Chrome.
function voiceOutputDeviceLabel(listed, deviceId) {
  const match = listed.find((info) => info.kind === 'audiooutput' && info.deviceId === deviceId);
  return match ? match.label : null;
}

// --- Startup report ---------------------------------------------------------
// One record per recording run keeps the click-time origin, startup steps and
// terminal result. Its flag makes later exits no-ops, so a session switch or
// page close after completion adds no second line. The server writes one line.

function newVoiceStartup(startRequest) {
  return {
    clickedAt: startRequest.clickedAt,
    reached: 'mic_opening',
    end: null,
    error: null,
    stopMs: null,
    textMs: null,
    chunkSentMs: [],
    partialShownMs: [],
    partialSamples: [],
    micOpenMs: null,
    graphReadyMs: null,
    audioContextSettledState: null,
    audioContextResume: null,
    audioContextState: null,
    audioContextRate: null,
    trackReadyState: null,
    trackMuted: null,
    firstFrameMs: null,
    firstSoundMs: null,
    frames: 0,
    micPermission: startRequest.micPermission,
    sent: false,
  };
}

// Milliseconds from the click, on the page's monotonic clock, which no clock
// adjustment moves.
function voiceStartupElapsed(run) {
  return Math.max(0, Math.round(performance.now() - run.startup.clickedAt));
}

// A failed startup step's error text: its name when that name says something
// (NotAllowedError), else its message.
function voiceStartupErrorText(err) {
  const message = err && err.message ? err.message : String(err);
  const name = err && err.name ? err.name : '';
  return name && name !== 'Error' ? name + ': ' + message : message;
}

// Tags a startup branch's first rejection with the step it came from, so the
// catch reports `mic: ...` or `graph: ...` without waiting for both branches.
function tagVoiceStartupError(run, step, promise) {
  return promise.catch((err) => {
    if (run.startup.error === null) run.startup.error = step + ': ' + voiceStartupErrorText(err);
    throw err;
  });
}

// The body of one voice_startup line: the run's startup record plus the audio
// and microphone state at this moment. Devices rides only once the collection
// has settled, and the send never waits for it.
function voiceStartupBody(run) {
  const startup = run.startup;
  const context = run.audioContext;
  const track = run.stream ? run.stream.getAudioTracks()[0] : null;
  return {
    run_id: run.runId,
    backend: run.backend.id,
    reached: startup.reached,
    end: startup.end,
    error: startup.error,
    mic_open_ms: startup.micOpenMs,
    graph_ready_ms: startup.graphReadyMs,
    first_frame_ms: startup.firstFrameMs,
    first_sound_ms: startup.firstSoundMs,
    end_ms: voiceStartupElapsed(run),
    stop_ms: startup.stopMs,
    text_ms: startup.textMs,
    chunk_sent_ms: startup.chunkSentMs,
    partial_shown_ms: startup.partialShownMs,
    partial_samples: startup.partialSamples,
    frames: startup.frames,
    samples: Math.min(run.totalSamples, VOICE_MAX_SAMPLES),
    audio_context_state: startup.audioContextState || (context ? context.state : null),
    audio_context_rate: startup.audioContextRate || (context ? context.sampleRate : null),
    audio_context_settled_state: startup.audioContextSettledState,
    audio_context_resume: startup.audioContextResume,
    mic_permission: startup.micPermission,
    track_ready_state: startup.trackReadyState || (track ? track.readyState : null),
    track_muted: startup.trackMuted === null ? (track ? track.muted : null) : startup.trackMuted,
    devices: run.devices,
    user_agent: navigator.userAgent,
    client_ts: new Date().toISOString(),
  };
}

// The one send: the record's flag makes every call after the first a no-op, so
// each run writes exactly one line. Keepalive lets a page-close report outlive
// the page, and only a report from a live page names its own failure.
function sendVoiceStartup(run, end) {
  const startup = run.startup;
  if (startup.sent) return;
  startup.sent = true;
  if (startup.end === null || end === 'page_closed') startup.end = end;
  const report = end !== 'page_closed';
  fetch(`/api/voice/${encodeURIComponent(run.sessionId)}/startup`, {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(voiceStartupBody(run)),
    keepalive: true,
    credentials: 'same-origin',
  }).then((response) => {
    if (report && !response.ok) console.error('Voice startup report failed:', response.status);
  }).catch((err) => {
    if (report) console.error('Voice startup report failed:', err);
  });
}

// A page close, reload or navigation during a run: the report goes out from
// this handler so keepalive can carry it after the document is gone.
function onVoicePageHide() {
  if (activeVoiceRun) sendVoiceStartup(activeVoiceRun, 'page_closed');
}

function handleVoiceWorkletMessage(run, event) {
  if (activeVoiceRun !== run) return;
  const data = event.data || {};
  if (data.type === 'flushed') {
    const resolve = run.flushResolvers.get(data.id);
    if (resolve) {
      run.flushResolvers.delete(data.id);
      resolve();
    }
    return;
  }
  if (data.type !== 'pcm') return;
  // Chunks keep appending through the stop flush (a transfer in flight when the
  // click lands would otherwise lose its samples). Trigger checks stay live only
  // while actually recording.
  if (!run.recording && !run.stopping) return;
  const samples = new Int16Array(data.buffer);
  run.pcmChunks.push(data.buffer);
  run.totalSamples += samples.length;
  if (run.startup.firstFrameMs === null) {
    run.startup.firstFrameMs = voiceStartupElapsed(run);
    run.startup.reached = 'silent';
  }
  run.startup.frames += 1;
  const wasReady = run.signalReady;
  const levels = updateVoiceLevel(run, samples);
  updateVoiceIndicator(run);
  sendVoiceRelayAudio(run, data.buffer);
  if (run.recording) {
    // The ready signal: sound is arriving, so the button may go red — the
    // speak cue. One non-zero sample is enough. A level threshold would keep
    // a quiet room amber forever. The flag ends the inspection for the rest
    // of the run.
    if (!run.signalReady) {
      for (let i = 0; i < samples.length; i++) {
        if (samples[i] === 0) continue;
        run.signalReady = true;
        run.startup.firstSoundMs = voiceStartupElapsed(run);
        run.startup.reached = 'ready';
        setVoiceButtonState(VOICE_BUTTON_STATE.RECORDING);
        showVoiceHint(run, voiceReadyHint(run));
        break;
      }
    }
    if (wasReady) {
      if (run.quietWarmupChunks < VOICE_SILENCE_WARMUP_CHUNKS) {
        run.quietWarmupChunks += 1;
      } else if (levels.nonZero && levels.rms * 32768 < VOICE_SILENCE_THRESHOLD) {
        run.quietChunks += 1;
        if (run.quietChunks >= VOICE_SILENCE_CHUNKS) stopRecording(run, 'mic_silent');
      } else {
        run.quietChunks = 0;
      }
    }
    if (run.totalSamples >= VOICE_MAX_SAMPLES) stopRecording(run, 'stop'); // auto-stop enters the stop flow
  }
}

function flushVoiceWorklet(run) {
  if (!run.workletNode) return Promise.resolve();
  const id = ++run.flushId;
  return new Promise((resolve) => {
    run.flushResolvers.set(id, resolve);
    run.workletNode.port.postMessage({type: 'flush', id});
    setTimeout(() => {
      const pending = run.flushResolvers.get(id);
      if (pending) {
        run.flushResolvers.delete(id);
        pending();
      }
    }, 500);
  });
}

async function stopRecording(run, end) {
  if (!run.recording) return;
  run.recording = false;
  run.stopping = true;
  run.startup.stopMs = voiceStartupElapsed(run);
  run.startup.end = end;
  setVoiceButtonState(VOICE_BUTTON_STATE.IDLE);
  updateVoiceMicTitle();
  updateVoiceLeaveGuard();
  if (end === 'mic_silent') {
    showVoiceHint(run, VOICE_HINT_MIC_SILENT);
    showToast(VOICE_HINT_MIC_SILENT, true);
  }
  try {
    // The worklet drains its buffered tail into the local buffer — and, live,
    // out to the relay — so the recording is whole before it ends. The upload
    // (after a stop-state click, or right away on a non-live backend)
    // assembles its WAV from the same buffer.
    await flushVoiceWorklet(run);
    if (activeVoiceRun !== run) return;
    cleanupVoiceCapture(run);
    // A live backend gets the length-scaled final budget here. The leave guard
    // stays engaged through the wait because run.stopping has not cleared yet.
    const finalText = run.relaySocket ? await collectVoiceRelayFinal(run) : null;
    if (activeVoiceRun !== run) return;
    run.stopping = false;
    if (finalText !== null) {
      // The relay's final means the recording is already archived server-side.
      // The words go in through the one insertion path and the run ends with
      // no upload, so the send lock never engages.
      finishWithRelayFinal(run, finalText);
      return;
    }
    if (run.backend.livePartials) {
      // No final from a live relay: the budget expired (the relay is still
      // open) or a relay failure was recorded earlier. No upload starts from
      // here. The run stops visibly and one click starts the upload.
      enterVoiceStopState(run);
      return;
    }
    startUpload(run);
  } catch (err) {
    console.error('Voice stop failed:', err);
    showToast('Voice input failed: ' + err.message, true);
    releaseVoiceRun(run, run.startup.end || 'stop');
  }
}

// The relay's final arrives inside the budget, or after it through the stop
// state's still-open relay. The recording is already archived, so the words go
// in and the run ends with nothing uploaded. An empty final means the recording
// held no speech.
function finishWithRelayFinal(run, finalText) {
  const words = finalText.trim();
  if (!words) {
    showToast('No speech detected');
    releaseVoiceRun(run, run.startup.end || 'stop');
    return;
  }
  insertVoiceText(run, words);
  run.startup.textMs = voiceStartupElapsed(run);
  releaseVoiceRun(run, run.startup.end || 'stop');
}

// The stop state: a live recording is over, no final owns the words, and no
// upload may start on its own. The buffer stays and the hint names the backend
// and the reason. One click on the voice button (the toggleVoice stop branch)
// closes the relay and starts the upload. While the relay is
// still open, the budget-expiry case, a final arriving before that click ends
// the run through finishWithRelayFinal instead. The phase also keeps the leave
// guard engaged: the recording exists only in this browser until the click.
function enterVoiceStopState(run) {
  run.phase = VOICE_RUN_PHASE.STOP;
  updateVoiceLeaveGuard();
  showVoiceHint(run, run.relayFailure !== null
      ? voiceRelayFailureHint(run, run.relayFailure)
      : voiceRelayBudgetHint(run));
}

async function startUpload(run) {
  // The devices object settled at recording start rides the form. Awaiting it
  // here is the upload waiting for the enumeration, never the reverse.
  const devices = await run.devicesPromise;
  if (activeVoiceRun !== run) return;
  run.phase = VOICE_RUN_PHASE.UPLOADING;
  run.uploadTimedOut = false;
  run.requestInFlight = true;
  // The transcription window (upload + decode) opens here: it counts toward
  // the send-button lock. The recording before it never did.
  inputWorkStarted(VOICE_TRANSCRIBE_IN_FLIGHT_MESSAGE);
  updateVoiceLeaveGuard();
  showVoiceHint(run, VOICE_HINT_UPLOADING);
  run.uploadTimer = setTimeout(() => {
    run.uploadTimer = null;
    if (activeVoiceRun !== run || !run.xhr) return;
    run.uploadTimedOut = true;
    run.xhr.abort();
  }, VOICE_UPLOAD_TIMEOUT_MS);
  const wav = assembleVoiceWav(run.pcmChunks, Math.min(run.totalSamples, VOICE_MAX_SAMPLES));
  postVoiceRecording(`/api/voice/${encodeURIComponent(run.sessionId)}`, wav, {
    runId: run.runId,
    backendId: run.backend.id,
    devices,
    onXhr: (xhr) => { run.xhr = xhr; },
    onSent: () => {
      if (activeVoiceRun !== run) return;
      clearVoiceUploadTimer(run);
      run.phase = VOICE_RUN_PHASE.DECODING;
      showVoiceHint(run, VOICE_HINT_DECODING);
    },
  }).then((text) => {
    if (activeVoiceRun !== run) return;
    applyVoiceFinal(run, text);
  }).catch((err) => {
    if (activeVoiceRun !== run) return;
    failVoiceUpload(run, err);
  });
}

function failVoiceUpload(run, err) {
  clearVoiceUploadTimer(run);
  run.xhr = null;
  run.requestInFlight = false;
  // The window closes with the failure. A retry re-enters startUpload and
  // opens a fresh one.
  inputWorkSettled(VOICE_TRANSCRIBE_IN_FLIGHT_MESSAGE);
  run.phase = VOICE_RUN_PHASE.RETRY; // the buffer stays; the next click re-uploads it
  updateVoiceLeaveGuard();
  const message = run.uploadTimedOut ? 'Voice upload timed out' : err.message;
  console.warn('Voice upload failed:', err);
  showToast(message, true);
  showVoiceHint(run, VOICE_HINT_RETRY);
}

function cancelVoiceUpload(run) {
  // Explicit user cancel: the only failure-free path that drops the buffer.
  discardVoiceRecording(run, 'Voice upload canceled');
}

function applyVoiceFinal(run, fullText) {
  clearVoiceUploadTimer(run);
  run.xhr = null;
  run.requestInFlight = false;
  // The window closes once the decode lands. The run's xhr is already null, so
  // the release's abort below settles nothing further.
  inputWorkSettled(VOICE_TRANSCRIBE_IN_FLIGHT_MESSAGE);
  const words = fullText.trim();
  if (!words) {
    showToast('No speech detected');
    releaseVoiceRun(run, run.startup.end || 'stop');
    return;
  }
  insertVoiceText(run, words);
  run.startup.textMs = voiceStartupElapsed(run);
  releaseVoiceRun(run, run.startup.end || 'stop');
}

function insertVoiceText(run, text) {
  // The one text-insertion path for recognized voice words: append to the input,
  // mark the message as voice-contributed, and restore the caret. Returns the
  // start offset of the inserted span.
  const input = document.getElementById('msg-input');
  const current = input.value.trim();
  const merged = mergeVoiceText(run.lastPartial, text).trim();
  input.value = current ? current + ' ' + merged : merged;
  Chat.setInputMode(VOICE_INPUT_MODE);
  autoResize(input);
  saveDraft();
  input.focus();
  return input.value.length - merged.length;
}

// Match the visible preview to the backend final. A matching token anchors
// its surrounding span. A preview-only span survives while a final-only span
// wins when the backend rewrote those words.
function mergeVoiceText(partialText, finalText) {
  const tokenPattern = /[A-Za-z0-9_]+(?:['.-][A-Za-z0-9_]+)*|\p{Script=Han}/gu;
  const tokenize = (text) => Array.from(text.matchAll(tokenPattern), (match) => ({
    text: match[0],
    key: match[0].toLocaleLowerCase('en-US'),
    start: match.index,
    end: match.index + match[0].length,
  }));
  const partial = tokenize(partialText);
  const final = tokenize(finalText);
  if (!partial.length || !final.length) return partial.length ? partialText : finalText;

  const lengths = Array.from({length: partial.length + 1}, () => new Uint32Array(final.length + 1));
  for (let i = partial.length - 1; i >= 0; i--) {
    for (let j = final.length - 1; j >= 0; j--) {
      lengths[i][j] = partial[i].key === final[j].key
        ? lengths[i + 1][j + 1] + 1
        : Math.max(lengths[i + 1][j], lengths[i][j + 1]);
    }
  }

  const matches = [];
  let i = 0;
  let j = 0;
  while (i < partial.length && j < final.length) {
    if (partial[i].key === final[j].key) {
      matches.push([i++, j++]);
    } else if (lengths[i][j + 1] >= lengths[i + 1][j]) {
      // On a tie keep the earlier preview token available to match.
      j += 1;
    } else {
      i += 1;
    }
  }

  const insertions = [];
  let previousPartial = -1;
  let previousFinal = -1;
  for (const [matchedPartial, matchedFinal] of [...matches, [partial.length, final.length]]) {
    const partialStart = previousPartial + 1;
    const finalStart = previousFinal + 1;
    const partialCount = matchedPartial - partialStart;
    const finalCount = matchedFinal - finalStart;
    if (partialCount > 0 && finalCount === 0) {
      const start = partial[partialStart].start;
      const end = partial[matchedPartial - 1].end;
      const offset = matchedFinal < final.length
        ? final[matchedFinal].start
        : final[final.length - 1].end;
      insertions.push({offset, text: partialText.slice(start, end)});
    }
    previousPartial = matchedPartial;
    previousFinal = matchedFinal;
  }

  let merged = finalText;
  for (const insertion of insertions.sort((left, right) => right.offset - left.offset)) {
    const before = merged[insertion.offset - 1] || '';
    const after = merged[insertion.offset] || '';
    const first = insertion.text[0] || '';
    const last = insertion.text[insertion.text.length - 1] || '';
    const asciiWord = (character) => /[A-Za-z0-9_]/.test(character);
    const prefix = asciiWord(before) && asciiWord(first) ? ' ' : '';
    const suffix = asciiWord(last) && asciiWord(after) ? ' ' : '';
    merged = merged.slice(0, insertion.offset) + prefix + insertion.text + suffix + merged.slice(insertion.offset);
  }
  return merged;
}

// --- WAV assembly -----------------------------------------------------------
// The buffer is a list of int16-LE ArrayBuffers in capture order. The WAV is
// built once at upload from the first sampleCount samples (the auto-stop trim).

function assembleVoiceWav(chunks, sampleCount) {
  const bytes = new Uint8Array(44 + sampleCount * 2);
  writeWavHeader(new DataView(bytes.buffer), sampleCount);
  let offset = 44;
  let remaining = sampleCount;
  for (const chunk of chunks) {
    if (remaining <= 0) break;
    const source = new Uint8Array(chunk);
    const take = Math.min(source.length, remaining * 2);
    bytes.set(source.subarray(0, take), offset);
    offset += take;
    remaining -= take / 2;
  }
  return bytes.buffer;
}

function writeWavHeader(view, sampleCount) {
  // Canonical PCM16 mono 16 kHz WAV header, the one format the server accepts.
  const writeStr = (offset, text) => {
    for (let i = 0; i < text.length; i++) view.setUint8(offset + i, text.charCodeAt(i));
  };
  writeStr(0, 'RIFF');
  view.setUint32(4, 36 + sampleCount * 2, true);
  writeStr(8, 'WAVE');
  writeStr(12, 'fmt ');
  view.setUint32(16, 16, true); // fmt chunk size
  view.setUint16(20, 1, true); // PCM
  view.setUint16(22, 1, true); // mono
  view.setUint32(24, VOICE_SAMPLE_RATE, true);
  view.setUint32(28, VOICE_SAMPLE_RATE * 2, true); // byte rate
  view.setUint16(32, 2, true); // block align
  view.setUint16(34, 16, true); // bits per sample
  writeStr(36, 'data');
  view.setUint32(40, sampleCount * 2, true);
}

// --- Upload transport -------------------------------------------------------

function sendVoiceXhr(path, body, {onSent, onXhr}) {
  // XHR, not fetch: the two-state progress needs upload.onload, the one event
  // that fires when the body is fully sent, to switch Uploading -> Decoding.
  // XHR gets no auth from the fetch wrapper, so the header goes on explicitly.
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    if (onXhr) onXhr(xhr);
    xhr.open('POST', path);
    const authorization = accessTokenAuthorization();
    if (authorization) xhr.setRequestHeader('Authorization', authorization);
    xhr.responseType = 'json';
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve(xhr.response && typeof xhr.response.text === 'string' ? xhr.response.text : '');
      } else {
        reject(new Error((xhr.response && xhr.response.error) || `Voice request failed (HTTP ${xhr.status})`));
      }
    };
    xhr.onerror = () => reject(new Error('Voice request failed: network error'));
    xhr.onabort = () => reject(new Error('Voice upload canceled'));
    xhr.upload.onload = () => {
      if (onSent) onSent();
    };
    xhr.send(body);
  });
}

// The upload's transport: a multipart form with the audio plus the run id,
// the backend the browser selected and the recording's device object as JSON.
// The server keeps the recording under the run id's stem, transcribes it, and
// returns the text. A null backendId or devices leaves that form field out.
function postVoiceRecording(path, wavBuffer, {runId, backendId, devices, ...opts}) {
  const form = new FormData();
  form.append('audio', new Blob([wavBuffer]), 'recording.wav');
  form.append('run_id', runId);
  if (backendId !== null) form.append('backend', backendId);
  if (devices !== null) form.append('devices', JSON.stringify(devices));
  return sendVoiceXhr(path, form, opts);
}

function clearVoiceUploadTimer(run) {
  if (run.uploadTimer !== null) {
    clearTimeout(run.uploadTimer);
    run.uploadTimer = null;
  }
}

// --- Preview relay -----------------------------------------------------------
// One live recording's channel to /ws/voice/{session}: binary chunks out,
// partial/final/error frames in. Receiving the final means the relay archived
// the recording it already received, so the run ends and nothing uploads. A
// relay failure, an error frame, a lost socket, or a failed end send, never
// starts an upload. During recording the reason lands on the run and in the
// hint at once while the recording continues. At stop the run enters the stop
// state, where a failure only rewrites the hint reason.

function openVoiceRelay(run) {
  const socket = new WebSocket(wsUrlWithToken(
      `/ws/voice/${encodeURIComponent(run.sessionId)}?backend=${encodeURIComponent(run.backend.id)}` +
      `&run_id=${encodeURIComponent(run.runId)}`));
  run.relaySocket = socket;
  socket.onopen = () => {
    if (activeVoiceRun !== run) return;
    // Chunks captured while the handshake ran go out first, in capture order.
    for (const chunk of run.pcmChunks) sendVoiceRelayAudio(run, chunk);
  };
  socket.onmessage = (event) => handleVoiceRelayMessage(run, event);
  socket.onerror = () => failVoiceRelay(run, 'connection lost');
  socket.onclose = () => failVoiceRelay(run, 'connection lost');
}

function sendVoiceRelayAudio(run, chunk) {
  const socket = run.relaySocket;
  // Chunks captured before the socket opens ride the open event's replay.
  if (!socket || socket.readyState !== VOICE_WS_OPEN) return;
  try {
    socket.send(chunk);
    run.startup.chunkSentMs.push(voiceStartupElapsed(run));
  } catch (err) {
    console.warn('Voice relay send failed:', err);
    failVoiceRelay(run, 'connection lost');
  }
}

function handleVoiceRelayMessage(run, event) {
  if (activeVoiceRun !== run) return;
  let message;
  try {
    message = JSON.parse(event.data);
  } catch (err) {
    console.warn('Voice preview frame was not JSON:', err);
    failVoiceRelay(run, 'bad frame');
    return;
  }
  if (message.type === 'partial') {
    if (!Number.isSafeInteger(message.samples) || message.samples < 0) {
      console.warn('Voice preview partial had an invalid sample count');
      failVoiceRelay(run, 'bad partial frame');
      return;
    }
    showVoicePartial(run, message.text, message.samples);
    return;
  }
  if (message.type === 'final') {
    // The final ends the run wherever it can still matter. Inside the budget it
    // arrives through the wait's resolve. In the stop state (the budget expired,
    // the relay stayed open) it arrives through the one insertion path. With
    // neither, after the click that started the upload or after the release,
    // the socket is already down and the late final is discarded.
    if (run.relayFinalResolve) {
      settleVoiceRelayWait(run, message.text);
      return;
    }
    if (run.phase === VOICE_RUN_PHASE.STOP) finishWithRelayFinal(run, message.text);
    return;
  }
  if (message.type === 'error') {
    console.warn('Voice preview failed:', message.message);
    failVoiceRelay(run, message.message);
    return;
  }
  console.warn('Voice preview sent an unknown frame type:', message.type);
  failVoiceRelay(run, 'unknown frame type: ' + message.type);
}

// The final budget one recording earns: the base plus the per-second rate for
// every recorded second, from the sample count the run accumulated. The math
// stays in integers so a 201.6 s recording budgets exactly 6032 ms.
function voiceRelayFinalBudgetMs(run) {
  return VOICE_RELAY_FINAL_BUDGET_BASE_MS +
      (VOICE_RELAY_FINAL_BUDGET_PER_SECOND_MS * run.totalSamples) / VOICE_SAMPLE_RATE;
}

// The stop state's two hints: a relay failure names what broke, a spent budget
// says the run is still waiting and offers the upload.
function voiceRelayFailureHint(run, reason) {
  return run.backend.label + ': ' + reason + '. Click to transcribe.';
}

function voiceRelayBudgetHint(run) {
  const seconds = (voiceRelayFinalBudgetMs(run) / 1000).toFixed(1);
  return run.backend.label + ': no final after ' + seconds + ' s, still waiting. Click to transcribe.';
}

function voiceReadyHint(run) {
  // A relay that already failed keeps its reason up. The ready cue must not
  // overwrite it.
  if (run && run.relayFailure !== null) return voiceRelayFailureHint(run, run.relayFailure);
  return VOICE_HINT_LISTENING;
}

async function collectVoiceRelayFinal(run) {
  // Seal the relay's recording and wait out the final for the budget the
  // recording's length earns. Null means the budget expired with the relay
  // still open, and the stop state keeps waiting for it. The end frame carries
  // the devices object collected at recording start.
  const devices = await run.devicesPromise;
  const socket = run.relaySocket;
  try {
    socket.send(JSON.stringify({type: 'end', devices}));
  } catch (err) {
    // The relay died between the last chunk and the stop: no final is coming,
    // and the stop state owns the recording until a click uploads it.
    console.warn('Voice relay end send failed:', err);
    failVoiceRelay(run, 'connection lost');
    return null;
  }
  return new Promise((resolve) => {
    run.relayFinalResolve = resolve;
    run.relayWaitTimer = setTimeout(() => {
      run.relayWaitTimer = null;
      settleVoiceRelayWait(run, null);
    }, voiceRelayFinalBudgetMs(run));
  });
}

// Settle the final wait with *text* (or null at budget expiry): resolve the
// waiter and disarm the timer. The socket's fate belongs to the callers, the
// release path closing it behind a final and the stop state keeping it open
// for one.
function settleVoiceRelayWait(run, text) {
  const resolve = run.relayFinalResolve;
  run.relayFinalResolve = null;
  if (run.relayWaitTimer !== null) {
    clearTimeout(run.relayWaitTimer);
    run.relayWaitTimer = null;
  }
  if (resolve) resolve(text);
}

// A relay failure, an error frame, a lost socket, or a failed end send. The
// reason lands on the run and in the hint at once, and the socket goes down.
// No upload starts here. During recording the recording just continues, and at
// stop the run is on its way into (or already sits in) the stop state. There
// the failure only rewrites the hint reason.
function failVoiceRelay(run, reason) {
  closeVoiceRelay(run);
  run.relayFailure = reason;
  showVoiceHint(run, voiceRelayFailureHint(run, reason));
}

function closeVoiceRelay(run) {
  const socket = run.relaySocket;
  run.relaySocket = null;
  if (run.relayFinalResolve) settleVoiceRelayWait(run, null);
  if (!socket) return;
  socket.onopen = null;
  socket.onmessage = null;
  socket.onerror = null;
  socket.onclose = null;
  try {
    socket.close();
  } catch (err) {
    console.warn('Voice relay close failed:', err);
  }
}

// --- Indicators -------------------------------------------------------------

function ensureVoiceOverlay() {
  const input = document.getElementById('msg-input');
  if (!input || !input.parentElement) return null;
  let overlay = document.getElementById('voice-partial-overlay');
  if (overlay) return collectVoiceOverlay(overlay);
  overlay = document.createElement('div');
  overlay.id = 'voice-partial-overlay';
  overlay.className = VOICE_OVERLAY_CLASS;
  const partial = document.createElement('div');
  partial.className = VOICE_PARTIAL_CLASS;
  overlay.appendChild(partial);
  const bar = document.createElement('div');
  bar.className = 'voice-level-bar';
  const fill = document.createElement('div');
  fill.className = 'voice-level-fill';
  bar.appendChild(fill);
  const meta = document.createElement('div');
  meta.className = 'voice-meta';
  const timer = document.createElement('span');
  timer.className = 'voice-timer';
  const hint = document.createElement('span');
  hint.className = 'voice-hint';
  meta.appendChild(timer);
  meta.appendChild(hint);
  overlay.appendChild(bar);
  overlay.appendChild(meta);
  input.parentElement.appendChild(overlay);
  return collectVoiceOverlay(overlay);
}

function collectVoiceOverlay(overlay) {
  // children is a live HTMLCollection, not an array: go through Array.from.
  const byClass = (name) => Array.from(overlay.children).find((child) => child.className === name) || null;
  const bar = byClass('voice-level-bar');
  const meta = byClass('voice-meta');
  return {
    partial: byClass(VOICE_PARTIAL_CLASS),
    fill: bar ? Array.from(bar.children).find((child) => child.className === 'voice-level-fill') : null,
    timer: meta ? Array.from(meta.children).find((child) => child.className === 'voice-timer') : null,
    hint: meta ? Array.from(meta.children).find((child) => child.className === 'voice-hint') : null,
  };
}

function updateVoiceIndicator(run) {
  if (!run.ui) return;
  const seconds = Math.floor(run.totalSamples / VOICE_SAMPLE_RATE);
  if (run.ui.timer) run.ui.timer.textContent = Math.floor(seconds / 60) + ':' + String(seconds % 60).padStart(2, '0');
}

function showVoiceHint(run, text) {
  if (run.ui && run.ui.hint) run.ui.hint.textContent = text;
}

function showVoicePartial(run, text, samples) {
  // Keep the latest grey preview for the final merge and scroll it into view.
  run.lastPartial = text;
  run.startup.partialShownMs.push(voiceStartupElapsed(run));
  run.startup.partialSamples.push(samples);
  if (run.ui && run.ui.partial) {
    run.ui.partial.textContent = text;
    run.ui.partial.scrollTop = run.ui.partial.scrollHeight;
  }
}

function updateVoiceLevel(run, samples) {
  // Per-chunk RMS mapped onto the bar, with a decay so silence falls instead of
  // snapping: pure client-side, no server round-trip.
  let sum = 0;
  let nonZero = false;
  for (let i = 0; i < samples.length; i++) {
    if (samples[i] !== 0) nonZero = true;
    const v = samples[i] / 32768;
    sum += v * v;
  }
  const rms = Math.sqrt(sum / Math.max(samples.length, 1));
  const level = Math.min(100, Math.round(rms * 400));
  run.level = Math.max(level, Math.round(run.level * 0.8));
  if (run.ui && run.ui.fill) run.ui.fill.style.width = run.level + '%';
  return {rms, nonZero};
}

// --- Page-leave guard -------------------------------------------------------
// Engaged while recording (including the stop flush), while a request is in
// flight, or while a stopped live recording sits in the stop state. In the stop
// state the buffer exists only in this browser until its click. Disengaged when
// idle. The popstate SPA branch in app.js is untouched: this is the generic
// page-leave path, so a reload cannot silently drop a recording.
let voiceLeaveGuardInstalled = false;

function onVoiceLeaveGuard(event) {
  event.preventDefault();
  event.returnValue = '';
}

function updateVoiceLeaveGuard() {
  const busy = activeVoiceRun && (activeVoiceRun.recording || activeVoiceRun.stopping ||
      activeVoiceRun.requestInFlight || activeVoiceRun.phase === VOICE_RUN_PHASE.STOP);
  if (busy && !voiceLeaveGuardInstalled) {
    window.addEventListener('beforeunload', onVoiceLeaveGuard);
    voiceLeaveGuardInstalled = true;
  } else if (!busy && voiceLeaveGuardInstalled) {
    window.removeEventListener('beforeunload', onVoiceLeaveGuard);
    voiceLeaveGuardInstalled = false;
  }
}

// --- Teardown ---------------------------------------------------------------

function resetVoiceState() {
  // Teardown takes the run out of the slot first, then aborts it: pending
  // continuations die on the ownership check and no request outlives the view.
  const run = activeVoiceRun;
  activeVoiceRun = null;
  if (run) {
    sendVoiceStartup(run, run.startup.end || 'left');
    abortVoiceRun(run);
  }
  voiceInputBusy = false;
  setVoiceButtonState(VOICE_BUTTON_STATE.IDLE);
  updateVoiceMicTitle();
  removeVoiceOverlay();
  updateVoiceLeaveGuard();
  updateVoiceCaret();
  Chat.setInputMode(null);
}

function discardVoiceRecording(run, message) {
  releaseVoiceRun(run, 'left');
  if (message) showToast(message, true);
}

// End-of-run release for the run that owns the slot: tear down its resources,
// free the slot, and reset the button and overlay. Callers add any toast.
function releaseVoiceRun(run, end) {
  sendVoiceStartup(run, run.startup.end || end);
  abortVoiceRun(run);
  if (activeVoiceRun === run) activeVoiceRun = null;
  voiceInputBusy = false;
  clearVoiceUploadTimer(run);
  setVoiceButtonState(VOICE_BUTTON_STATE.IDLE);
  updateVoiceMicTitle();
  removeVoiceOverlay();
  updateVoiceLeaveGuard();
  updateVoiceCaret();
}

// Tear down every resource a run owns: capture graph, mic stream, and any
// in-flight upload. Idempotent, so an ownership-lost continuation and the
// teardown path can both call it. The later call only sees nulls.
function abortVoiceRun(run) {
  run.startRequest.cancelled = true;
  cleanupVoiceCapture(run);
  closeVoiceRelay(run);
  abortVoiceUpload(run);
}

function abortVoiceUpload(run) {
  clearVoiceUploadTimer(run);
  const xhr = run.xhr;
  run.xhr = null;
  if (!xhr) return;
  xhr.abort();
  // An xhr still held here means its startUpload window never settled. The
  // abort's own callbacks die on the ownership guard, so the cancel and the
  // session-switch teardown settle the count at this one spot.
  inputWorkSettled(VOICE_TRANSCRIBE_IN_FLIGHT_MESSAGE);
}

function cleanupVoiceCapture(run) {
  if (run.sourceNode) {
    try { run.sourceNode.disconnect(); } catch (err) { console.warn('Voice source disconnect failed:', err); }
    run.sourceNode = null;
  }
  if (run.workletNode) {
    run.workletNode.port.onmessage = null;
    try { run.workletNode.disconnect(); } catch (err) { console.warn('Voice worklet disconnect failed:', err); }
    run.workletNode = null;
  }
  if (run.audioContext) {
    const ctx = run.audioContext;
    run.startup.audioContextState = ctx.state;
    run.startup.audioContextRate = ctx.sampleRate;
    run.audioContext = null;
    ctx.close().catch((err) => console.warn('Voice audio context close failed:', err));
  }
  if (run.stream) {
    const tracks = run.stream.getTracks();
    const audioTrack = run.stream.getAudioTracks()[0];
    if (audioTrack) {
      run.startup.trackReadyState = audioTrack.readyState;
      run.startup.trackMuted = audioTrack.muted;
    }
    tracks.forEach((track) => track.stop());
    run.stream = null;
  }
  run.flushResolvers.forEach((resolve) => resolve());
  run.flushResolvers.clear();
}

function removeVoiceOverlay() {
  document.getElementById('voice-partial-overlay')?.remove();
}

// The page-close report listens from here, before any run exists. The mic's
// tooltip, the composer's ⋯ menu entry and the dictated-message badge belong
// to the page's voice-events part, which runs before this file loads.
window.addEventListener('pagehide', onVoicePageHide);

// A run ends with the session view it belongs to.
document.addEventListener('charliebot:session-view-teardown', resetVoiceState);

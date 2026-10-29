// ---------------------------------------------------------------------------
// Voice input: pick a transcription backend, record locally, end by relay or upload
// ---------------------------------------------------------------------------
// One recording = one run object owning its mic stream, capture graph, local PCM
// buffer, and at most one in-flight upload request. A single module-level slot
// (`activeVoiceRun`) is claimed synchronously inside the click handler, so two
// pipelines can never coexist: every asynchronous continuation of a run first
// checks that the slot still belongs to it and silently exits otherwise.
//
// The recording itself never touches the persistence path: the worklet's 16 kHz
// PCM chunks append to a local buffer, and the level bar plus elapsed timer are
// computed from those chunks client-side. On release the whole buffer assembles
// into a WAV and uploads once; the server persists it and returns the full text.
// Every failure keeps the buffer and offers retry; only success or an explicit
// cancel discards it.
//
// The backend comes from the caret menu beside the microphone (the choices ride
// the page as VOICE_BACKENDS). With a backend whose livePartials is true the
// same chunks also stream to the server's preview relay (/ws/voice/{session}),
// which forwards the backend's partials while speaking and keeps the audio; on
// stop the run waits for the relay's final inside a budget that grows with the
// recording (2 s plus 20 ms per recorded second, because the backend needs
// longer to finalize longer recordings). Receiving the final means the relay
// has already archived the recording: the words go in and the run ends with no
// upload. A live recording that ends with no final never uploads on its own:
// the run enters the stop state, where the buffer stays, the hint names the
// backend and the reason, and one click on the voice button starts the upload
// whose transcript owns the text. The budget expiry itself is not a failure:
// the relay stays open, so a final arriving before that click still wins and
// ends the run with no upload. A relay failure (an error frame, a lost socket,
// a failed end send) never starts an upload either: during recording the
// reason lands in the hint at once and the recording continues; at stop the
// run enters the stop state. On a non-live backend the whole buffer assembles
// into a WAV and uploads once, right after stop; the server then archives the
// recording and returns the full text. No request goes out mid-recording.
let activeVoiceRun = null;

const VOICE_SAMPLE_RATE = 16000;
const VOICE_CHUNK_SAMPLES = 2048; // 128 ms of audio per worklet chunk
const VOICE_MAX_SAMPLES = 5 * 60 * VOICE_SAMPLE_RATE; // the server's recording cap
// The upload-phase budget: past it the request aborts into the retry state. The
// decode wait after the body is sent is unbounded but visible ("Decoding...").
const VOICE_UPLOAD_TIMEOUT_MS = 60 * 1000;
// The send gate's message while a transcription window (upload + decode) is open.
const VOICE_TRANSCRIBE_IN_FLIGHT_MESSAGE = 'Please wait for the voice transcription to finish';
// The input_mode a dictated message carries (src/features/voice/turn_contribution.py VOICE_INPUT_MODE).
const VOICE_INPUT_MODE = 'voice';
// The preview relay's final budget after the end frame: a base plus 20 ms per
// recorded second. The backend needs longer to finalize longer recordings, so
// the budget grows with the run; a fixed wait is either too short for long
// recordings or too long for short ones.
const VOICE_RELAY_FINAL_BUDGET_BASE_MS = 2 * 1000;
const VOICE_RELAY_FINAL_BUDGET_PER_SECOND_MS = 20;
// Where the caret's choice lives; the server default backs an unknown or
// unavailable stored id.
const VOICE_BACKEND_STORAGE_KEY = 'charliebot-voice-backend';
// WebSocket readyState OPEN: relay chunks go out only on an open socket; the
// chunks captured before that ride the open event's replay.
const VOICE_WS_OPEN = 1;

const VOICE_HINT_LISTENING = 'Listening...';
const VOICE_HINT_WAITING = 'Waiting for microphone signal...';
const VOICE_HINT_UPLOADING = 'Uploading...';
const VOICE_HINT_DECODING = 'Decoding...';
const VOICE_HINT_RETRY = 'Upload failed — click to retry';

const VOICE_OVERLAY_CLASS = 'absolute left-0 right-0 bottom-full mb-2 rounded-lg border border-blue-500/40 bg-slate-800 px-3 py-2 text-sm text-slate-100 shadow-lg max-h-28 overflow-y-auto';
// Not-yet-final words from the preview relay: grey, above the level bar.
const VOICE_PARTIAL_CLASS = 'voice-partial-text text-sm text-slate-400 whitespace-pre-wrap break-words';
// The caret's dropdown, anchored above the mic group the template renders.
const VOICE_MENU_CLASS = 'absolute bottom-full right-0 mb-2 w-64 rounded-lg border border-slate-600 bg-slate-800 py-1 shadow-lg z-50';
const VOICE_MENU_TITLE_CLASS = 'px-3 py-1.5 text-xs text-slate-500';
const VOICE_MENU_ITEM_CLASS = 'flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm';
const VOICE_MENU_ITEM_ENABLED_CLASS = ' text-slate-200 hover:bg-slate-700';
const VOICE_MENU_ITEM_DISABLED_CLASS = ' text-slate-500 cursor-not-allowed';
const VOICE_MENU_CHECK_CLASS = 'w-4 shrink-0 text-green-400';
// On phones the caret is hidden and the composer's ⋯ menu carries this entry
// instead; it opens the same menu the caret does.
const VOICE_OVERFLOW_MENU_ID = 'overflow-menu';
const VOICE_OVERFLOW_ENTRY_ID = 'voice-backend-menu-entry';

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

    const consumed = Math.floor(this.sourcePosition);
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
// Backend selection and the caret menu
// ---------------------------------------------------------------------------
// The choices ride the page as VOICE_BACKENDS (id, label, livePartials,
// unavailableReason) with the server's VOICE_DEFAULT_BACKEND; the user's pick
// lives in localStorage. The relay, the upload's backend field, and the mic's
// tooltip all read the same selection; none of them compares
// against a backend id — the livePartials attribute decides everything.

function selectedVoiceBackend() {
  // The stored choice wins while it exists and is configured; an unknown or
  // unavailable stored id falls back to the server default (a typo there fails
  // config load). The stored id stays put, so a credential that arrives later
  // re-activates it on the next page load.
  const stored = localStorage.getItem(VOICE_BACKEND_STORAGE_KEY);
  const storedEntry = stored ? VOICE_BACKENDS.find((entry) => entry.id === stored) : null;
  if (storedEntry && !storedEntry.unavailableReason) return storedEntry;
  return VOICE_BACKENDS.find((entry) => entry.id === VOICE_DEFAULT_BACKEND);
}

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
    labelSpan.className = 'truncate';
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

function updateVoiceMicTitle() {
  const btn = document.getElementById('voice-btn');
  const selected = selectedVoiceBackend();
  if (btn && selected) btn.title = 'Voice input · ' + selected.label;
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

// The composer's ⋯ menu entry, created by this package so the core template
// names nothing it defines. Opening from the entry closes the ⋯ menu first;
// both entries drive the one backend menu.
function ensureVoiceBackendEntry() {
  const overflow = document.getElementById(VOICE_OVERFLOW_MENU_ID);
  if (!overflow || document.getElementById(VOICE_OVERFLOW_ENTRY_ID)) return;
  const entry = document.createElement('button');
  entry.type = 'button';
  entry.id = VOICE_OVERFLOW_ENTRY_ID;
  entry.textContent = 'Voice backend';
  entry.onclick = openVoiceBackendMenuFromOverflow;
  overflow.appendChild(entry);
}

function openVoiceBackendMenuFromOverflow() {
  const overflow = document.getElementById(VOICE_OVERFLOW_MENU_ID);
  if (overflow) overflow.classList.remove('show');
  openVoiceBackendMenu();
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
  // menu's right edge past the group until its left edge sits at the margin.
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

async function toggleVoice() {
  const run = activeVoiceRun;
  if (!run) {
    await startRecording();
    return;
  }
  // Clicks during arming or finalizing are ignored, not queued; a recording
  // converts a click into stop, an in-flight upload into a cancel, a failed
  // upload into a retry, and a stopped live recording into the upload (stop is
  // idempotent: the recording flag clears before the first await).
  if (run.recording) await stopRecording(run);
  else if (run.phase === 'uploading' || run.phase === 'decoding') cancelVoiceUpload(run);
  else if (run.phase === 'retry') startUpload(run);
  else if (run.phase === 'stop') {
    // The stop state's one click: the relay closes first, so a final still in
    // flight cannot land beside or after the upload's transcript. The form
    // keeps the live backend id; the server transcribes with that backend.
    closeVoiceRelay(run);
    startUpload(run);
  }
}

// The one construction site of a recording's run object: every phase below
// mutates the fields it initializes, so a field the pipeline needs starts here.
function newVoiceRun(sessionId, backend) {
  return {
    sessionId,
    stream: null,
    audioContext: null,
    sourceNode: null,
    workletNode: null,
    recording: false,
    stopping: false,
    signalReady: false,
    phase: 'idle',
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
    relaySocket: null,
    relayFinalResolve: null,
    relayWaitTimer: null,
    relayFailure: null,
  };
}

async function startRecording() {
  if (!SESSION_ID) {
    showToast('Open a session before recording voice input', true);
    return;
  }
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    showToast('Microphone access is not available. Use HTTPS or a supported browser.', true);
    return;
  }
  if (!window.AudioContext && !window.webkitAudioContext) {
    showToast('Audio capture is not available in this browser', true);
    return;
  }

  const run = newVoiceRun(SESSION_ID, selectedVoiceBackend());
  activeVoiceRun = run;
  // Amber from the click on: the mic is not delivering sound yet, so the
  // user must not speak — red is the speak cue and lights at the first
  // chunk with a non-zero sample.
  setVoiceButtonState('starting');
  updateVoiceCaret();
  if (run.backend.livePartials) openVoiceRelay(run);
  run.ui = ensureVoiceOverlay();
  showVoiceHint(run, 'Starting...');

  // Parallel startup: both branches launch in this one synchronous step, so
  // the wait is the longer of the two instead of their sum. Each resource
  // lands on the run as it opens, which the catch below relies on: one
  // release path frees whatever a failed startup leaves behind.
  const micBranch = navigator.mediaDevices.getUserMedia({
    audio: {
      channelCount: 1,
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    },
  }).then((stream) => {
    run.stream = stream;
    // The recording's device facts: collection starts here, at recording
    // start, without delaying it — the end frame and the upload await the
    // settled object instead.
    run.devicesPromise = collectVoiceDevices(run);
  });
  const graphBranch = (async () => {
    const AudioContextCtor = window.AudioContext || window.webkitAudioContext;
    run.audioContext = new AudioContextCtor();
    const workletUrl = URL.createObjectURL(new Blob([VOICE_WORKLET_SOURCE], {type: 'application/javascript'}));
    try {
      await run.audioContext.audioWorklet.addModule(workletUrl);
    } finally {
      URL.revokeObjectURL(workletUrl);
    }
  })();

  try {
    await Promise.all([micBranch, graphBranch]);
    if (activeVoiceRun !== run) {
      abortVoiceRun(run);
      return;
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
    run.phase = 'recording';
    updateVoiceLeaveGuard();
    // Connected is not yet sounding: a just-opened device can deliver
    // seconds of digital zero, so the button stays amber and the hint waits
    // for the first non-zero sample.
    showVoiceHint(run, VOICE_HINT_WAITING);
    updateVoiceIndicator(run);
  } catch (err) {
    // Both branches settle before the release, so the slower branch's
    // resources are already on the run: the one release path closes the
    // AudioContext and stops every opened track in either order of success
    // and failure.
    await Promise.allSettled([micBranch, graphBranch]);
    if (activeVoiceRun !== run) {
      abortVoiceRun(run);
      return;
    }
    console.error('Voice input failed:', err);
    showToast('Voice input failed: ' + err.message, true);
    releaseVoiceRun(run);
  }
}

// --- Recording devices ------------------------------------------------------
// One devices object per recording, collected when the mic opens: which
// microphone captured it, the audio processing the track actually ran with,
// and the output devices outside sound could have bled in from. Exactly five
// keys — the server's voice_transcribed log schema.
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
  // click lands would otherwise lose its samples); trigger checks stay live only
  // while actually recording.
  if (!run.recording && !run.stopping) return;
  const samples = new Int16Array(data.buffer);
  run.pcmChunks.push(data.buffer);
  run.totalSamples += samples.length;
  updateVoiceLevel(run, samples);
  updateVoiceIndicator(run);
  sendVoiceRelayAudio(run, data.buffer);
  if (run.recording) {
    // The ready signal: sound is arriving, so the button may go red — the
    // speak cue. One non-zero sample is enough; a level threshold would keep
    // a quiet room amber forever. The flag ends the inspection for the rest
    // of the run.
    if (!run.signalReady) {
      for (let i = 0; i < samples.length; i++) {
        if (samples[i] === 0) continue;
        run.signalReady = true;
        setVoiceButtonState('recording');
        showVoiceHint(run, voiceReadyHint(run));
        break;
      }
    }
    if (run.totalSamples >= VOICE_MAX_SAMPLES) stopRecording(run); // auto-stop enters the stop flow
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

async function stopRecording(run) {
  if (!run.recording) return;
  run.recording = false;
  run.stopping = true;
  setVoiceButtonState('idle');
  updateVoiceLeaveGuard();
  try {
    // The worklet drains its buffered tail into the local buffer — and, live,
    // out to the relay — so the recording is whole before it ends; the upload
    // (after a stop-state click, or right away on a non-live backend)
    // assembles its WAV from the same buffer.
    await flushVoiceWorklet(run);
    if (activeVoiceRun !== run) return;
    cleanupVoiceCapture(run);
    // A live backend gets the length-scaled final budget here; the leave guard
    // stays engaged through the wait because run.stopping has not cleared yet.
    const finalText = run.relaySocket ? await collectVoiceRelayFinal(run) : null;
    if (activeVoiceRun !== run) return;
    run.stopping = false;
    showVoicePartial(run, '');
    if (finalText !== null) {
      // The relay's final means the recording is already archived server-side:
      // the words go in through the one insertion path and the run ends with
      // no upload, so the send lock never engages.
      finishWithRelayFinal(run, finalText);
      return;
    }
    if (run.backend.livePartials) {
      // No final from a live relay: the budget expired (the relay is still
      // open) or a relay failure was recorded earlier. No upload starts from
      // here; the run stops visibly and one click starts the upload.
      enterVoiceStopState(run);
      return;
    }
    startUpload(run);
  } catch (err) {
    console.error('Voice stop failed:', err);
    showToast('Voice input failed: ' + err.message, true);
    releaseVoiceRun(run);
  }
}

// The relay's final, inside the budget or after it through the stop state's
// still-open relay: the recording is already archived, so the words go in and
// the run ends with nothing uploaded. An empty final means the recording held
// no speech.
function finishWithRelayFinal(run, finalText) {
  releaseVoiceRun(run);
  const words = finalText.trim();
  if (!words) {
    showToast('No speech detected');
    return;
  }
  insertVoiceText(words);
}

// The stop state: a live recording is over, no final owns the words, and no
// upload may start on its own. The buffer stays and the hint names the backend
// and the reason; one click on the voice button (the toggleVoice stop branch)
// closes the relay and starts the upload. While the relay is
// still open, the budget-expiry case, a final arriving before that click ends
// the run through finishWithRelayFinal instead. The phase also keeps the leave
// guard engaged: the recording exists only in this browser until the click.
function enterVoiceStopState(run) {
  run.phase = 'stop';
  updateVoiceLeaveGuard();
  showVoiceHint(run, run.relayFailure !== null
      ? voiceRelayFailureHint(run, run.relayFailure)
      : voiceRelayBudgetHint(run));
}

async function startUpload(run) {
  // The devices object settled at recording start rides the form; awaiting it
  // here is the upload waiting for the enumeration, never the reverse.
  const devices = await run.devicesPromise;
  if (activeVoiceRun !== run) return;
  run.phase = 'uploading';
  run.uploadTimedOut = false;
  run.requestInFlight = true;
  // The transcription window (upload + decode) opens here: it counts toward
  // the send-button lock; the recording before it never did.
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
    backendId: run.backend.id,
    devices,
    onXhr: (xhr) => { run.xhr = xhr; },
    onSent: () => {
      if (activeVoiceRun !== run) return;
      clearVoiceUploadTimer(run);
      run.phase = 'decoding';
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
  // The window closes with the failure; a retry re-enters startUpload and
  // opens a fresh one.
  inputWorkSettled(VOICE_TRANSCRIBE_IN_FLIGHT_MESSAGE);
  run.phase = 'retry'; // the buffer stays; the next click re-uploads it
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
  // The window closes with the decode landed; run.xhr is already null, so the
  // release's abort below settles nothing further.
  inputWorkSettled(VOICE_TRANSCRIBE_IN_FLIGHT_MESSAGE);
  releaseVoiceRun(run);
  const words = fullText.trim();
  if (!words) {
    showToast('No speech detected');
    return;
  }
  insertVoiceText(words);
}

function insertVoiceText(text) {
  // The one text-insertion path for recognized voice words: append to the input,
  // mark the message as voice-contributed, and restore the caret. Returns the
  // start offset of the inserted span.
  const input = document.getElementById('msg-input');
  const current = input.value.trim();
  input.value = current ? current + ' ' + text : text;
  Chat.setInputMode(VOICE_INPUT_MODE);
  autoResize(input);
  saveDraft();
  input.focus();
  return input.value.length - text.length;
}

// --- WAV assembly -----------------------------------------------------------
// The buffer is a list of int16-LE ArrayBuffers in capture order; the WAV is
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

// The upload's transport: a multipart form with the audio plus the
// backend the browser selected and the recording's device object as JSON — the
// server persists the recording, transcribes it, and returns the text.
// A null backendId or devices leaves that form field out.
function postVoiceRecording(path, wavBuffer, {backendId, devices, ...opts}) {
  const form = new FormData();
  form.append('audio', new Blob([wavBuffer]), 'recording.wav');
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
// starts an upload: during recording the reason lands on the run and in the
// hint at once while the recording continues; at stop the run enters the stop
// state, where a failure only rewrites the hint reason.

function openVoiceRelay(run) {
  const socket = new WebSocket(wsUrlWithToken(
      `/ws/voice/${encodeURIComponent(run.sessionId)}?backend=${encodeURIComponent(run.backend.id)}`));
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
    showVoicePartial(run, message.text);
    return;
  }
  if (message.type === 'final') {
    // The final ends the run wherever it can still matter: inside the budget
    // through the wait's resolve, in the stop state (the budget expired, the
    // relay stayed open) through the one insertion path. With neither, after
    // the click that started the upload or after the release, the socket is
    // already down and the late final is discarded.
    if (run.relayFinalResolve) {
      settleVoiceRelayWait(run, message.text);
      return;
    }
    if (run.phase === 'stop') finishWithRelayFinal(run, message.text);
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
  // A relay that already failed keeps its reason up; the ready cue must not
  // overwrite it.
  if (run.relayFailure !== null) return voiceRelayFailureHint(run, run.relayFailure);
  return run.backend.livePartials ? 'Listening · ' + run.backend.label : VOICE_HINT_LISTENING;
}

async function collectVoiceRelayFinal(run) {
  // Seal the relay's recording and wait out the final for the budget the
  // recording's length earns; null means the budget expired with the relay
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

// A relay failure, an error frame, a lost socket, or a failed end send: the
// reason lands on the run and in the hint at once and the socket goes down. No
// upload starts here; during recording the recording just continues, and at
// stop the run is on its way into (or already sits in) the stop state, where
// the failure only rewrites the hint reason.
function failVoiceRelay(run, reason) {
  showVoicePartial(run, '');
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

function showVoicePartial(run, text) {
  // Not-yet-final words from the preview relay, in grey; '' clears them.
  if (run.ui && run.ui.partial) run.ui.partial.textContent = text;
}

function updateVoiceLevel(run, samples) {
  // Per-chunk RMS mapped onto the bar, with a decay so silence falls instead of
  // snapping: pure client-side, no server round-trip.
  let sum = 0;
  for (let i = 0; i < samples.length; i++) {
    const v = samples[i] / 32768;
    sum += v * v;
  }
  const rms = Math.sqrt(sum / Math.max(samples.length, 1));
  const level = Math.min(100, Math.round(rms * 400));
  run.level = Math.max(level, Math.round(run.level * 0.8));
  if (run.ui && run.ui.fill) run.ui.fill.style.width = run.level + '%';
}

// --- Page-leave guard -------------------------------------------------------
// Engaged while recording (including the stop flush), while a request is in
// flight, or while a stopped live recording sits in the stop state, where the
// buffer exists only in this browser until its click. Disengaged when idle.
// The popstate SPA branch in app.js is untouched: this is the generic
// page-leave path, so a reload cannot silently drop a recording.
let voiceLeaveGuardInstalled = false;

function onVoiceLeaveGuard(event) {
  event.preventDefault();
  event.returnValue = '';
}

function updateVoiceLeaveGuard() {
  const busy = activeVoiceRun && (activeVoiceRun.recording || activeVoiceRun.stopping ||
      activeVoiceRun.requestInFlight || activeVoiceRun.phase === 'stop');
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
  if (run) abortVoiceRun(run);
  setVoiceButtonState('idle');
  removeVoiceOverlay();
  updateVoiceLeaveGuard();
  updateVoiceCaret();
  Chat.setInputMode(null);
}

function discardVoiceRecording(run, message) {
  releaseVoiceRun(run);
  if (message) showToast(message, true);
}

// End-of-run release for the run that owns the slot: tear down its resources,
// free the slot, and reset the button and overlay. Callers add any toast.
function releaseVoiceRun(run) {
  abortVoiceRun(run);
  if (activeVoiceRun === run) activeVoiceRun = null;
  clearVoiceUploadTimer(run);
  setVoiceButtonState('idle');
  removeVoiceOverlay();
  updateVoiceLeaveGuard();
  updateVoiceCaret();
}

// Tear down every resource a run owns: capture graph, mic stream, and any
// in-flight upload. Idempotent, so an ownership-lost continuation and the
// teardown path can both call it; the later call only sees nulls.
function abortVoiceRun(run) {
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
  // An xhr still held here means its startUpload window never settled: the
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
    run.audioContext = null;
    ctx.close().catch((err) => console.warn('Voice audio context close failed:', err));
  }
  if (run.stream) {
    run.stream.getTracks().forEach((track) => track.stop());
    run.stream = null;
  }
  run.flushResolvers.forEach((resolve) => resolve());
  run.flushResolvers.clear();
}

// The button's one class pair per run state: amber promises no sound is
// being recorded yet, red means sound is arriving now.
function setVoiceButtonState(state) {
  const btn = document.getElementById('voice-btn');
  if (!btn) return;
  btn.classList.remove('bg-slate-800', 'border-slate-600', 'bg-amber-600', 'border-amber-500', 'bg-red-600', 'border-red-500');
  if (state === 'starting') btn.classList.add('bg-amber-600', 'border-amber-500');
  else if (state === 'recording') btn.classList.add('bg-red-600', 'border-red-500');
  else btn.classList.add('bg-slate-800', 'border-slate-600');
}

function removeVoiceOverlay() {
  document.getElementById('voice-partial-overlay')?.remove();
}

// First paint: the mic's tooltip names the backend a recording would use, and
// the composer's ⋯ menu carries the backend entry phones use.
updateVoiceMicTitle();
ensureVoiceBackendEntry();

// A dictated message's bubble carries the microphone badge, and the run ends
// with the session view it belongs to.
Chat.registerInputMode(VOICE_INPUT_MODE, '&#127908; Voice');
document.addEventListener('charliebot:session-view-teardown', resetVoiceState);

// ---------------------------------------------------------------------------
// Voice input: record locally, upload on release ("record-then-upload")
// ---------------------------------------------------------------------------
// One recording = one run object owning its mic stream, capture graph, local PCM
// buffer, and at most one in-flight upload request. A single module-level slot
// (`activeVoiceRun`) is claimed synchronously inside the click handler, so two
// pipelines can never coexist: every asynchronous continuation of a run first
// checks that the slot still belongs to it and silently exits otherwise.
//
// The recording itself never touches the server: the worklet's 16 kHz PCM chunks
// append to a local buffer, and the level bar plus elapsed timer are computed
// from those chunks client-side. Exactly one round-trip happens mid-recording —
// the recognition-confirm probe at 5 s, whose words are inserted through the
// same path as the final text (and replaced by it on success). On release the
// whole buffer assembles into a WAV and uploads once; the server persists it,
// decodes offline, and returns the full text. Every failure keeps the buffer
// and offers retry; only success or an explicit cancel discards it.
let activeVoiceRun = null;

const VOICE_SAMPLE_RATE = 16000;
const VOICE_CHUNK_SAMPLES = 2048; // 128 ms of audio per worklet chunk
const VOICE_CONFIRM_TRIGGER_SAMPLES = 5 * VOICE_SAMPLE_RATE; // the one probe fires at 5 s
const VOICE_MAX_SAMPLES = 5 * 60 * VOICE_SAMPLE_RATE; // the server's recording cap
// The upload-phase budget: past it the request aborts into the retry state. The
// decode wait after the body is sent is unbounded but visible ("Decoding...").
const VOICE_UPLOAD_TIMEOUT_MS = 60 * 1000;

const VOICE_HINT_LISTENING = 'Listening...';
const VOICE_HINT_UPLOADING = 'Uploading...';
const VOICE_HINT_DECODING = 'Decoding...';
const VOICE_HINT_RETRY = 'Upload failed — click to retry';
const VOICE_HINT_CONFIRM_FAILED = 'Recognition check failed';

const VOICE_OVERLAY_CLASS = 'absolute left-0 right-0 bottom-full mb-2 rounded-lg border border-blue-500/40 bg-slate-800 px-3 py-2 text-sm text-slate-100 shadow-lg max-h-28 overflow-y-auto';

const VOICE_WORKLET_SOURCE = `
class VoiceCaptureProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.inputRate = options.processorOptions.inputSampleRate;
    this.outputRate = 16000;
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

async function toggleVoice() {
  const run = activeVoiceRun;
  if (!run) {
    await startRecording();
    return;
  }
  // Clicks during arming or finalizing are ignored, not queued; a live recording
  // converts a click into stop-and-upload, an in-flight upload into a cancel,
  // and a failed upload into a retry (stop is idempotent: the recording flag
  // clears before the first await).
  if (run.recording) await stopRecording(run);
  else if (run.phase === 'uploading' || run.phase === 'decoding') cancelVoiceUpload(run);
  else if (run.phase === 'retry') startUpload(run);
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

  const run = {
    sessionId: SESSION_ID,
    stream: null,
    audioContext: null,
    sourceNode: null,
    workletNode: null,
    recording: false,
    stopping: false,
    phase: 'idle',
    pcmChunks: [],
    totalSamples: 0,
    level: 0,
    confirmFired: false,
    confirmedText: '',
    confirmedSpan: null,
    xhr: null,
    requestInFlight: false,
    uploadTimedOut: false,
    uploadTimer: null,
    flushId: 0,
    flushResolvers: new Map(),
    ui: null,
  };
  activeVoiceRun = run;
  setVoiceButtonRecording(true);
  run.ui = ensureVoiceOverlay();
  showVoiceHint(run, 'Starting...');
  try {
    run.stream = await navigator.mediaDevices.getUserMedia({audio: true});

    const AudioContextCtor = window.AudioContext || window.webkitAudioContext;
    run.audioContext = new AudioContextCtor();
    const workletUrl = URL.createObjectURL(new Blob([VOICE_WORKLET_SOURCE], {type: 'application/javascript'}));
    try {
      await run.audioContext.audioWorklet.addModule(workletUrl);
    } finally {
      URL.revokeObjectURL(workletUrl);
    }
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
    showVoiceHint(run, VOICE_HINT_LISTENING);
    updateVoiceIndicator(run);
  } catch (err) {
    if (activeVoiceRun !== run) return;
    console.error('Voice input failed:', err);
    showToast('Voice input failed: ' + err.message, true);
    releaseVoiceRun(run);
  }
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
  if (run.recording) {
    if (!run.confirmFired && run.totalSamples >= VOICE_CONFIRM_TRIGGER_SAMPLES) fireVoiceConfirm(run);
    if (run.totalSamples >= VOICE_MAX_SAMPLES) stopRecording(run); // auto-stop flows into the upload
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
  setVoiceButtonRecording(false);
  updateVoiceLeaveGuard();
  try {
    // The worklet drains its buffered tail into the local buffer before the
    // upload assembles the WAV from it.
    await flushVoiceWorklet(run);
    if (activeVoiceRun !== run) return;
    run.stopping = false;
    cleanupVoiceCapture(run);
    startUpload(run);
  } catch (err) {
    console.error('Voice stop failed:', err);
    showToast('Voice input failed: ' + err.message, true);
    releaseVoiceRun(run);
  }
}

function fireVoiceConfirm(run) {
  run.confirmFired = true;
  const wav = assembleVoiceWav(run.pcmChunks, VOICE_CONFIRM_TRIGGER_SAMPLES);
  // Fire-and-forget by design: a failed or slow probe shows a hint only and
  // never blocks the recording or the later upload.
  postVoiceWav(`/api/voice/${encodeURIComponent(run.sessionId)}/confirm`, wav)
    .then((text) => {
      if (activeVoiceRun !== run) return;
      const words = text.trim();
      if (!words) return;
      const spanStart = insertVoiceText(words);
      run.confirmedSpan = {start: spanStart, end: spanStart + words.length};
      run.confirmedText = words;
      showVoiceHint(run, words);
    })
    .catch((err) => {
      if (activeVoiceRun !== run) return;
      console.warn('Voice confirm probe failed:', err);
      showVoiceHint(run, VOICE_HINT_CONFIRM_FAILED);
    });
}

function startUpload(run) {
  run.phase = 'uploading';
  run.uploadTimedOut = false;
  run.requestInFlight = true;
  updateVoiceLeaveGuard();
  showVoiceHint(run, VOICE_HINT_UPLOADING);
  run.uploadTimer = setTimeout(() => {
    run.uploadTimer = null;
    if (activeVoiceRun !== run || !run.xhr) return;
    run.uploadTimedOut = true;
    run.xhr.abort();
  }, VOICE_UPLOAD_TIMEOUT_MS);
  const wav = assembleVoiceWav(run.pcmChunks, Math.min(run.totalSamples, VOICE_MAX_SAMPLES));
  postVoiceWav(`/api/voice/${encodeURIComponent(run.sessionId)}`, wav, {
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
  releaseVoiceRun(run);
  const words = fullText.trim();
  if (!words) {
    if (!run.confirmedText) showToast('No speech detected');
    return;
  }
  const input = document.getElementById('msg-input');
  const span = run.confirmedSpan;
  if (span && input.value.slice(span.start, span.end) === run.confirmedText) {
    // The confirm words are still where the probe inserted them: the full text
    // (which contains them) replaces that span instead of duplicating it.
    const head = input.value.slice(0, span.start);
    const tail = input.value.slice(span.end);
    input.value = head + words + tail;
    Chat.setVoiceContributed(true);
    autoResize(input);
    saveDraft();
    input.focus();
  } else {
    insertVoiceText(words);
  }
}

function insertVoiceText(text) {
  // The one text-insertion path for recognized voice words: append to the input,
  // mark the message as voice-contributed, and restore the caret. Returns the
  // start offset of the inserted span.
  const input = document.getElementById('msg-input');
  const current = input.value.trim();
  input.value = current ? current + ' ' + text : text;
  Chat.setVoiceContributed(true);
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

function postVoiceWav(path, wavBuffer, {onSent = null, onXhr = null} = {}) {
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
    xhr.send(wavBuffer);
  });
}

function clearVoiceUploadTimer(run) {
  if (run.uploadTimer !== null) {
    clearTimeout(run.uploadTimer);
    run.uploadTimer = null;
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
  const byClass = (name) => overlay.children.find((child) => child.className === name) || null;
  const bar = byClass('voice-level-bar');
  const meta = byClass('voice-meta');
  return {
    overlay,
    fill: bar ? bar.children.find((child) => child.className === 'voice-level-fill') : null,
    timer: meta ? meta.children.find((child) => child.className === 'voice-timer') : null,
    hint: meta ? meta.children.find((child) => child.className === 'voice-hint') : null,
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
// Engaged while recording (including the stop flush) or a request is in flight,
// disengaged when idle. The popstate SPA branch in app.js is untouched: this is
// the generic page-leave path, so a reload cannot silently drop a recording.
let voiceLeaveGuardInstalled = false;

function onVoiceLeaveGuard(event) {
  event.preventDefault();
  event.returnValue = '';
}

function updateVoiceLeaveGuard() {
  const busy = activeVoiceRun && (activeVoiceRun.recording || activeVoiceRun.stopping || activeVoiceRun.requestInFlight);
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
  setVoiceButtonRecording(false);
  removeVoiceOverlay();
  updateVoiceLeaveGuard();
  Chat.setVoiceContributed(false);
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
  setVoiceButtonRecording(false);
  removeVoiceOverlay();
  updateVoiceLeaveGuard();
}

// Tear down every resource a run owns: capture graph, mic stream, and any
// in-flight upload. Idempotent, so an ownership-lost continuation and the
// teardown path can both call it; the later call only sees nulls.
function abortVoiceRun(run) {
  cleanupVoiceCapture(run);
  abortVoiceUpload(run);
}

function abortVoiceUpload(run) {
  clearVoiceUploadTimer(run);
  const xhr = run.xhr;
  run.xhr = null;
  if (!xhr) return;
  xhr.abort();
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

function setVoiceButtonRecording(recording) {
  const btn = document.getElementById('voice-btn');
  if (!btn) return;
  if (recording) {
    btn.classList.add('bg-red-600', 'border-red-500');
    btn.classList.remove('bg-slate-800', 'border-slate-600');
  } else {
    btn.classList.remove('bg-red-600', 'border-red-500');
    btn.classList.add('bg-slate-800', 'border-slate-600');
  }
}

function removeVoiceOverlay() {
  document.getElementById('voice-partial-overlay')?.remove();
}

// ---------------------------------------------------------------------------
// Voice input
// ---------------------------------------------------------------------------
// One recording = one run object owning its WebSocket, mic stream, and capture
// graph. A single module-level slot (`activeVoiceRun`) is claimed
// synchronously inside the click handler, so two pipelines can never coexist:
// every asynchronous continuation and event of a run first checks that the
// slot still belongs to it and silently exits otherwise, and no socket can
// outlive the click that created it.
let activeVoiceRun = null;

const VOICE_CHUNK_SAMPLES = 2048;

// Discard toasts name their failure mode through one constant each: the stop
// path and the close race report the same closed connection, and both
// undecodable server frames report the same invalid response.
const VOICE_CONNECTION_CLOSED_MESSAGE = 'Voice connection closed before transcription finished';
const VOICE_INVALID_RESPONSE_MESSAGE = 'Invalid voice response from server';

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
  // Clicks during arming or finalizing are ignored, not queued; only a live
  // recording converts a click into a stop (and stop is idempotent: the
  // recording flag clears before the first await).
  if (run.recording) await stopRecording(run);
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
    socket: null,
    stream: null,
    audioContext: null,
    sourceNode: null,
    workletNode: null,
    recording: false,
    stopping: false,
    awaitingFinal: false,
    flushId: 0,
    flushResolvers: new Map(),
  };
  const targetSession = SESSION_ID;
  activeVoiceRun = run;
  setVoiceButtonRecording(true);
  showVoiceOverlay('Starting...');
  try {
    run.stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        channelCount: 1,
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      },
    });
    if (activeVoiceRun !== run) {
      abortVoiceRun(run);
      return;
    }

    run.socket = await openVoiceSocket(run, targetSession);
    if (activeVoiceRun !== run) {
      abortVoiceRun(run);
      return;
    }
    setupVoiceSocketHandlers(run, targetSession);

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
    showVoiceOverlay('Listening...');
  } catch (err) {
    if (activeVoiceRun !== run) return;
    console.error('Voice input failed:', err);
    showToast('Voice input failed: ' + err.message, true);
    releaseVoiceRun(run);
  }
}

async function stopRecording(run) {
  if (!run.recording) return;
  run.recording = false;
  setVoiceButtonRecording(false);
  try {
    // The worklet drains its buffered tail while frames are still accepted on
    // the socket; the drop flag and the stop message follow the flush.
    await flushVoiceWorklet(run);
    if (activeVoiceRun !== run) return;
    run.stopping = true;
    cleanupVoiceCapture(run);
    if (run.socket && run.socket.readyState === WebSocket.OPEN) {
      run.awaitingFinal = true;
      run.socket.send(JSON.stringify({type: 'stop'}));
      showVoiceOverlay('Finalizing...');
    } else {
      discardVoiceRecording(run, VOICE_CONNECTION_CLOSED_MESSAGE);
    }
  } catch (err) {
    console.error('Voice stop failed:', err);
    discardVoiceRecording(run, 'Voice input failed: ' + err.message);
  }
}

function resetVoiceState() {
  // Teardown takes the run out of the slot first, then aborts it: pending
  // continuations die on the ownership check and no socket outlives the view.
  const run = activeVoiceRun;
  activeVoiceRun = null;
  if (run) abortVoiceRun(run);
  setVoiceButtonRecording(false);
  removeVoiceOverlay();
  Chat.setVoiceContributed(false);
}

function openVoiceSocket(run, targetSession) {
  // The socket is claimed onto the run synchronously, so a teardown that lands
  // during the connecting window can abort it like any other resource.
  const wsUrl = wsUrlWithToken(`/ws/voice/${encodeURIComponent(targetSession)}`);
  const socket = new WebSocket(wsUrl);
  socket.binaryType = 'arraybuffer';
  run.socket = socket;
  return new Promise((resolve, reject) => {
    socket.onopen = () => resolve(socket);
    socket.onerror = () => reject(new Error('voice WebSocket connection failed'));
    socket.onclose = () => reject(new Error('voice WebSocket closed before recording started'));
  });
}

function setupVoiceSocketHandlers(run, targetSession) {
  const socket = run.socket;
  socket.onmessage = (event) => {
    if (activeVoiceRun !== run || targetSession !== SESSION_ID) return;
    let data;
    try {
      data = JSON.parse(event.data);
    } catch (err) {
      console.error('Invalid voice message:', err);
      discardVoiceRecording(run, VOICE_INVALID_RESPONSE_MESSAGE);
      return;
    }

    if (data.type === 'partial') {
      showVoiceOverlay(data.text || 'Listening...');
      return;
    }
    if (data.type === 'final') {
      applyVoiceFinal(run, data.text || '');
      return;
    }
    if (data.type === 'error') {
      discardVoiceRecording(run, data.text || 'Voice transcription failed');
      return;
    }
    discardVoiceRecording(run, VOICE_INVALID_RESPONSE_MESSAGE);
  };

  socket.onclose = () => {
    if (activeVoiceRun !== run) return;
    if (run.recording || run.stopping || run.awaitingFinal) {
      discardVoiceRecording(run, VOICE_CONNECTION_CLOSED_MESSAGE);
    }
  };

  socket.onerror = () => {
    if (activeVoiceRun !== run) return;
    discardVoiceRecording(run, 'Voice connection error');
  };
}

function handleVoiceWorkletMessage(run, event) {
  if (activeVoiceRun !== run) return;
  const data = event.data || {};
  if (data.type === 'pcm') {
    if (run.socket && run.socket.readyState === WebSocket.OPEN && !run.stopping) {
      run.socket.send(data.buffer);
    }
    return;
  }
  if (data.type === 'flushed') {
    const resolve = run.flushResolvers.get(data.id);
    if (resolve) {
      run.flushResolvers.delete(data.id);
      resolve();
    }
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

function applyVoiceFinal(run, text) {
  const finalText = text.trim();
  releaseVoiceRun(run);
  if (!finalText) {
    showToast('No speech detected');
    return;
  }

  const input = document.getElementById('msg-input');
  const current = input.value.trim();
  input.value = current ? current + ' ' + finalText : finalText;
  Chat.setVoiceContributed(true);
  autoResize(input);
  saveDraft();
  input.focus();
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
  setVoiceButtonRecording(false);
  removeVoiceOverlay();
}

// Tear down every resource a run owns: capture graph, mic stream, and socket
// (closing even a still-connecting socket and an acquired-but-unused stream).
// Idempotent, so an ownership-lost continuation and the teardown path can both
// call it; the later call only sees nulls. View state (button, overlay, slot)
// belongs to the slot holder and is handled by the callers.
function abortVoiceRun(run) {
  cleanupVoiceCapture(run);
  closeVoiceSocket(run);
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

function closeVoiceSocket(run) {
  const socket = run.socket;
  run.socket = null;
  if (!socket) return;
  detachSocketHandlers(socket);
  if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) {
    socket.close();
  }
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

function showVoiceOverlay(text) {
  const input = document.getElementById('msg-input');
  if (!input || !input.parentElement) return;
  let overlay = document.getElementById('voice-partial-overlay');
  if (!overlay) {
    overlay = document.createElement('div');
    overlay.id = 'voice-partial-overlay';
    overlay.className = 'absolute left-0 right-0 bottom-full mb-2 rounded-lg border border-blue-500/40 bg-slate-800 px-3 py-2 text-sm text-slate-100 shadow-lg max-h-28 overflow-y-auto';
    input.parentElement.appendChild(overlay);
  }
  overlay.textContent = text || 'Listening...';
}

function removeVoiceOverlay() {
  document.getElementById('voice-partial-overlay')?.remove();
}

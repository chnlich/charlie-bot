// The streamed draft is a pure function of the frame sequence the page receives.
//
// A live `stream` frame carries only the text added since the previous one, so the page accumulates the
// draft. A page that joins or reconnects mid-reply gets one catch-up frame holding the whole draft, and it
// must end up with the same draft and the same committed message as a page that saw every live frame. The
// mechanism-level statement is that splitting the delivery at a reconnect and resending the whole draft
// there leaves the same screen.

const assert = require('node:assert/strict');
const test = require('node:test');

const { loadWebsocketContext } = require('./websocket_context_stub');

function buildClient() {
  const bubbles = [];
  const preview = {visible: false, content: ''};

  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: 's1',
    setTimeout: () => 0,
    document: {getElementById: () => null, querySelector: () => null},
    hideStreaming: () => { preview.visible = false; preview.content = ''; },
    showStreaming: (draft) => {
      preview.visible = true;
      preview.content = (draft && draft.content) || '';
    },
    appendMessageObject: (msg) => bubbles.push({role: msg.role, content: msg.content || ''}),
  });
  return {context, FakeWebSocket, bubbles, preview};
}

// What is on the screen: the committed bubbles plus the preview, if it holds text.
function screen(bubbles, preview) {
  const out = bubbles.map(b => b.role + ':' + b.content);
  if (preview.visible && preview.content) out.push('assistant:' + preview.content);
  return out;
}

function connect(client) {
  client.context.connectWS();
  const socket = client.FakeWebSocket.instances.at(-1);
  socket.emitOpen();
  return socket;
}

function deliver(socket, frames) {
  for (const frame of frames) socket.emitMessage(frame);
}

const REPLY = 'Here is the answer.';
const PIECES = ['Here is ', 'the answer.'];

// The live frames of one turn: the user message, then each streamed piece and its commit, then the
// separator. A commit carries no text: the page holds that piece from its append.
function liveFrames() {
  const frames = [{type: 'message', message: {role: 'user', content: 'q1', event_index: 0}}];
  PIECES.forEach((piece, index) => {
    const eventIndex = index + 1;
    frames.push({type: 'stream', append: piece, length: piece.length, event_index: eventIndex});
    frames.push({
      type: 'message',
      message: {
        role: 'assistant',
        content_streamed: true,
        event_index: eventIndex,
        id: `draft-${eventIndex}`,
        timestamp: `t${eventIndex}`,
      },
    });
  });
  frames.push({type: 'message', message: {role: 'separator', event_index: PIECES.length + 1}});
  frames.push({type: 'master_done', still_thinking: false, event_index: PIECES.length + 1});
  return frames;
}

const RENDERED = ['user:q1', 'assistant:' + PIECES[0], 'assistant:' + PIECES[1], 'separator:'];

// The catch-up a page receives when it joins or reconnects mid-reply: the committed messages, then one
// frame holding the whole draft so far.
function catchupFrames(draftText) {
  return [{
    type: 'stream',
    reset: true,
    append: draftText,
    length: draftText.length,
    event_index: 1,
  }];
}

function renderUnsplit(frames) {
  const client = buildClient();
  const socket = connect(client);
  deliver(socket, frames);
  socket.emitMessage({type: 'catchup_complete'});
  return screen(client.bubbles, client.preview);
}

// The draft the page holds at `k`: the appends since the last committed message.
function pendingDraftAt(frames, k) {
  let text = '';
  for (const frame of frames.slice(0, k)) {
    if (frame.type === 'message' && frame.message.role !== 'user') text = '';
    else if (frame.type === 'stream') text += frame.append;
  }
  return text;
}

function renderSplitAt(frames, k) {
  const client = buildClient();
  const first = connect(client);
  first.emitMessage({type: 'catchup_complete'});
  deliver(first, frames.slice(0, k));
  // The socket drops; the reconnect's catch-up resends the whole draft still open, then the rest.
  first.emitClose();
  const draftText = pendingDraftAt(frames, k);
  const second = connect(client);
  if (draftText) deliver(second, catchupFrames(draftText));
  deliver(second, frames.slice(k));
  second.emitMessage({type: 'catchup_complete'});
  return screen(client.bubbles, client.preview);
}

test('a completed turn renders the reply exactly once', () => {
  assert.deepEqual(renderUnsplit(liveFrames()), RENDERED);
});

test('a page that joined mid-reply renders the same draft and message', () => {
  const client = buildClient();
  const socket = connect(client);
  // The catch-up replays the turn's committed frames, then the whole draft so far.
  deliver(socket, [liveFrames()[0]]);
  deliver(socket, catchupFrames(PIECES[0]));
  deliver(socket, liveFrames().slice(2));
  socket.emitMessage({type: 'catchup_complete'});
  assert.deepEqual(screen(client.bubbles, client.preview), RENDERED);
});

test('a replayed append the page already holds is not concatenated twice', () => {
  const client = buildClient();
  const socket = connect(client);
  deliver(socket, catchupFrames(REPLY));
  // A frame queued during the catch-up repeats an append the catch-up already covered.
  deliver(socket, [liveFrames()[1]]);
  assert.equal(client.preview.content, REPLY);
});

test('splitting the frame sequence at a reconnect renders the same screen', () => {
  const frames = liveFrames();
  const want = renderUnsplit(frames);
  for (let k = 0; k <= frames.length; k++) {
    assert.deepEqual(renderSplitAt(frames, k), want, 'divergence when split at frame ' + k);
  }
});

test('a page that joins after the reply ended shows the full text', () => {
  const client = buildClient();
  const socket = connect(client);
  // The catch-up replay of a finished turn: the committed messages carry their text.
  deliver(socket, [{
    type: 'message',
    message: {role: 'assistant', content: REPLY, event_index: 1, id: 'draft-1', timestamp: 't1'},
  }]);
  socket.emitMessage({type: 'catchup_complete'});
  assert.deepEqual(screen(client.bubbles, client.preview), ['assistant:' + REPLY]);
});

test('a commit without its appends still commits what the page streamed', () => {
  const frames = liveFrames().slice(0, 2);
  const client = buildClient();
  const socket = connect(client);
  deliver(socket, frames);
  deliver(socket, [liveFrames()[2]]);
  assert.deepEqual(screen(client.bubbles, client.preview), ['user:q1', 'assistant:' + PIECES[0]]);
});

test('a reply that opens like the reply before it keeps its own text', () => {
  const client = buildClient();
  const socket = connect(client);
  deliver(socket, [
    {type: 'message', message: {role: 'user', content: 'q1', event_index: 0}},
    {type: 'stream', append: 'Let me check the logs first.', length: 28, event_index: 1},
    {type: 'message', message: {role: 'assistant', content_streamed: true, event_index: 1, id: 'a1', timestamp: 't1'}},
    {type: 'message', message: {role: 'user', content: 'q2', event_index: 2}},
    {type: 'stream', append: 'Let me', length: 6, event_index: 3},
    {type: 'stream', append: ' check', length: 12, event_index: 4},
    {type: 'stream', append: ' the schedule.', length: 26, event_index: 5},
    {type: 'message', message: {role: 'assistant', content_streamed: true, event_index: 5, id: 'a2', timestamp: 't2'}},
  ]);
  socket.emitMessage({type: 'catchup_complete'});
  assert.deepEqual(screen(client.bubbles, client.preview), [
    'user:q1',
    'assistant:Let me check the logs first.',
    'user:q2',
    'assistant:Let me check the schedule.',
  ]);
});

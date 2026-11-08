// ---------------------------------------------------------------------------
// The streamed draft
// ---------------------------------------------------------------------------
// A `stream` frame carries only the text added since the previous frame, so the page accumulates the draft
// from them. `length` and `thinking_length` are the draft's lengths after the frame, in UTF-16 code units. A
// frame whose end the page already holds either repeats text it has (a bootstrap or transcript snapshot may
// be ahead of frames still in flight) or belongs to the next draft; the overlap with the held text tells the
// two apart. The catch-up sends one `reset` frame holding the whole draft, so a page that joins or reconnects
// mid-reply renders the same draft as one that saw every frame.
//
// The tool rows ride the same frames as a suffix: a live frame's `tools_from` is the index of its first row in
// the draft's rows, and a reset frame carries the whole list without it. The rows are per-draft socket state:
// once the socket streams a suffix for the draft, a bootstrap or transcript seed's rows are older news, and
// only socket frames change the held ones.

let streamDraft = {content: '', thinking: '', tools: null, socketRows: false};

function resetStreamDraft() {
  streamDraft = {content: '', thinking: '', tools: null, socketRows: false};
}

// The text a frame extends. The part of `append` that overlaps the held text is either the same text this
// page already has, or text of another draft: only matching text is skipped.
function extendedStreamText(held, append, end) {
  if (!append) return held;
  if (typeof end !== 'number') return held + append;
  const start = end - append.length;
  const overlap = Math.min(held.length, end) - start;
  if (overlap <= 0) return held + append;
  if (held.slice(start, start + overlap) !== append.slice(0, overlap)) return append;
  return held.length > end ? held : held + append.slice(overlap);
}

// Seed the accumulated draft from a whole draft, as a bootstrap or a worker transcript poll delivers it. The
// seed fills the rows only while the socket has never streamed a suffix for this draft.
function seedStreamDraft(draft) {
  const content = (draft && draft.content) || '';
  const thinking = (draft && draft.thinking) || '';
  streamDraft = {
    content: extendedStreamText(streamDraft.content, content, content.length),
    thinking: extendedStreamText(streamDraft.thinking, thinking, thinking.length),
    tools: streamDraft.socketRows ? streamDraft.tools : (draft && draft.tools) || null,
    socketRows: streamDraft.socketRows,
  };
}

// The tool rows of a frame. A frame without `tools_from` — a reset frame — replaces the rows with its whole
// list. A frame with it keeps the held rows before that index and appends its own. A `tools_from` past the
// held rows is a protocol error: the page reports it and leaves its rows alone, since a correct server never
// sends one and no repair can guess the missing rows.
function applyStreamTools(ev) {
  if (!Array.isArray(ev.tools)) return;
  if (typeof ev.tools_from !== 'number') {
    streamDraft.tools = ev.tools;
    return;
  }
  const held = Array.isArray(streamDraft.tools) ? streamDraft.tools : [];
  if (ev.tools_from > held.length) {
    const message = 'stream frame tools_from ' + ev.tools_from + ' is past the ' + held.length + ' held tool rows';
    console.error(message);
    if (typeof showToast === 'function') showToast('Live tool rows out of sync — reload the page', true);
    return;
  }
  streamDraft.tools = held.slice(0, ev.tools_from).concat(ev.tools);
  streamDraft.socketRows = true;
}

// Apply a stream frame; returns the draft and whether the frame changed it. Tool rows count as a change: a frame
// that adds only tool rows still repaints the preview.
function applyStreamFrame(ev) {
  const before = streamDraftFingerprint();
  if (ev.reset) resetStreamDraft();
  streamDraft.content = extendedStreamText(streamDraft.content, ev.append, ev.length);
  streamDraft.thinking = extendedStreamText(streamDraft.thinking, ev.thinking_append, ev.thinking_length);
  applyStreamTools(ev);
  return {draft: streamDraft, changed: before !== streamDraftFingerprint()};
}

function streamDraftFingerprint() {
  return streamDraft.content + '\u0000' + streamDraft.thinking + '\u0000' + JSON.stringify(streamDraft.tools);
}

// The draft the page streamed, handed to the committed message that closes it. The caller resets the
// accumulation once that message lands.
function takeStreamedDraft() {
  return {content: streamDraft.content, thinking: streamDraft.thinking, tools: streamDraft.tools};
}

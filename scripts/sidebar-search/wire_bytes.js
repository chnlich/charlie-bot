// Wire bytes of the requests one page makes, from its CDP network events, read at any time window.
//
// Chrome 153 gives no usable wire size per data chunk: `Network.dataReceived.encodedDataLength` is 0 for some
// chunks and partial for others. The wire total of a finished request is `Network.loadingFinished.encodedDataLength`:
// its headers, its body and the chunk framing. That total is split across the request's data chunks in proportion to
// each chunk's decoded length (`Network.dataReceived.dataLength`), which is the encoded-to-decoded ratio of this
// response, so a window reads the part of the total that arrived inside it. A chunk's bytes take the arrival time of
// the chunk.
//
// A request the page aborts, or one still loading, has no total. Its bytes are the decoded body it received before it
// ended. For an identity body that is the body without headers and framing, a lower bound. For an encoded body (see the
// record's contentEncoding) it is an upper bound, since the wire body is smaller than its decoded form.
'use strict';

// Records every request the page makes from the time the session's Network domain is on. Each record keeps its URL,
// its category (from `categorize`), the wall-clock times it was sent and ended, how it ended (finished, aborted by the
// page, failed, or pending), its response's content-encoding, Chrome's wire total when it finished, and each data
// chunk's arrival time with its decoded length. Times are this process's wall clock, the clock of the page's input
// events.
function recordNetwork(cdp, categorize) {
  const byId = new Map();
  const list = [];
  cdp.on('Network.requestWillBeSent', (event) => {
    const record = {
      url: event.request.url,
      category: categorize(event.request.url),
      sentWall: Date.now(),
      endWall: null,
      outcome: 'pending',
      contentEncoding: null,
      chromeTotal: null,
      chunks: [],
    };
    byId.set(event.requestId, record);
    list.push(record);
  });
  cdp.on('Network.responseReceived', (event) => {
    const record = byId.get(event.requestId);
    if (!record) return;
    const entry = Object.entries(event.response.headers).find(([name]) => name.toLowerCase() === 'content-encoding');
    record.contentEncoding = entry ? entry[1].trim().toLowerCase() : null;
  });
  cdp.on('Network.dataReceived', (event) => {
    const record = byId.get(event.requestId);
    if (record) record.chunks.push({ wall: Date.now(), decoded: event.dataLength });
  });
  cdp.on('Network.loadingFinished', (event) => {
    const record = byId.get(event.requestId);
    if (!record) return;
    record.endWall = Date.now();
    record.outcome = 'finished';
    record.chromeTotal = event.encodedDataLength;
  });
  cdp.on('Network.loadingFailed', (event) => {
    const record = byId.get(event.requestId);
    if (!record) return;
    record.endWall = Date.now();
    record.outcome = event.canceled ? 'aborted' : 'failed';
  });
  return list;
}

function decodedTotal(record) {
  return record.chunks.reduce((sum, chunk) => sum + chunk.decoded, 0);
}

// The wire bytes of a request by chunk: `{wall, bytes}` for each chunk, in arrival order. A finished request with no
// decoded body (headers only) has its whole total at its end.
function wireChunks(record) {
  if (record.outcome !== 'finished') {
    return record.chunks.map((chunk) => ({ wall: chunk.wall, bytes: chunk.decoded }));
  }
  const decoded = decodedTotal(record);
  if (decoded === 0) return record.chromeTotal > 0 ? [{ wall: record.endWall, bytes: record.chromeTotal }] : [];
  return record.chunks.map((chunk) => ({ wall: chunk.wall, bytes: record.chromeTotal * chunk.decoded / decoded }));
}

// The request's wire bytes that arrived after `from` and at or before `to`.
function wireBytesIn(record, from = -Infinity, to = Infinity) {
  return wireChunks(record)
    .filter((chunk) => chunk.wall > from && chunk.wall <= to)
    .reduce((sum, chunk) => sum + chunk.bytes, 0);
}

// The request's wire bytes in all: Chrome's total when it finished, else the decoded body it received.
function wireTotal(record) {
  return wireBytesIn(record);
}

module.exports = { recordNetwork, decodedTotal, wireChunks, wireBytesIn, wireTotal };

// ---------------------------------------------------------------------------
// Fake WebSocket stand-in shared by the websocket.js vm tests. The class keeps
// one `instances` array per build, so each build creates a fresh class via
// `createFakeWebSocketClass()` and asserts against that build's sockets.
// ---------------------------------------------------------------------------
function createFakeWebSocketClass() {
  class FakeWebSocket {
    static instances = [];

    constructor(url) {
      this.url = url;
      this.sent = [];
      this.closed = false;
      this.onopen = null;
      this.onmessage = null;
      this.onclose = null;
      this.onerror = null;
      FakeWebSocket.instances.push(this);
    }

    send(payload) {
      this.sent.push(payload);
    }

    close() {
      this.closed = true;
    }

    emitOpen() {
      if (this.onopen) this.onopen();
    }

    emitClose() {
      if (this.onclose) this.onclose();
    }

    emitMessage(data) {
      if (this.onmessage) this.onmessage({data: JSON.stringify(data)});
    }
  }

  return FakeWebSocket;
}

module.exports = {createFakeWebSocketClass};

// The shaping proxy: an HTTP CONNECT proxy that carries every tunnel of the browser over one shaped link.
//
// The link has one bandwidth budget per direction, shared by every tunnel. Tunnels take turns one quantum at a time, as
// TCP flows share a bottleneck, so a background download and a switch split the link. Bytes within one tunnel keep their
// order, as one TCP stream does, so a switch's response waits behind the bytes its own connection queued before it.
// Each direction adds half the round trip to every byte. A delay spike holds both directions: the link starts no byte
// while a hold is on.
//
// A blackout drops packets. No byte crosses the link while it runs, and a connection attempt that starts inside it is
// lost: its CONNECT is never answered, not even at the restore, so the browser retries on its own schedule rather than
// waiting for a held answer. A `hold` blackout keeps the established tunnels: their bytes stop with the link and travel
// again at the restore, as a real link's TCP retransmission does. A `reset` blackout also resets every tunnel when it
// starts, so the browser sees its connections close.
//
// Bytes are counted per direction where they cross the link: `sent` is browser to server, `received` is server to
// browser. The CONNECT answer counts as received, because it crosses the link too.

import net from 'node:net';

import { LINK } from './profile.mjs';

// The link starts one quantum at a time, so a hold stops it within one quantum's time.
const QUANTUM_BYTES = 1024;
// The bytes one tunnel may queue in one direction before its source pauses. TCP's window plays this role on a link.
const WINDOW_BYTES = 64 * 1024;
const CONNECT_RESPONSE = Buffer.from('HTTP/1.1 200 Connection Established\r\n\r\n', 'latin1');
const MAX_REQUEST_HEAD_BYTES = 8192;

class Direction {
  name;
  // Each tunnel's queue of {conn, buf, off} for data and {conn, end: true} for the end of its stream, in order.
  queues = new Map();
  // The tunnels with queued items, in the order the link serves them.
  ring = [];
  nextFreeAt = 0;
  timer = null;
  bytes = 0;

  constructor(name) {
    this.name = name;
  }
}

function resetSocket(socket) {
  if (!socket.destroyed) {
    socket.resetAndDestroy();
  }
}

export class ShapingProxy {
  #target;
  #bytesPerSecond;
  #oneWayMs;
  #spikeEveryMs;
  #spikeMs;
  #spikeOffsetMs;
  #startedAt = 0;
  #server = null;
  #dirs = { sent: new Direction('sent'), received: new Direction('received') };
  // The routes of the CONNECT authorities other than the target: `host:port` -> {host, port}. Each route shares the
  // link's budget, so a route's bytes cross the shaped link as the target's do.
  #routes;
  // The scheduled holds (blackouts), each {start, end} in performance.now() milliseconds. A hold drops connection
  // attempts and stops the link's bytes.
  #holds = [];
  // Every socket the proxy holds, including a client whose tunnel is not up yet.
  #sockets = new Set();
  // The tunnels that are up.
  #conns = new Set();
  #timers = new Set();

  // `target` is the server the tunnels reach: {host, port}. `routes` maps other CONNECT authorities (`host:port`) to
  // the servers they reach. `link` takes the profile's fields.
  constructor({ target, routes = {}, link = LINK }) {
    this.#target = target;
    this.#routes = new Map(Object.entries(routes));
    this.#bytesPerSecond = link.bytesPerSecond;
    this.#oneWayMs = link.roundTripMs / 2;
    this.#spikeEveryMs = link.spikeEverySeconds * 1000;
    this.#spikeMs = link.spikeSeconds * 1000;
    // The first spike comes half a period after the proxy starts, so a measurement that starts at once sees none.
    this.#spikeOffsetMs = this.#spikeEveryMs / 2;
  }

  // Listens on loopback and returns the port the browser's proxy setting names.
  async start() {
    this.#server = net.createServer({ allowHalfOpen: true }, (client) => this.#onClient(client));
    await new Promise((resolve, reject) => {
      this.#server.once('error', reject);
      this.#server.listen(0, '127.0.0.1', resolve);
    });
    this.#startedAt = performance.now();
    return this.#server.address().port;
  }

  // The bytes that have crossed the link so far, per direction.
  counters() {
    return { sent: this.#dirs.sent.bytes, received: this.#dirs.received.bytes };
  }

  // Drops every packet for `seconds` from now. The `hold` mode keeps the established tunnels: their bytes stop and
  // travel again at the restore. The `reset` mode also resets every tunnel at the start, so the browser sees its
  // connections close. In both modes an attempt still waiting for its CONNECT answer is lost.
  blackout({ seconds = LINK.blackoutSeconds, mode }) {
    this.schedule([{ at: 0, seconds, mode }]);
  }

  // Schedules holds. Each window is {at, seconds, mode}: it starts `at` milliseconds from now and lasts `seconds`, with
  // the mode blackout() takes. A reset window resets every tunnel when it starts.
  schedule(windows) {
    const now = performance.now();
    for (const { at, seconds, mode } of windows) {
      if (mode !== 'hold' && mode !== 'reset') {
        throw new Error(`blackout mode must be hold or reset, got ${mode}`);
      }
      const start = now + at;
      this.#holds.push({ start, end: start + seconds * 1000 });
      this.#holdStartsAt(start, mode);
    }
    for (const dir of Object.values(this.#dirs)) {
      this.#pump(dir);
    }
  }

  // Applies a window's start at `at` (performance.now() milliseconds), now when that time has come. A reset window
  // resets every tunnel; a hold window drops every attempt still waiting for its CONNECT answer.
  #holdStartsAt(at, mode) {
    const begin = () => {
      for (const conn of [...this.#conns]) {
        if (mode === 'reset') {
          this.#reset(conn);
        } else if (!conn.established) {
          this.#dropAttempt(conn);
        }
      }
    };
    const delay = at - performance.now();
    if (delay <= 0) {
      begin();
      return;
    }
    const timer = setTimeout(() => {
      this.#timers.delete(timer);
      begin();
    }, delay);
    this.#timers.add(timer);
  }

  // The end of the delay spike or blackout that overlaps the window [at, at + windowMs), or 0 when the link is clear
  // for that whole window. A step that starts at `at` and takes up to `windowMs` waits out a hold it would otherwise
  // measure.
  holdEndWithin(at, windowMs) {
    let end = 0;
    for (const hold of this.#holds) {
      if (hold.start < at + windowMs && hold.end > at) {
        end = Math.max(end, hold.end);
      }
    }
    // Spikes are a period apart and a period outlasts any window, so the spike at or before `at` and the next one are
    // the only candidates: the next one starts inside the window even when the window's first moment is clear.
    const first = Math.max(0, Math.floor((at - this.#startedAt - this.#spikeOffsetMs) / this.#spikeEveryMs));
    for (const index of [first, first + 1]) {
      const start = this.#startedAt + this.#spikeOffsetMs + index * this.#spikeEveryMs;
      if (start < at + windowMs && start + this.#spikeMs > at) {
        end = Math.max(end, start + this.#spikeMs);
      }
    }
    return end;
  }

  // Whether a scheduled blackout covers `at`. A delay spike is not a blackout: it holds bytes and leaves connection
  // attempts alone.
  #inBlackout(at) {
    return this.#holds.some((hold) => at >= hold.start && at < hold.end);
  }

  // The time at or after `at` when the link may start a byte. Skips every hold that covers `at`.
  #openAt(at) {
    let open = at;
    for (let hold = this.#holdCovering(open); hold; hold = this.#holdCovering(open)) {
      open = hold.end;
    }
    return open;
  }

  #holdCovering(at) {
    const hold = this.#holds.find((candidate) => at >= candidate.start && at < candidate.end);
    if (hold) {
      return hold;
    }
    const sinceStart = at - this.#startedAt;
    if (sinceStart < this.#spikeOffsetMs) {
      return null;
    }
    const index = Math.floor((sinceStart - this.#spikeOffsetMs) / this.#spikeEveryMs);
    const start = this.#startedAt + this.#spikeOffsetMs + index * this.#spikeEveryMs;
    return at >= start && at < start + this.#spikeMs ? { start, end: start + this.#spikeMs } : null;
  }

  #onClient(client) {
    this.#sockets.add(client);
    client.setNoDelay(true);
    client.on('error', () => client.destroy());
    client.on('close', () => this.#sockets.delete(client));
    let head = Buffer.alloc(0);
    const onData = (chunk) => {
      head = Buffer.concat([head, chunk]);
      const end = head.indexOf('\r\n\r\n');
      if (end < 0) {
        if (head.length > MAX_REQUEST_HEAD_BYTES) {
          client.destroy(new Error('CONNECT request head too long'));
        }
        return;
      }
      client.off('data', onData);
      client.pause();
      this.#connect(client, head.subarray(0, end).toString('latin1'), head.subarray(end + 4));
    };
    client.on('data', onData);
  }

  #connect(client, requestHead, rest) {
    const match = /^CONNECT ([^ ]+) HTTP\/1\.[01]$/.exec(requestHead.split('\r\n')[0]);
    const authority = match ? match[1] : '';
    const port = Number(authority.split(':')[1]);
    // Only the bench's routes are reachable through the proxy, so no other traffic reaches the link.
    const route = this.#routes.get(authority) ?? (port === this.#target.port ? this.#target : null);
    if (route === null) {
      client.end('HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n');
      return;
    }
    // A blackout drops packets, the SYN of a new connection included. The attempt never reaches the target and its
    // CONNECT is never answered, not even at the restore, so the browser retries on its own schedule. The client socket
    // stays open and silent until the browser gives up on it.
    if (this.#inBlackout(performance.now())) {
      client.resume();
      return;
    }
    const upstream = net.connect({ host: route.host, port: route.port, allowHalfOpen: true });
    upstream.setNoDelay(true);
    this.#sockets.add(upstream);
    upstream.on('close', () => this.#sockets.delete(upstream));
    const conn = {
      client,
      upstream,
      dead: false,
      // Whether the CONNECT answer has reached the browser. Until it has, the tunnel's bytes are a connection attempt's
      // packets, which a blackout drops; after it, they belong to an established connection, which a blackout only stops.
      established: false,
      pending: { sent: 0, received: 0 },
      paused: { sent: false, received: false },
    };
    // A half-close travels as an end marker behind the bytes before it. The browser's close ends the tunnel.
    client.on('end', () => this.#enqueue('sent', { conn, end: true }));
    upstream.on('end', () => this.#enqueue('received', { conn, end: true }));
    client.on('error', () => this.#reset(conn));
    upstream.on('error', () => this.#reset(conn));
    client.on('close', () => this.#retire(conn));
    upstream.once('connect', () => {
      this.#conns.add(conn);
      this.#enqueue('received', { conn, buf: CONNECT_RESPONSE, connectAnswer: true });
      if (rest.length > 0) {
        this.#enqueue('sent', { conn, buf: rest });
      }
      client.on('data', (chunk) => this.#enqueue('sent', { conn, buf: chunk }));
      upstream.on('data', (chunk) => this.#enqueue('received', { conn, buf: chunk }));
      client.resume();
    });
  }

  #enqueue(dirName, item) {
    const dir = this.#dirs[dirName];
    if (item.conn.dead) {
      return;
    }
    if (!item.end) {
      const { conn, buf } = item;
      conn.pending[dirName] += buf.length;
      if (!conn.paused[dirName] && conn.pending[dirName] >= WINDOW_BYTES) {
        this.#source(conn, dirName).pause();
        conn.paused[dirName] = true;
      }
      item = { conn, buf, off: 0, connectAnswer: item.connectAnswer === true };
    }
    let queue = dir.queues.get(item.conn);
    if (!queue) {
      queue = [];
      dir.queues.set(item.conn, queue);
      dir.ring.push(item.conn);
    }
    queue.push(item);
    if (!dir.timer) {
      this.#pump(dir);
    }
  }

  // The socket that feeds a direction: the browser's for `sent`, the server's for `received`.
  #source(conn, dirName) {
    return dirName === 'sent' ? conn.client : conn.upstream;
  }

  #destination(conn, dirName) {
    return dirName === 'sent' ? conn.upstream : conn.client;
  }

  // Sends one quantum of the tunnel at the head of the ring, then moves that tunnel to the back. Each quantum arrives one
  // half round trip after it leaves the link. An end marker goes out after the tunnel's bytes before it.
  #pump(dir) {
    if (dir.timer) {
      clearTimeout(dir.timer);
      dir.timer = null;
    }
    for (;;) {
      if (dir.ring.length === 0) {
        return;
      }
      const now = performance.now();
      const freeAt = Math.max(now, dir.nextFreeAt);
      const conn = dir.ring[0];
      // A blackout drops a connection attempt's packets: a tunnel whose CONNECT answer has not reached the browser is
      // lost, not held to the restore, so the browser's own retry decides the recovery. A byte whose half round trip
      // lands inside the blackout is in flight when the packets drop, and is lost too.
      if (!conn.established && (this.#inBlackout(freeAt) || this.#inBlackout(freeAt + this.#oneWayMs))) {
        this.#dropAttempt(conn);
        continue;
      }
      const openAt = this.#openAt(freeAt);
      if (openAt > now) {
        this.#arm(dir, openAt - now);
        return;
      }
      const queue = dir.queues.get(conn);
      const item = queue[0];
      if (item.end) {
        queue.shift();
        this.#deliverAt(freeAt + this.#oneWayMs, { conn, dirName: dir.name, end: true });
        this.#advance(dir, conn, queue);
        continue;
      }
      const size = Math.min(QUANTUM_BYTES, item.buf.length - item.off);
      const chunk = item.buf.subarray(item.off, item.off + size);
      const connectAnswer = item.connectAnswer === true;
      item.off += size;
      if (item.off >= item.buf.length) {
        queue.shift();
      }
      dir.nextFreeAt = freeAt + (size * 1000) / this.#bytesPerSecond;
      dir.bytes += size;
      this.#deliverAt(dir.nextFreeAt + this.#oneWayMs, { conn, dirName: dir.name, buf: chunk, connectAnswer });
      this.#advance(dir, conn, queue);
      if (dir.ring.length > 0) {
        this.#arm(dir, dir.nextFreeAt - now);
      }
      return;
    }
  }

  // After a tunnel's turn: it leaves the ring when its queue is empty, and goes to the back otherwise.
  #advance(dir, conn, queue) {
    if (queue.length === 0) {
      dir.queues.delete(conn);
      dir.ring.shift();
    } else {
      dir.ring.push(dir.ring.shift());
    }
  }

  #arm(dir, delayMs) {
    dir.timer = setTimeout(() => {
      dir.timer = null;
      this.#pump(dir);
    }, delayMs);
  }

  #deliverAt(atMs, payload) {
    const timer = setTimeout(() => {
      this.#timers.delete(timer);
      this.#deliver(payload);
    }, Math.max(0, atMs - performance.now()));
    this.#timers.add(timer);
  }

  #deliver({ conn, dirName, buf, end, connectAnswer }) {
    if (conn.dead) {
      return;
    }
    if (connectAnswer) {
      conn.established = true;
    }
    const destination = this.#destination(conn, dirName);
    if (end) {
      if (!destination.destroyed) {
        destination.end();
      }
      return;
    }
    conn.pending[dirName] -= buf.length;
    if (conn.paused[dirName] && conn.pending[dirName] < WINDOW_BYTES / 2) {
      this.#source(conn, dirName).resume();
      conn.paused[dirName] = false;
    }
    if (!destination.destroyed) {
      destination.write(buf);
    }
  }

  #forget(conn) {
    conn.dead = true;
    this.#conns.delete(conn);
    for (const dir of Object.values(this.#dirs)) {
      dir.queues.delete(conn);
      dir.ring = dir.ring.filter((queued) => queued !== conn);
    }
  }

  #reset(conn) {
    if (conn.dead) {
      return;
    }
    this.#forget(conn);
    resetSocket(conn.client);
    resetSocket(conn.upstream);
  }

  // Drops a connection attempt: the link loses its packets, so it never reaches the target and the browser gets no
  // answer, not even at the restore. The client socket stays open and silent, its bytes discarded, until the browser's
  // own connect timeout ends the attempt.
  #dropAttempt(conn) {
    if (conn.dead) {
      return;
    }
    this.#forget(conn);
    conn.client.pause();
    if (!conn.upstream.destroyed) {
      conn.upstream.destroy();
    }
  }

  #retire(conn) {
    this.#forget(conn);
    conn.client.destroy();
    conn.upstream.destroy();
  }

  // Closes the proxy, every tunnel and every timer, so a stopped proxy holds no socket and no timer.
  async stop() {
    for (const timer of this.#timers) {
      clearTimeout(timer);
    }
    this.#timers.clear();
    for (const dir of Object.values(this.#dirs)) {
      if (dir.timer) {
        clearTimeout(dir.timer);
        dir.timer = null;
      }
    }
    for (const socket of this.#sockets) {
      socket.destroy();
    }
    this.#sockets.clear();
    this.#conns.clear();
    await new Promise((resolve) => this.#server.close(resolve));
  }
}

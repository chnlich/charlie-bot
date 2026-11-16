import { spawnSync } from 'node:child_process';

function iptables(args, missingRuleIsOk = false) {
  const result = spawnSync('sudo', ['-n', 'iptables', '-w', ...args], { encoding: 'utf8' });
  if (result.error) {
    throw result.error;
  }
  if (missingRuleIsOk && result.status === 1) {
    return false;
  }
  if (result.status !== 0) {
    throw new Error(
      `sudo iptables ${args.join(' ')} exited ${result.status}: ${result.stderr.trim()}`,
    );
  }
  return true;
}

// Drops only new TCP SYNs from this host to the bench's loopback proxy. Linux then retries each connection attempt
// with its normal TCP timer, and a retransmission after the blackout reaches the proxy.
export class LoopbackSynDropper {
  #port;
  #active = false;

  constructor(port) {
    this.#port = port;
  }

  #rule() {
    return [
      '-p', 'tcp',
      '-d', '127.0.0.1/32',
      '--dport', String(this.#port),
      '--tcp-flags', 'SYN,RST,ACK', 'SYN',
      '-j', 'DROP',
    ];
  }

  drop() {
    if (this.#active) {
      throw new Error(`the loopback SYN drop for port ${this.#port} is already active`);
    }
    iptables(['-L', 'OUTPUT', '-n']);
    if (iptables(['-C', 'OUTPUT', ...this.#rule()], true)) {
      throw new Error(`the loopback SYN drop rule for port ${this.#port} already exists`);
    }
    iptables(['-I', 'OUTPUT', '1', ...this.#rule()]);
    this.#active = true;
  }

  restore() {
    if (!this.#active) {
      throw new Error(`the loopback SYN drop for port ${this.#port} is not active`);
    }
    iptables(['-D', 'OUTPUT', ...this.#rule()]);
    this.#active = false;
  }
}

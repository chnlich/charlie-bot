// The shaping proxy as a command, for the perf loop's slow-network measure. It carries one trial server's tunnels over
// the bench's link: the same ShapingProxy at the link in profile.mjs, so the loads and the bench's scenarios share one
// link model.
//
//   node dev/poor-network/relay.mjs --target "$CHARLIEBOT_SERVE_URL"
//
// Once it listens, the relay prints its own address, `http://127.0.0.1:<port>`, as its first stdout line, and it runs
// until SIGINT or SIGTERM, which stops it and its tunnels. Its tunnels reach the target's port only.

import { parseArgs } from 'node:util';

import { ShapingProxy } from './proxy.mjs';

function parseTarget(argv) {
  const { values } = parseArgs({ args: argv, options: { target: { type: 'string' } }, strict: true });
  if (values.target === undefined) {
    throw new Error("--target is required: the trial server's http://<host>:<port>");
  }
  const url = new URL(values.target);
  if (url.protocol !== 'http:' || url.port === '' || url.pathname !== '/') {
    throw new Error(`--target must be http://<host>:<port> with no path, got ${values.target}`);
  }
  return { host: url.hostname, port: Number(url.port) };
}

async function main() {
  const target = parseTarget(process.argv.slice(2));
  const proxy = new ShapingProxy({ target });
  const port = await proxy.start();
  process.stdout.write(`http://127.0.0.1:${port}\n`);
  await new Promise((resolve) => {
    process.once('SIGINT', resolve);
    process.once('SIGTERM', resolve);
  });
  await proxy.stop();
}

await main();

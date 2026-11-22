// The backend a call runs on, read through `charliebot backend resolve`: the server picks the option, and this module
// reads its endpoint, model and credential. A refusal (an unset call class, an unknown id, no selectable member) is the
// CLI's own error text, which names the key to set; it is passed on unchanged.

import { spawnSync } from 'node:child_process';

import { readCredential } from './gateway.mjs';

// The CLI that resolves. `CHARLIEBOT_BIN` names another one, as taste-replay's does.
function charliebotBin() {
  return process.env.CHARLIEBOT_BIN || 'charliebot';
}

function resolve(args, subject) {
  const bin = charliebotBin();
  const result = spawnSync(bin, ['backend', 'resolve', ...args], { encoding: 'utf8' });
  if (result.error) throw new Error(`cannot run ${bin}: ${result.error.message}`);
  if (result.status !== 0) throw new Error(`${subject}: ${result.stderr.trim()}`);
  const { option } = JSON.parse(result.stdout);
  return option;
}

// The option record a backends.calls class selects.
export function resolveCall(name) {
  return resolve(['--call', name], `backends.calls.${name}`);
}

// The option record a backends.options or backends.pools id selects.
export function resolveBackend(id) {
  return resolve(['--backend', id], `backend ${id}`);
}

// The gateway call an option makes: its endpoint, its model, and the key its credential names in credentials.yaml.
// An option without one of the three fails here, naming the field it lacks.
export function gatewayCall(option, credentialsPath) {
  for (const field of ['api_base', 'model', 'credential']) {
    if (typeof option[field] !== 'string' || option[field] === '') {
      throw new Error(`backend ${option.id} names no ${field}; a gateway call needs api_base, model and credential`);
    }
  }
  return {
    id: option.id,
    base: option.api_base,
    model: option.model,
    key: readCredential(credentialsPath, option.credential),
  };
}

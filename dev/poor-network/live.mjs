// The bench's live resources: each browser and each shaping proxy registers here when it starts and unregisters when it
// stops, so a step that runs past its cap can stop everything it started in one call (step-gate.mjs). A resource that
// stopped on its own unregisters itself, so the registry holds only what is still running.

const live = new Map();
let nextId = 1;

// trackLive(name, stop): register a running resource and return the function that unregisters it. `stop` is async and
// may run after the resource already stopped, so it must be idempotent.
export function trackLive(name, stop) {
  const id = nextId++;
  live.set(id, { name, stop });
  return () => {
    live.delete(id);
  };
}

// closeAllLive(): stop every registered resource, in the order it started. A resource that fails to stop fails the
// call, after the others have been asked to stop, so a caller sees every failure's name.
export async function closeAllLive() {
  const failures = [];
  for (const [id, resource] of [...live]) {
    live.delete(id);
    try {
      await resource.stop();
    } catch (error) {
      failures.push(`${resource.name}: ${error.message}`);
    }
  }
  if (failures.length > 0) {
    throw new Error(`could not stop: ${failures.join('; ')}`);
  }
}

// The names of the registered resources, for the record of a step that was stopped.
export function liveNames() {
  return [...live.values()].map((resource) => resource.name);
}

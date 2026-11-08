// The serve trials that run on this host beside the bench's own. A trial that overlaps a repetition shares the host
// with it and puts a mode of about 3 s into the switch and send timings, while byte counts stay as they are. The watch
// samples the host's trials every 30 s, so each repetition records the trials that ran beside it.

import { execFileSync } from 'node:child_process';

const SAMPLE_MS = 30000;
// The build host lists thousands of processes, some with command lines of many kilobytes.
const PS_MAX_BYTES = 64 * 1024 * 1024;
// A process that runs the trial script, directly or as bash's script: a command that only names it, such as a flock
// that waits to start one, is not a trial yet.
const TRIAL_COMMAND = /^(\S*\bbash\s+)?\S*scripts\/release\/trial\.sh(\s|$)/;

// Every process as {pid, ppid, args}.
function processes() {
  const out = execFileSync('ps', ['-eo', 'pid=,ppid=,args='], { encoding: 'utf8', maxBuffer: PS_MAX_BYTES });
  return out
    .split('\n')
    .filter((line) => line.trim().length > 0)
    .map((line) => {
      const [, pid, ppid, args] = line.match(/^\s*(\d+)\s+(\d+)\s+(.*)$/);
      return { pid: Number(pid), ppid: Number(ppid), args };
    });
}

// A trial's name: the tree root it runs in, or its command line when it names none.
function trialName(args) {
  return args.match(/--tree-root[ =](\S+)/)?.[1] ?? args.slice(0, 160);
}

export class TrialWatch {
  // The bench's own ancestors, its own trial among them.
  #own;
  // {at, trials}: the other trials seen at each sample.
  #samples = [];
  #timer = null;

  start() {
    const parentOf = new Map(processes().map((entry) => [entry.pid, entry.ppid]));
    this.#own = new Set();
    for (let pid = process.pid; pid > 1 && !this.#own.has(pid); pid = parentOf.get(pid) ?? 1) {
      this.#own.add(pid);
    }
    this.#sample();
    this.#timer = setInterval(() => this.#sample(), SAMPLE_MS);
    // The watch never holds the bench open once its work is done.
    this.#timer.unref();
  }

  stop() {
    clearInterval(this.#timer);
  }

  #sample() {
    const trials = processes()
      .filter((entry) => TRIAL_COMMAND.test(entry.args) && !this.#own.has(entry.pid))
      .map((entry) => trialName(entry.args));
    this.#samples.push({ at: Date.now(), trials: [...new Set(trials)].sort() });
  }

  // The other trials seen in [from, now], sampled once more now, so a repetition shorter than the interval still has
  // a sample at each end.
  since(from) {
    this.#sample();
    const seen = new Set();
    for (const sample of this.#samples) {
      if (sample.at >= from) {
        sample.trials.forEach((name) => seen.add(name));
      }
    }
    return [...seen].sort();
  }

  // A sample at the start of a repetition, so its first moments are seen.
  mark() {
    const at = Date.now();
    this.#sample();
    return at;
  }
}

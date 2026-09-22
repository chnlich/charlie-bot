// ---------------------------------------------------------------------------
// mulberry32 — the deterministic PRNG the randomized property tests share.
// A failure reproduces from the seed in the assertion message, and every
// consumer must see the same sequence per seed, so this file is the only copy.
// ---------------------------------------------------------------------------
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

module.exports = {mulberry32};

//! The seeded pseudo-random generator of the synthetic data.
//!
//! SplitMix64 expands the seed into the state of xoshiro256**. The generator is part of the data's definition: any change
//! to it changes every byte, so it lives here instead of in a crate whose output could change under a dependency update.

/// A xoshiro256** generator. Its output depends only on the seed and this file.
pub struct Rng {
  state: [u64; 4],
}

impl Rng {
  pub fn new(seed: u64) -> Self {
    let mut split = seed;
    let state = std::array::from_fn(|_| splitmix64(&mut split));
    Self { state }
  }

  pub fn next_u64(&mut self) -> u64 {
    let result = self.state[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
    let shifted = self.state[1] << 17;
    self.state[2] ^= self.state[0];
    self.state[3] ^= self.state[1];
    self.state[1] ^= self.state[2];
    self.state[0] ^= self.state[3];
    self.state[2] ^= shifted;
    self.state[3] = self.state[3].rotate_left(45);
    result
  }

  /// A uniform number in [0, 1).
  pub fn unit(&mut self) -> f64 {
    (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
  }

  /// A uniform integer in [0, n). `n` is at least 1.
  pub fn below(&mut self, n: u64) -> u64 {
    ((self.next_u64() as u128 * n as u128) >> 64) as u64
  }

  /// A uniform integer in [low, high], both ends included.
  pub fn range(&mut self, low: u64, high: u64) -> u64 {
    low + self.below(high - low + 1)
  }

  /// An item of `items`, each one equally likely. `items` is not empty.
  pub fn choose<'a, T>(&mut self, items: &'a [T]) -> &'a T {
    &items[self.below(items.len() as u64) as usize]
  }

  /// Shuffles `items` in place with the Fisher-Yates method.
  pub fn shuffle<T>(&mut self, items: &mut [T]) {
    for index in (1..items.len()).rev() {
      items.swap(index, self.below(index as u64 + 1) as usize);
    }
  }
}

fn splitmix64(state: &mut u64) -> u64 {
  *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
  let mut value = *state;
  value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
  value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
  value ^ (value >> 31)
}

use std::collections::HashMap;
use std::hash::Hash;

/// A contiguous equal block with indices into the two compared sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchingBlock {
  pub a: usize,
  pub b: usize,
  pub size: usize,
}

/// An operation that transforms one sequence slice into another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opcode {
  pub tag: OpcodeTag,
  pub a_start: usize,
  pub a_end: usize,
  pub b_start: usize,
  pub b_end: usize,
}

/// The four operation tags returned by `SequenceMatcher.get_opcodes()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpcodeTag {
  Replace,
  Delete,
  Insert,
  Equal,
}

/// Find the earliest longest equal block within the supplied half-open ranges.
pub fn find_longest_match<T: Eq + Hash>(
  a: &[T],
  b: &[T],
  a_start: usize,
  a_end: usize,
  b_start: usize,
  b_end: usize,
) -> MatchingBlock {
  let b_positions = build_b_positions(b);
  find_longest_match_with_positions(a, b, a_start, a_end, b_start, b_end, &b_positions)
}

fn find_longest_match_with_positions<T: Eq + Hash>(
  a: &[T],
  b: &[T],
  a_start: usize,
  a_end: usize,
  b_start: usize,
  b_end: usize,
  b_positions: &HashMap<&T, Vec<usize>>,
) -> MatchingBlock {
  let mut best_a = a_start;
  let mut best_b = b_start;
  let mut best_size = 0;
  let mut previous_lengths = HashMap::new();
  let mut current_lengths = HashMap::new();
  for (a_index, a_item) in a.iter().enumerate().take(a_end).skip(a_start) {
    current_lengths.clear();
    if let Some(indices) = b_positions.get(a_item) {
      for &b_index in indices {
        if b_index < b_start {
          continue;
        }
        if b_index >= b_end {
          break;
        }
        let size = previous_lengths.get(&b_index.wrapping_sub(1)).copied().unwrap_or(0) + 1;
        current_lengths.insert(b_index, size);
        if size > best_size {
          best_a = a_index + 1 - size;
          best_b = b_index + 1 - size;
          best_size = size;
        }
      }
    }
    std::mem::swap(&mut previous_lengths, &mut current_lengths);
  }

  while best_a > a_start && best_b > b_start && a[best_a - 1] == b[best_b - 1] {
    best_a -= 1;
    best_b -= 1;
    best_size += 1;
  }
  while best_a + best_size < a_end && best_b + best_size < b_end && a[best_a + best_size] == b[best_b + best_size] {
    best_size += 1;
  }
  MatchingBlock { a: best_a, b: best_b, size: best_size }
}

/// Return non-adjacent matching blocks followed by the `SequenceMatcher` sentinel.
pub fn matching_blocks<T: Eq + Hash>(a: &[T], b: &[T]) -> Vec<MatchingBlock> {
  let b_positions = build_b_positions(b);
  let mut queue = vec![(0, a.len(), 0, b.len())];
  let mut blocks = Vec::new();
  while let Some((a_start, a_end, b_start, b_end)) = queue.pop() {
    let block = find_longest_match_with_positions(a, b, a_start, a_end, b_start, b_end, &b_positions);
    let i = block.a;
    let j = block.b;
    let size = block.size;
    if size != 0 {
      blocks.push(block);
      if a_start < i && b_start < j {
        queue.push((a_start, i, b_start, j));
      }
      if i + size < a_end && j + size < b_end {
        queue.push((i + size, a_end, j + size, b_end));
      }
    }
  }
  blocks.sort_by_key(|block| (block.a, block.b, block.size));

  let mut collapsed = Vec::new();
  let (mut previous_a, mut previous_b, mut previous_size) = (0, 0, 0);
  for block in blocks {
    if previous_a + previous_size == block.a && previous_b + previous_size == block.b {
      previous_size += block.size;
    } else {
      if previous_size != 0 {
        collapsed.push(MatchingBlock { a: previous_a, b: previous_b, size: previous_size });
      }
      previous_a = block.a;
      previous_b = block.b;
      previous_size = block.size;
    }
  }
  if previous_size != 0 {
    collapsed.push(MatchingBlock { a: previous_a, b: previous_b, size: previous_size });
  }
  collapsed.push(MatchingBlock { a: a.len(), b: b.len(), size: 0 });
  collapsed
}

fn build_b_positions<T: Eq + Hash>(b: &[T]) -> HashMap<&T, Vec<usize>> {
  let mut b_positions = HashMap::new();
  for (index, item) in b.iter().enumerate() {
    b_positions.entry(item).or_insert_with(Vec::new).push(index);
  }
  b_positions
}

/// Return CPython `SequenceMatcher(None, a, b, autojunk=False).get_opcodes()` results.
pub fn opcodes<T: Eq + Hash>(a: &[T], b: &[T]) -> Vec<Opcode> {
  let mut result = Vec::new();
  let (mut i, mut j) = (0, 0);
  for block in matching_blocks(a, b) {
    let tag = if i < block.a && j < block.b {
      Some(OpcodeTag::Replace)
    } else if i < block.a {
      Some(OpcodeTag::Delete)
    } else if j < block.b {
      Some(OpcodeTag::Insert)
    } else {
      None
    };
    if let Some(tag) = tag {
      result.push(Opcode { tag, a_start: i, a_end: block.a, b_start: j, b_end: block.b });
    }
    i = block.a + block.size;
    j = block.b + block.size;
    if block.size != 0 {
      result.push(Opcode { tag: OpcodeTag::Equal, a_start: block.a, a_end: i, b_start: block.b, b_end: j });
    }
  }
  result
}

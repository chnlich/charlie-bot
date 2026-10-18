//! CPython 3.14.7-compatible HTML, character-reference, and sequence-diff primitives.

mod entities;
mod parser;
mod sequence;

pub use entities::{escape, unescape};
pub use parser::{Event, EventKind, ParseError, Position, parse};
pub use sequence::{MatchingBlock, Opcode, OpcodeTag, find_longest_match, matching_blocks, opcodes};

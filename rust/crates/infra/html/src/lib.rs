//! CPython 3.14.7-compatible HTML, character-reference, and sequence-diff primitives.

mod entities;
mod fold;
mod parser;
mod sequence;

pub use entities::{escape, unescape};
pub use fold::folds_at_open;
pub use parser::{Event, EventKind, ParseError, Position, StreamEvent, parse, parse_with};
pub use sequence::{Opcode, OpcodeTag, opcodes};

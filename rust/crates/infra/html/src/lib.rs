//! HTML tokenizer, character-reference, and sequence-diff primitives.

mod elements;
mod entities;
mod fold;
mod parser;
mod sequence;

pub use elements::VOID_TAGS;
pub use entities::{escape, unescape};
pub use fold::folds_at_open;
pub use parser::{Event, EventKind, ParseError, StreamEvent, parse, parse_with};
pub use sequence::{Opcode, OpcodeTag, opcodes};

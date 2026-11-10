//! HTML tokenizer, character-reference, and page-open fold primitives.

mod elements;
mod entities;
mod fold;
mod parser;

pub use elements::VOID_TAGS;
pub use entities::{EscapeContext, escape, unescape};
pub use fold::folds_at_open;
pub use parser::{Event, EventKind, ParseError, StreamEvent, parse, parse_with};

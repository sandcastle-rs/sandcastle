//! Dockerfile parsing for sandcastle.
//!
//! `parse` turns Dockerfile text into `Node`s with the same rules as
//! BuildKit's `frontend/dockerfile/parser`; `Instruction::try_from` gives the
//! typed form of one node, and `expand` / `expand_words` perform BuildKit's
//! variable expansion and quote removal on an argument. The crate does no I/O.

#![forbid(unsafe_code)]

mod line_parsers;
mod parser;

pub use parser::{Dockerfile, Node, ParseError, ParseErrorKind, parse};

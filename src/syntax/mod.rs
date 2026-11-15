//! Roc syntax: lossless lexer, forgiving parser, and source-oriented AST.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod token;

pub use ast::{Module, ParsedFile};
pub use token::{Keyword, Token, TokenKind};

/// Parse a Roc source file.  Never panics on malformed input; errors are
/// collected in [`ParsedFile::diagnostics`] and the returned tree is partial.
pub fn parse(source: &str) -> ParsedFile {
    parser::parse(source)
}

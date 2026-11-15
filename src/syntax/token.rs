//! Lexical token definitions for Roc.
//!
//! The lexer is lossless: whitespace, newlines, comments, and doc comments are
//! tokens too.  Parsing uses a filtered view, while the server uses the full
//! token stream for semantic highlighting, folding, and completion context.

use crate::text::TextRange;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    App,
    Module,
    Package,
    Platform,
    Import,
    Exposing,
    As,
    If,
    Else,
    Match,
    For,
    In,
    While,
    Var,
    Return,
    Break,
    Crash,
    Dbg,
    Expect,
    Where,
    Requires,
    Exposes,
    Packages,
    Provides,
    Hosted,
    Targets,
    And,
    Or,
}

impl Keyword {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Option<Self> {
        Some(match text {
            "app" => Self::App,
            "module" => Self::Module,
            "package" => Self::Package,
            "platform" => Self::Platform,
            "import" => Self::Import,
            "exposing" => Self::Exposing,
            "as" => Self::As,
            "if" => Self::If,
            "else" => Self::Else,
            "match" => Self::Match,
            "for" => Self::For,
            "for!" => Self::For,
            "in" => Self::In,
            "while" => Self::While,
            "while!" => Self::While,
            "var" => Self::Var,
            "return" => Self::Return,
            "break" => Self::Break,
            "crash" => Self::Crash,
            "dbg" => Self::Dbg,
            "expect" => Self::Expect,
            "where" => Self::Where,
            "requires" => Self::Requires,
            "exposes" => Self::Exposes,
            "packages" => Self::Packages,
            "provides" => Self::Provides,
            "hosted" => Self::Hosted,
            "targets" => Self::Targets,
            "and" => Self::And,
            "or" => Self::Or,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Module => "module",
            Self::Package => "package",
            Self::Platform => "platform",
            Self::Import => "import",
            Self::Exposing => "exposing",
            Self::As => "as",
            Self::If => "if",
            Self::Else => "else",
            Self::Match => "match",
            Self::For => "for",
            Self::In => "in",
            Self::While => "while",
            Self::Var => "var",
            Self::Return => "return",
            Self::Break => "break",
            Self::Crash => "crash",
            Self::Dbg => "dbg",
            Self::Expect => "expect",
            Self::Where => "where",
            Self::Requires => "requires",
            Self::Exposes => "exposes",
            Self::Packages => "packages",
            Self::Provides => "provides",
            Self::Hosted => "hosted",
            Self::Targets => "targets",
            Self::And => "and",
            Self::Or => "or",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Whitespace,
    Newline,
    Comment,
    DocComment,

    LowerIdent,
    UpperIdent,
    Keyword,
    Number,
    String,
    MultilineString,
    Char,

    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Dot,
    DotDot,
    DotDotLess,
    DotDotEq,
    Colon,
    ColonColon,
    ColonEquals,
    Eq,
    Arrow,
    FatArrow,
    Pipe,
    Backslash,
    Question,
    OptionalQuestion,
    Bang,
    Dollar,
    Underscore,

    Operator,
    Error,
    Eof,
}

impl TokenKind {
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::Newline | Self::Comment | Self::DocComment
        )
    }

    pub fn is_comment(self) -> bool {
        matches!(self, Self::Comment | Self::DocComment)
    }

    pub fn is_literal(self) -> bool {
        matches!(
            self,
            Self::Number | Self::String | Self::MultilineString | Self::Char
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: TextRange,
    pub keyword: Option<Keyword>,
}

impl Token {
    pub fn new(kind: TokenKind, range: TextRange) -> Self {
        Self {
            kind,
            range,
            keyword: None,
        }
    }

    pub fn keyword(keyword: Keyword, range: TextRange) -> Self {
        Self {
            kind: TokenKind::Keyword,
            range,
            keyword: Some(keyword),
        }
    }

    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        let start = (self.range.start as usize).min(source.len());
        let end = (self.range.end as usize).min(source.len());
        let start = floor_char_boundary(source, start);
        let end = floor_char_boundary(source, end).max(start);
        &source[start..end]
    }

    pub fn is(&self, kind: TokenKind) -> bool {
        self.kind == kind
    }

    pub fn is_keyword(&self, keyword: Keyword) -> bool {
        self.keyword == Some(keyword)
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

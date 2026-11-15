//! Lossless lexer for Roc.
//!
//! Every byte of the input belongs to exactly one token.  Whitespace, newlines,
//! comments, and doc comments are tokens too, so tooling can reconstruct the
//! original source and implement comment-aware folding/semantic highlighting.
//!
//! The lexer is deliberately independent of the parser: it never reports errors
//! and always returns a usable stream (including an `Eof` token).  Malformed
//! literals are consumed up to a safe boundary; the parser turns the resulting
//! token spans into diagnostics.

use crate::syntax::token::{Keyword, Token, TokenKind};
use crate::text::TextRange;

/// Tokenize `source` losslessly.  The final token is always `Eof` and has an
/// empty range at the end of the input.
pub fn lex(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::with_capacity(source.len() / 3 + 8);
    let mut index = 0usize;

    while index < bytes.len() {
        let start = index;
        match bytes[index] {
            b' ' | b'\t' | b'\r' => {
                index += 1;
                while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\r') {
                    index += 1;
                }
                tokens.push(Token::new(TokenKind::Whitespace, range(start, index)));
            }
            b'\n' => {
                index += 1;
                tokens.push(Token::new(TokenKind::Newline, range(start, index)));
            }
            b'#' => {
                let mut end = index;
                while end < bytes.len() && bytes[end] != b'\n' {
                    end += 1;
                }
                let text = &source[index..end];
                let kind = if text.starts_with("## ") || text == "##" {
                    TokenKind::DocComment
                } else {
                    TokenKind::Comment
                };
                index = end;
                tokens.push(Token::new(kind, range(start, index)));
            }
            b'"' => {
                index = scan_string(bytes, index);
                tokens.push(Token::new(TokenKind::String, range(start, index)));
            }
            b'\'' => {
                index = scan_char(bytes, index);
                tokens.push(Token::new(TokenKind::Char, range(start, index)));
            }
            b'\\' => {
                if bytes.get(index + 1) == Some(&b'\\') {
                    index = scan_multiline_string(bytes, index);
                    tokens.push(Token::new(TokenKind::MultilineString, range(start, index)));
                } else {
                    index += 1;
                    tokens.push(Token::new(TokenKind::Backslash, range(start, index)));
                }
            }
            b'0'..=b'9' => {
                index = scan_number(bytes, index);
                tokens.push(Token::new(TokenKind::Number, range(start, index)));
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => {
                index = scan_identifier(bytes, index);
                let text = &source[start..index];
                if text == "_" {
                    tokens.push(Token::new(TokenKind::Underscore, range(start, index)));
                } else if let Some(keyword) = Keyword::from_str(text) {
                    // Keywords are lowercase-only in Roc; `True` and friends are
                    // tags, not keywords.
                    tokens.push(Token::keyword(keyword, range(start, index)));
                } else {
                    let first = text.trim_start_matches('$').chars().next();
                    let kind = if first.is_some_and(|ch| ch.is_ascii_uppercase()) {
                        TokenKind::UpperIdent
                    } else {
                        TokenKind::LowerIdent
                    };
                    tokens.push(Token::new(kind, range(start, index)));
                }
            }
            b'.' => {
                let (kind, len) = if bytes.get(index + 1) == Some(&b'.') {
                    match bytes.get(index + 2) {
                        Some(b'<') => (TokenKind::DotDotLess, 3),
                        Some(b'=') => (TokenKind::DotDotEq, 3),
                        _ => (TokenKind::DotDot, 2),
                    }
                } else if bytes.get(index + 1) == Some(&b'?') {
                    (TokenKind::OptionalQuestion, 2)
                } else {
                    (TokenKind::Dot, 1)
                };
                index += len;
                tokens.push(Token::new(kind, range(start, index)));
            }
            b':' => {
                let (kind, len) = match bytes.get(index + 1) {
                    Some(b':') => (TokenKind::ColonColon, 2),
                    Some(b'=') => (TokenKind::ColonEquals, 2),
                    _ => (TokenKind::Colon, 1),
                };
                index += len;
                tokens.push(Token::new(kind, range(start, index)));
            }
            b'=' => {
                let (kind, len) = match bytes.get(index + 1) {
                    Some(b'=') => (TokenKind::Operator, 2),
                    Some(b'>') => (TokenKind::FatArrow, 2),
                    _ => (TokenKind::Eq, 1),
                };
                index += len;
                tokens.push(Token::new(kind, range(start, index)));
            }
            b'-' => {
                let (kind, len) = if bytes.get(index + 1) == Some(&b'>') {
                    (TokenKind::Arrow, 2)
                } else {
                    (TokenKind::Operator, 1)
                };
                index += len;
                tokens.push(Token::new(kind, range(start, index)));
            }
            b'|' => {
                if bytes.get(index + 1) == Some(&b'>') {
                    index += 2;
                    tokens.push(Token::new(TokenKind::Operator, range(start, index)));
                } else {
                    index += 1;
                    tokens.push(Token::new(TokenKind::Pipe, range(start, index)));
                }
            }
            b'?' => {
                let (kind, len) = if bytes.get(index + 1) == Some(&b'?') {
                    (TokenKind::Operator, 2)
                } else {
                    (TokenKind::Question, 1)
                };
                index += len;
                tokens.push(Token::new(kind, range(start, index)));
            }
            b'!' => {
                let (kind, len) = if bytes.get(index + 1) == Some(&b'=') {
                    (TokenKind::Operator, 2)
                } else {
                    (TokenKind::Bang, 1)
                };
                index += len;
                tokens.push(Token::new(kind, range(start, index)));
            }
            b'<' => {
                let (kind, len) = if bytes.get(index + 1) == Some(&b'=') {
                    (TokenKind::Operator, 2)
                } else {
                    (TokenKind::Operator, 1)
                };
                index += len;
                tokens.push(Token::new(kind, range(start, index)));
            }
            b'>' => {
                let (kind, len) = if bytes.get(index + 1) == Some(&b'=') {
                    (TokenKind::Operator, 2)
                } else {
                    (TokenKind::Operator, 1)
                };
                index += len;
                tokens.push(Token::new(kind, range(start, index)));
            }
            b'+' | b'*' | b'%' | b'/' | b'&' | b'^' | b'~' => {
                // `//` is a single operator; everything else is one byte.
                let len = if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
                    2
                } else {
                    1
                };
                index += len;
                tokens.push(Token::new(TokenKind::Operator, range(start, index)));
            }
            b'(' => single(&mut tokens, &mut index, TokenKind::LParen),
            b')' => single(&mut tokens, &mut index, TokenKind::RParen),
            b'{' => single(&mut tokens, &mut index, TokenKind::LBrace),
            b'}' => single(&mut tokens, &mut index, TokenKind::RBrace),
            b'[' => single(&mut tokens, &mut index, TokenKind::LBracket),
            b']' => single(&mut tokens, &mut index, TokenKind::RBracket),
            b',' => single(&mut tokens, &mut index, TokenKind::Comma),
            _ => {
                // Consume one full UTF-8 code point so spans never split a
                // character.
                let ch = source[index..].chars().next().unwrap_or('\u{fffd}');
                index += ch.len_utf8();
                tokens.push(Token::new(TokenKind::Error, range(start, index)));
            }
        }
        debug_assert!(index > start, "lexer must make progress");
    }

    tokens.push(Token::new(
        TokenKind::Eof,
        TextRange::empty(source.len() as u32),
    ));
    tokens
}

fn range(start: usize, end: usize) -> TextRange {
    TextRange::new(start as u32, end as u32)
}

fn single(tokens: &mut Vec<Token>, index: &mut usize, kind: TokenKind) {
    let start = *index;
    *index += 1;
    tokens.push(Token::new(kind, range(start, *index)));
}

fn scan_identifier(bytes: &[u8], mut index: usize) -> usize {
    // The caller has already checked the first byte.
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' => index += 1,
            b'!' => {
                // A trailing bang is part of an effectful name.  Consume it and
                // stop; `!=` is lexed separately when followed by `=`.
                if bytes.get(index + 1) != Some(&b'=') {
                    index += 1;
                }
                break;
            }
            _ => break,
        }
    }
    index
}

fn scan_number(bytes: &[u8], mut index: usize) -> usize {
    if bytes[index] == b'0' {
        match bytes.get(index + 1) {
            Some(b'x') | Some(b'X') => {
                index += 2;
                while index < bytes.len()
                    && (bytes[index].is_ascii_hexdigit() || bytes[index] == b'_')
                {
                    index += 1;
                }
                return scan_number_suffix(bytes, index);
            }
            Some(b'o') | Some(b'O') => {
                index += 2;
                while index < bytes.len()
                    && (matches!(bytes[index], b'0'..=b'7') || bytes[index] == b'_')
                {
                    index += 1;
                }
                return scan_number_suffix(bytes, index);
            }
            Some(b'b') | Some(b'B') => {
                index += 2;
                while index < bytes.len()
                    && (matches!(bytes[index], b'0' | b'1') || bytes[index] == b'_')
                {
                    index += 1;
                }
                return scan_number_suffix(bytes, index);
            }
            _ => {}
        }
    }

    while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'_') {
        index += 1;
    }
    if bytes.get(index) == Some(&b'.')
        && bytes
            .get(index + 1)
            .is_some_and(|byte| byte.is_ascii_digit())
    {
        index += 1;
        while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'_') {
            index += 1;
        }
    }
    if matches!(bytes.get(index), Some(b'e') | Some(b'E')) {
        let mut cursor = index + 1;
        if matches!(bytes.get(cursor), Some(b'+') | Some(b'-')) {
            cursor += 1;
        }
        if bytes.get(cursor).is_some_and(|byte| byte.is_ascii_digit()) {
            index = cursor;
            while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'_') {
                index += 1;
            }
        }
    }
    scan_number_suffix(bytes, index)
}

fn scan_number_suffix(bytes: &[u8], index: usize) -> usize {
    // Numeric type suffixes: `.U8`, `.I64`, `.F32`, `.Dec`.  Only uppercase
    // suffixes are consumed; `1.to_str()` is a method call, not a suffix.
    if bytes.get(index) != Some(&b'.') {
        return index;
    }
    let Some(first) = bytes.get(index + 1) else {
        return index;
    };
    if !first.is_ascii_uppercase() {
        return index;
    }
    let mut cursor = index + 1;
    while cursor < bytes.len() && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_') {
        cursor += 1;
    }
    cursor
}

fn scan_string(bytes: &[u8], mut index: usize) -> usize {
    debug_assert_eq!(bytes[index], b'"');
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                if let Some(next) = bytes.get(index + 1) {
                    if *next == b'u' && bytes.get(index + 2) == Some(&b'(') {
                        index += 3;
                        while index < bytes.len() && bytes[index] != b')' {
                            index += 1;
                        }
                        if index < bytes.len() {
                            index += 1;
                        }
                    } else {
                        index += 1 + utf8_len(*next);
                    }
                } else {
                    index += 1;
                }
            }
            b'"' => {
                index += 1;
                return index;
            }
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                index = scan_interpolation(bytes, index + 2);
            }
            _ => index += 1,
        }
    }
    index
}

/// Scan a `${ ... }` interpolation expression, including nested strings and
/// braces. The returned index is just past the closing `}`.
fn scan_interpolation(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => index = scan_string(bytes, index),
            b'\'' => index = scan_char(bytes, index),
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth -= 1;
                index += 1;
                if depth == 0 {
                    return index;
                }
            }
            b'\\' => {
                index += 2;
            }
            _ => index += 1,
        }
    }
    index
}

fn scan_char(bytes: &[u8], mut index: usize) -> usize {
    debug_assert_eq!(bytes[index], b'\'');
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 1;
            if index < bytes.len() {
                index += utf8_len(bytes[index]);
            }
        } else if bytes[index] == b'\'' {
            index += 1;
            // Optional `.U8` suffix.
            if bytes.get(index) == Some(&b'.')
                && bytes
                    .get(index + 1)
                    .is_some_and(|byte| byte.is_ascii_uppercase())
            {
                index += 2;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
            }
            return index;
        } else if bytes[index] == b'\n' {
            return index;
        } else {
            index += 1;
        }
    }
    index
}

fn scan_multiline_string(bytes: &[u8], mut index: usize) -> usize {
    // Consume one or more consecutive `\\` continuation lines.  Leading
    // horizontal whitespace is allowed before the backslashes; the newline
    // belongs to the token.
    loop {
        if bytes.get(index) != Some(&b'\\') || bytes.get(index + 1) != Some(&b'\\') {
            return index;
        }
        index += 2;
        while index < bytes.len() && bytes[index] != b'\n' {
            if bytes[index] == b'\\' {
                index += 1;
                if index < bytes.len() {
                    index += utf8_len(bytes[index]);
                }
            } else {
                index += 1;
            }
        }
        if index >= bytes.len() {
            return index;
        }
        index += 1; // newline
        let mut cursor = index;
        while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t' | b'\r') {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'\\') && bytes.get(cursor + 1) == Some(&b'\\') {
            index = cursor;
            continue;
        }
        return index;
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Convenience for callers that only care about significant tokens.  The
/// returned ranges remain source ranges; no offsets are rewritten.
pub fn significant(tokens: &[Token]) -> impl Iterator<Item = &Token> {
    tokens.iter().filter(|token| !token.kind.is_trivia())
}

/// True when a token is a newline or EOF; useful for layout-sensitive parsing.
pub fn is_line_end(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Newline | TokenKind::Eof)
}

/// Find the token containing `offset`, preferring significant tokens.
pub fn token_at(tokens: &[Token], offset: u32) -> Option<&Token> {
    tokens
        .iter()
        .find(|token| !token.kind.is_trivia() && token.range.contains(offset))
        .or_else(|| tokens.iter().find(|token| token.range.contains(offset)))
}

/// A synthetic EOF range helper for tests and error recovery.
pub fn eof_range(source_len: usize) -> TextRange {
    TextRange::new(source_len as u32, source_len as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::token::Keyword;

    fn kinds(source: &str) -> Vec<TokenKind> {
        lex(source)
            .into_iter()
            .filter(|token| !token.kind.is_trivia() && token.kind != TokenKind::Eof)
            .map(|token| token.kind)
            .collect()
    }

    #[test]
    fn tokens_tile_the_source() {
        let source = "x = \"hi ${name}\" # c\n## doc\nfoo! = 1.U8\n";
        let tokens = lex(source);
        let mut rebuilt = String::new();
        for token in &tokens {
            rebuilt.push_str(token.text(source));
        }
        assert_eq!(rebuilt, source);
        assert_eq!(tokens.last().unwrap().kind, TokenKind::Eof);
    }

    #[test]
    fn identifiers_and_keywords() {
        let tokens = lex("$total line! Str match and _ _unused");
        let significant: Vec<_> = significant(&tokens).collect();
        assert_eq!(significant[0].kind, TokenKind::LowerIdent);
        assert_eq!(
            significant[0].text("$total line! Str match and _ _unused"),
            "$total"
        );
        assert_eq!(
            significant[1].text("$total line! Str match and _ _unused"),
            "line!"
        );
        assert_eq!(significant[2].kind, TokenKind::UpperIdent);
        assert_eq!(significant[3].keyword, Some(Keyword::Match));
        assert_eq!(significant[4].keyword, Some(Keyword::And));
        assert_eq!(significant[5].kind, TokenKind::Underscore);
        assert_eq!(significant[6].kind, TokenKind::LowerIdent);
    }

    #[test]
    fn numbers_include_suffixes_and_bases() {
        assert_eq!(
            kinds("42 1.5 0x5 0o7 0b101 5.U8 1e9"),
            vec![TokenKind::Number; 7]
        );
    }

    #[test]
    fn strings_and_interpolation_are_single_tokens() {
        let source = "x = \"a ${b} c \\n\"";
        let lexed = lex(source);
        let tokens: Vec<_> = significant(&lexed).collect();
        assert!(tokens.iter().any(|token| token.kind == TokenKind::String));
        let string = tokens
            .iter()
            .find(|token| token.kind == TokenKind::String)
            .unwrap();
        assert_eq!(string.text(source), "\"a ${b} c \\n\"");
    }

    #[test]
    fn multiline_strings_consume_continuations() {
        let source = "x = \\\\line one\n\t\\\\line ${two}\n\nnext = 1";
        let lexed = lex(source);
        let tokens: Vec<_> = significant(&lexed).collect();
        let multiline = tokens
            .iter()
            .find(|token| token.kind == TokenKind::MultilineString)
            .unwrap();
        assert!(multiline.text(source).contains("${two}"));
        assert!(tokens
            .iter()
            .any(|token| token.kind == TokenKind::LowerIdent && token.text(source) == "next"));
    }

    #[test]
    fn doc_comments_are_distinct() {
        let tokens = lex("## docs\n# ordinary\nx = 1");
        assert_eq!(tokens[0].kind, TokenKind::DocComment);
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Comment));
    }

    #[test]
    fn operators_are_not_merged_wrongly() {
        let source = "a != b and c <= d ..< e ?? f |> g";
        let lexed = lex(source);
        let tokens: Vec<_> = significant(&lexed).collect();
        assert!(tokens.iter().any(|token| token.text(source) == "!="));
        assert!(tokens.iter().any(|token| token.text(source) == "<="));
        assert!(tokens.iter().any(|token| token.text(source) == "..<"));
        assert!(tokens.iter().any(|token| token.text(source) == "??"));
        assert!(tokens.iter().any(|token| token.text(source) == "|>"));
    }
}

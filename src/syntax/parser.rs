//! Forgiving recursive-descent parser for Roc.
//!
//! The parser is designed for editors, not for code generation: it accepts
//! incomplete input, recovers at line/brace boundaries, and always returns a
//! partial tree.  Analysis only needs declarations, scopes, names, and enough
//! expression structure to resolve references; the exact tree shape of a
//! malformed expression is not important.

use crate::syntax::ast::*;
use crate::syntax::lexer;
use crate::syntax::token::{Keyword, Token, TokenKind};
use crate::text::TextRange;

/// Parse a Roc module.
pub fn parse(source: &str) -> ParsedFile {
    let tokens = lexer::lex(source);
    let (module, diagnostics) = {
        let mut parser = Parser::new(source, &tokens);
        let module = parser.parse_module();
        (module, parser.diagnostics)
    };
    ParsedFile {
        module,
        tokens,
        diagnostics,
    }
}

struct Parser<'a> {
    source: &'a str,
    tokens: &'a [Token],
    pos: usize,
    diagnostics: Vec<SyntaxDiagnostic>,
    line_starts: Vec<u32>,
    docs: Vec<DocLine>,
    doc_used: Vec<bool>,
}

#[derive(Debug, Clone)]
struct DocLine {
    line: usize,
    text: String,
    range: TextRange,
}

#[derive(Clone, Copy)]
struct ExprStop {
    kinds: &'static [TokenKind],
    keywords: &'static [Keyword],
    at_newline: bool,
}

impl ExprStop {
    const NONE: Self = Self {
        kinds: &[],
        keywords: &[],
        at_newline: false,
    };
    fn kinds(kinds: &'static [TokenKind]) -> Self {
        Self {
            kinds,
            keywords: &[],
            at_newline: true,
        }
    }
    fn keyword(keyword: Keyword) -> Self {
        Self {
            kinds: &[],
            keywords: keyword_slice(keyword),
            at_newline: true,
        }
    }
    fn contains(&self, token: &Token) -> bool {
        self.kinds.contains(&token.kind)
            || token
                .keyword
                .is_some_and(|keyword| self.keywords.contains(&keyword))
    }
}

fn keyword_slice(keyword: Keyword) -> &'static [Keyword] {
    // Returning a static slice per keyword avoids allocating in the hot parser.
    match keyword {
        Keyword::Else => &[Keyword::Else],
        Keyword::In => &[Keyword::In],
        Keyword::Exposing => &[Keyword::Exposing],
        Keyword::As => &[Keyword::As],
        _ => &[],
    }
}

fn qual_name_from_parts(parts: &[Name]) -> QualName {
    let range = parts
        .first()
        .zip(parts.last())
        .map(|(first, last)| TextRange::new(first.range.start, last.range.end))
        .unwrap_or_default();
    QualName {
        parts: parts.to_vec(),
        range,
    }
}

const EXPR_STOP_CALL: &[TokenKind] = &[TokenKind::Comma, TokenKind::RParen];
const EXPR_STOP_RECORD: &[TokenKind] = &[TokenKind::Comma, TokenKind::RBrace];
const EXPR_STOP_LIST: &[TokenKind] = &[TokenKind::Comma, TokenKind::RBracket];

impl<'a> Parser<'a> {
    fn new(source: &'a str, tokens: &'a [Token]) -> Self {
        let mut line_starts = vec![0u32];
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push((index + 1) as u32);
            }
        }
        let mut docs = Vec::new();
        for token in tokens {
            if token.kind == TokenKind::DocComment {
                let text = token.text(source);
                let line = line_of(&line_starts, token.range.start);
                let stripped = text
                    .strip_prefix("## ")
                    .or_else(|| text.strip_prefix("##"))
                    .unwrap_or(text);
                docs.push(DocLine {
                    line,
                    text: stripped.to_string(),
                    range: token.range,
                });
            }
        }
        let doc_used = vec![false; docs.len()];
        Self {
            source,
            tokens,
            pos: 0,
            diagnostics: Vec::new(),
            line_starts,
            docs,
            doc_used,
        }
    }

    // ------------------------------------------------------------------
    // Token access
    // ------------------------------------------------------------------

    fn raw(&self) -> Token {
        self.tokens.get(self.pos).copied().unwrap_or_else(|| {
            Token::new(TokenKind::Eof, TextRange::empty(self.source.len() as u32))
        })
    }

    fn skip_inline_trivia(&mut self) {
        while matches!(
            self.raw().kind,
            TokenKind::Whitespace | TokenKind::Comment | TokenKind::DocComment
        ) {
            self.pos += 1;
        }
    }

    fn skip_separators(&mut self, depth: usize) {
        loop {
            self.skip_inline_trivia();
            if depth > 0 && self.raw().kind == TokenKind::Newline {
                self.pos += 1;
                continue;
            }
            break;
        }
    }

    fn skip_newlines(&mut self) {
        loop {
            self.skip_inline_trivia();
            if self.raw().kind == TokenKind::Newline {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&mut self) -> Token {
        self.skip_inline_trivia();
        self.raw()
    }

    fn peek_at(&mut self, n: usize) -> Token {
        self.skip_inline_trivia();
        let mut index = self.pos;
        let mut seen = 0;
        while index < self.tokens.len() {
            let token = self.tokens[index];
            if !matches!(
                token.kind,
                TokenKind::Whitespace | TokenKind::Comment | TokenKind::DocComment
            ) {
                if seen == n {
                    return token;
                }
                seen += 1;
            }
            index += 1;
        }
        Token::new(TokenKind::Eof, TextRange::empty(self.source.len() as u32))
    }

    fn bump(&mut self) -> Token {
        self.skip_inline_trivia();
        let token = self.raw();
        if token.kind != TokenKind::Eof {
            self.pos += 1;
        }
        token
    }

    fn eat(&mut self, kind: TokenKind) -> Option<Token> {
        if self.peek().kind == kind {
            Some(self.bump())
        } else {
            None
        }
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> Option<Token> {
        if self.peek().keyword == Some(keyword) {
            Some(self.bump())
        } else {
            None
        }
    }

    fn expect(&mut self, kind: TokenKind, code: &'static str, message: &str) -> Option<Token> {
        self.skip_separators(0);
        if self.peek().kind == kind {
            Some(self.bump())
        } else {
            let start = self.peek().range.start;
            self.diagnostics.push(SyntaxDiagnostic {
                range: TextRange::empty(start),
                message: message.to_string(),
                code,
            });
            None
        }
    }

    fn eof(&mut self) -> bool {
        self.peek().kind == TokenKind::Eof
    }

    fn error_here(&mut self, code: &'static str, message: impl Into<String>) {
        let range = TextRange::empty(self.peek().range.start);
        self.diagnostics.push(SyntaxDiagnostic {
            range,
            message: message.into(),
            code,
        });
    }

    fn line_of(&self, offset: u32) -> usize {
        line_of(&self.line_starts, offset)
    }

    fn indent_of(&self, offset: u32) -> usize {
        let line = self.line_of(offset);
        let start = self.line_starts[line] as usize;
        let end = offset as usize;
        let mut indent = 0;
        for byte in self.source.as_bytes()[start..end].iter() {
            match byte {
                b' ' => indent += 1,
                b'\t' => indent += 4,
                _ => break,
            }
        }
        indent
    }

    fn current_indent(&mut self) -> usize {
        let token = self.peek();
        self.indent_of(token.range.start)
    }

    fn name(&self, token: Token) -> Name {
        Name::new(token.text(self.source).to_string(), token.range)
    }

    fn text(&self, token: Token) -> &'a str {
        token.text(self.source)
    }

    fn take_doc_before(&mut self, offset: u32) -> Option<Doc> {
        let target_line = self.line_of(offset);
        if target_line == 0 {
            return None;
        }
        let mut expected = target_line.saturating_sub(1);
        let mut indices = Vec::new();
        loop {
            let mut found = None;
            for (index, doc) in self.docs.iter().enumerate() {
                if !self.doc_used[index] && doc.line == expected {
                    found = Some(index);
                    break;
                }
            }
            let Some(index) = found else { break };
            indices.push(index);
            if expected == 0 {
                break;
            }
            expected -= 1;
        }
        if indices.is_empty() {
            return None;
        }
        indices.reverse();
        let mut text = String::new();
        let mut range = self.docs[indices[0]].range;
        for (position, index) in indices.iter().enumerate() {
            if position > 0 {
                text.push('\n');
            }
            text.push_str(&self.docs[*index].text);
            range = range.union(self.docs[*index].range);
            self.doc_used[*index] = true;
        }
        Some(Doc { text, range })
    }

    // ------------------------------------------------------------------
    // Module and headers
    // ------------------------------------------------------------------

    fn parse_module(&mut self) -> Module {
        let start = self.raw().range.start;
        let mut probe = self.pos;
        while probe < self.tokens.len() && self.tokens[probe].kind.is_trivia() {
            probe += 1;
        }
        let header = if probe < self.tokens.len()
            && matches!(
                self.tokens[probe].keyword,
                Some(Keyword::App)
                    | Some(Keyword::Module)
                    | Some(Keyword::Package)
                    | Some(Keyword::Platform)
            ) {
            self.pos = probe;
            Some(self.parse_header())
        } else {
            None
        };

        let mut imports = Vec::new();
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            if self.eof() {
                break;
            }
            let save = self.pos;
            let token = self.peek();
            if token.keyword == Some(Keyword::Import) {
                imports.push(self.parse_import());
            } else if token.keyword == Some(Keyword::Expect) {
                items.push(self.parse_expect_item());
            } else if let Some(item) = self.parse_top_item() {
                items.push(item);
            } else {
                self.error_here(
                    "unexpected-token",
                    format!("unexpected token `{}`", self.text(token)),
                );
                self.recover_top_level();
            }
            if self.pos == save {
                self.pos = (self.pos + 1).min(self.tokens.len());
            }
        }
        let end = self
            .tokens
            .iter()
            .rev()
            .find(|token| token.kind != TokenKind::Eof)
            .map(|token| token.range.end)
            .unwrap_or(start);
        Module {
            header,
            imports,
            items,
            range: TextRange::new(start, end),
        }
    }

    fn parse_header(&mut self) -> ModuleHeader {
        let start = self.peek().range.start;
        let keyword = self.bump();
        let kind = match keyword.keyword {
            Some(Keyword::App) => HeaderKind::App,
            Some(Keyword::Module) => HeaderKind::Module,
            Some(Keyword::Package) => HeaderKind::Package,
            _ => HeaderKind::Platform,
        };
        let mut name = None;
        let mut provides = Vec::new();
        let mut dependencies = Vec::new();
        let mut exposes = Vec::new();
        let mut requires = None;
        let mut provides_map = Vec::new();
        let mut hosted = Vec::new();
        let mut targets = None;

        if kind == HeaderKind::Platform {
            if self.peek().kind == TokenKind::String {
                let token = self.bump();
                name = Some(Name::new(
                    unquote(self.text(token)).to_string(),
                    token.range,
                ));
            }
            loop {
                self.skip_newlines();
                match self.peek().keyword {
                    Some(Keyword::Requires) => {
                        self.bump();
                        let indent = self.current_indent();
                        let mut ty = self.parse_type(
                            indent,
                            0,
                            &[],
                            &[
                                Keyword::Requires,
                                Keyword::Exposes,
                                Keyword::Packages,
                                Keyword::Provides,
                                Keyword::Hosted,
                                Keyword::Targets,
                            ],
                        );
                        // Platform headers declare `requires {} { ... }`: the
                        // first record is the host-requirement record and the
                        // second the type signature record.
                        while self.peek().kind == TokenKind::LBrace {
                            let extra = self.parse_type(
                                indent,
                                0,
                                &[],
                                &[
                                    Keyword::Requires,
                                    Keyword::Exposes,
                                    Keyword::Packages,
                                    Keyword::Provides,
                                    Keyword::Hosted,
                                    Keyword::Targets,
                                ],
                            );
                            ty.range = TextRange::new(ty.range.start, extra.range.end);
                        }
                        requires = Some(ty);
                    }
                    Some(Keyword::Exposes) => {
                        self.bump();
                        exposes = self.parse_name_list();
                    }
                    Some(Keyword::Packages) => {
                        self.bump();
                        dependencies.extend(self.parse_dependency_record());
                    }
                    Some(Keyword::Provides) => {
                        self.bump();
                        provides_map.extend(self.parse_provides());
                    }
                    Some(Keyword::Hosted) => {
                        self.bump();
                        hosted.extend(self.parse_hosted());
                    }
                    Some(Keyword::Targets) => {
                        self.bump();
                        self.eat(TokenKind::Colon);
                        let range_start = self.peek().range.start;
                        if self.peek().kind == TokenKind::LBrace {
                            let close = self.skip_balanced(TokenKind::LBrace, TokenKind::RBrace);
                            targets = Some(TextRange::new(range_start, close));
                        }
                    }
                    _ => break,
                }
            }
        } else {
            provides = self.parse_name_list();
            if kind != HeaderKind::Module {
                dependencies = self.parse_dependency_record();
            }
        }

        let end = self
            .tokens
            .iter()
            .take(self.pos)
            .rev()
            .find(|token| token.kind != TokenKind::Eof)
            .map(|token| token.range.end)
            .unwrap_or(start);
        ModuleHeader {
            kind,
            name,
            provides,
            dependencies,
            exposes,
            requires,
            provides_map,
            hosted,
            targets,
            range: TextRange::new(start, end),
        }
    }

    fn parse_name_list(&mut self) -> Vec<Name> {
        let mut names = Vec::new();
        self.skip_newlines();
        if !self.eat(TokenKind::LBracket).is_some() {
            return names;
        }
        loop {
            self.skip_newlines();
            match self.peek().kind {
                TokenKind::RBracket | TokenKind::Eof => {
                    self.eat(TokenKind::RBracket);
                    break;
                }
                TokenKind::UpperIdent | TokenKind::LowerIdent => {
                    let token = self.bump();
                    names.push(self.name(token));
                    self.skip_newlines();
                    self.eat(TokenKind::Comma);
                }
                _ => {
                    self.bump();
                }
            }
        }
        names
    }

    fn parse_dependency_record(&mut self) -> Vec<Dependency> {
        self.skip_newlines();
        let Some(open) = self.eat(TokenKind::LBrace) else {
            return Vec::new();
        };
        let mut dependencies = Vec::new();
        loop {
            self.skip_newlines();
            if self.eof() {
                self.diagnostics.push(SyntaxDiagnostic {
                    range: TextRange::empty({
                        let token = self.raw();
                        token.range.start
                    }),
                    message: "unclosed dependency record".to_string(),
                    code: "unclosed-record",
                });
                break;
            }
            if self.eat(TokenKind::RBrace).is_some() {
                break;
            }
            if self.peek().kind == TokenKind::Comma {
                self.bump();
                continue;
            }
            if !matches!(
                self.peek().kind,
                TokenKind::LowerIdent | TokenKind::UpperIdent
            ) {
                self.bump();
                continue;
            }
            let shorthand_token = self.bump();
            let shorthand = self.name(shorthand_token);
            self.eat(TokenKind::Colon);
            let is_platform = self.eat_keyword(Keyword::Platform).is_some();
            if self.peek().kind == TokenKind::LowerIdent && self.peek().text(self.source) == "glue"
            {
                let glue = self.bump();
                dependencies.push(Dependency {
                    shorthand,
                    is_platform,
                    source: DependencySource::CompilerOwned("glue".to_string()),
                    range: TextRange::new(start_of(&shorthand_token, open), glue.range.end),
                    source_range: glue.range,
                });
                self.eat(TokenKind::Comma);
                continue;
            }
            if self.peek().kind != TokenKind::String {
                self.recover_to(&[TokenKind::Comma, TokenKind::RBrace]);
                continue;
            }
            let source_token = self.bump();
            let raw = unquote(self.text(source_token)).to_string();
            let source = if shorthand.text == "roc" {
                DependencySource::RocVersion(raw)
            } else if raw.contains("://") {
                DependencySource::Url(raw)
            } else {
                DependencySource::Path(raw)
            };
            dependencies.push(Dependency {
                shorthand,
                is_platform,
                source,
                range: TextRange::new(start_of(&shorthand_token, open), source_token.range.end),
                source_range: source_token.range,
            });
            self.eat(TokenKind::Comma);
        }
        dependencies
    }

    fn parse_provides(&mut self) -> Vec<ProvidesEntry> {
        let mut entries = Vec::new();
        if self.eat(TokenKind::LBrace).is_none() {
            return entries;
        }
        loop {
            self.skip_newlines();
            if self.eof() || self.eat(TokenKind::RBrace).is_some() {
                break;
            }
            if self.peek().kind != TokenKind::String {
                self.bump();
                continue;
            }
            let symbol_token = self.bump();
            self.eat(TokenKind::Colon);
            if matches!(
                self.peek().kind,
                TokenKind::LowerIdent | TokenKind::UpperIdent
            ) {
                let target_token = self.bump();
                entries.push(ProvidesEntry {
                    symbol: Name::new(
                        unquote(self.text(symbol_token)).to_string(),
                        symbol_token.range,
                    ),
                    target: self.name(target_token),
                    range: TextRange::new(symbol_token.range.start, target_token.range.end),
                });
            }
            self.eat(TokenKind::Comma);
        }
        entries
    }

    fn parse_hosted(&mut self) -> Vec<HostedEntry> {
        let mut entries = Vec::new();
        if self.eat(TokenKind::LBrace).is_none() {
            return entries;
        }
        loop {
            self.skip_newlines();
            if self.eof() || self.eat(TokenKind::RBrace).is_some() {
                break;
            }
            if self.peek().kind != TokenKind::String {
                self.bump();
                continue;
            }
            let symbol_token = self.bump();
            self.eat(TokenKind::Colon);
            let target = self.parse_qual_name();
            if let Some(target) = target {
                entries.push(HostedEntry {
                    symbol: Name::new(
                        unquote(self.text(symbol_token)).to_string(),
                        symbol_token.range,
                    ),
                    range: TextRange::new(symbol_token.range.start, target.range.end),
                    target,
                });
            }
            self.eat(TokenKind::Comma);
        }
        entries
    }

    fn parse_import(&mut self) -> Import {
        let start = self.bump();
        let mut range = start.range;
        let mut alias = None;
        let mut exposing = Vec::new();
        let mut ty = None;

        let path = if self.peek().kind == TokenKind::String {
            let token = self.bump();
            range = range.union(token.range);
            ImportPath::File {
                path: unquote(self.text(token)).to_string(),
                range: token.range,
            }
        } else {
            let path_start = self.peek().range.start;
            let mut raw = String::new();
            let mut segments = Vec::new();
            let mut path_end = path_start;
            loop {
                let token = self.peek();
                if matches!(
                    token.kind,
                    TokenKind::Newline
                        | TokenKind::Eof
                        | TokenKind::Comment
                        | TokenKind::DocComment
                ) || token.keyword == Some(Keyword::As)
                    || token.keyword == Some(Keyword::Exposing)
                {
                    break;
                }
                if token.kind == TokenKind::LBracket {
                    break;
                }
                if matches!(token.kind, TokenKind::LowerIdent | TokenKind::UpperIdent) {
                    let token = self.bump();
                    raw.push_str(self.text(token));
                    segments.push(self.name(token));
                    path_end = token.range.end;
                } else if matches!(
                    token.kind,
                    TokenKind::Dot
                        | TokenKind::DotDot
                        | TokenKind::DotDotLess
                        | TokenKind::DotDotEq
                        | TokenKind::Operator
                        | TokenKind::Colon
                        | TokenKind::ColonColon
                ) {
                    let token = self.bump();
                    raw.push_str(self.text(token));
                    path_end = token.range.end;
                } else {
                    break;
                }
            }
            range = range.union(TextRange::new(path_start, path_end));
            ImportPath::Roc {
                raw,
                segments,
                range: TextRange::new(path_start, path_end),
            }
        };
        range = range.union(path.range());

        if self.eat_keyword(Keyword::As).is_some()
            && matches!(
                self.peek().kind,
                TokenKind::LowerIdent | TokenKind::UpperIdent
            )
        {
            let token = self.bump();
            alias = Some(self.name(token));
            range = range.union(token.range);
        }
        if self.eat(TokenKind::Colon).is_some() {
            let indent = self.current_indent();
            ty = Some(self.parse_type(indent, 0, &[TokenKind::Newline], &[]));
            if let Some(value) = &ty {
                range = range.union(value.range);
            }
        }
        if self.eat_keyword(Keyword::Exposing).is_some() {
            if let Some(open) = self.eat(TokenKind::LBracket) {
                range = range.union(open.range);
                loop {
                    self.skip_newlines();
                    if self.eof() {
                        break;
                    }
                    if let Some(close) = self.eat(TokenKind::RBracket) {
                        range = range.union(close.range);
                        break;
                    }
                    if matches!(
                        self.peek().kind,
                        TokenKind::LowerIdent | TokenKind::UpperIdent
                    ) {
                        let token = self.bump();
                        exposing.push(self.name(token));
                        range = range.union(token.range);
                    } else {
                        self.bump();
                    }
                }
            }
        }
        // Imports are line-oriented; consume a trailing comment/newline quietly.
        Import {
            path,
            alias,
            ty,
            exposing,
            range,
        }
    }

    // ------------------------------------------------------------------
    // Top-level items
    // ------------------------------------------------------------------

    fn parse_top_item(&mut self) -> Option<Item> {
        self.skip_newlines();
        let token = self.peek();
        let start = token.range.start;
        let doc = self.take_doc_before(start);
        if token.kind == TokenKind::UpperIdent {
            let after = self.peek_at(1);
            if matches!(after.kind, TokenKind::ColonColon | TokenKind::ColonEquals)
                || (after.kind == TokenKind::Colon
                    && !matches!(self.peek_at(2).kind, TokenKind::Colon | TokenKind::Eq))
            {
                return Some(self.parse_type_decl(doc));
            }
            if after.kind == TokenKind::LParen && self.type_params_end_in_type_marker() {
                return Some(self.parse_type_decl(doc));
            }
            return Some(self.parse_definition(doc, None));
        }
        if token.kind == TokenKind::LowerIdent && self.is_where_alias_start() {
            return Some(self.parse_where_alias(doc));
        }
        if matches!(
            token.kind,
            TokenKind::LowerIdent | TokenKind::Underscore | TokenKind::LParen | TokenKind::LBrace
        ) {
            let after = self.peek_at(1);
            if after.kind == TokenKind::Colon {
                return Some(self.parse_annotation_or_definition(doc));
            }
            return Some(self.parse_definition(doc, None));
        }
        if token.keyword == Some(Keyword::Var) {
            return Some(self.parse_definition(doc, None));
        }
        None
    }

    fn type_params_end_in_type_marker(&self) -> bool {
        let mut index = self.pos;
        let mut significant = 0;
        let mut open = None;
        while index < self.tokens.len() {
            if !self.tokens[index].kind.is_trivia() {
                significant += 1;
                if significant == 2 {
                    open = Some(index);
                    break;
                }
            }
            index += 1;
        }
        let Some(open) = open else {
            return false;
        };
        let mut depth = 0i32;
        for token in &self.tokens[open..] {
            match token.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        let mut next = self
                            .tokens
                            .iter()
                            .skip_while(|candidate| candidate.range.start <= token.range.start);
                        let marker = next.find(|candidate| !candidate.kind.is_trivia());
                        return marker.is_some_and(|candidate| {
                            matches!(
                                candidate.kind,
                                TokenKind::Colon | TokenKind::ColonColon | TokenKind::ColonEquals
                            )
                        });
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn is_where_alias_start(&mut self) -> bool {
        let mut index = self.pos;
        let next_significant = |mut index: usize| {
            while let Some(token) = self.tokens.get(index) {
                if !token.kind.is_trivia() {
                    return Some(index);
                }
                index += 1;
            }
            None
        };
        let Some(lower) = next_significant(index) else {
            return false;
        };
        if self.tokens[lower].kind != TokenKind::LowerIdent {
            return false;
        }
        let Some(dot) = next_significant(lower + 1) else {
            return false;
        };
        if self.tokens[dot].kind != TokenKind::Dot {
            return false;
        }
        let Some(upper) = next_significant(dot + 1) else {
            return false;
        };
        if self.tokens[upper].kind != TokenKind::UpperIdent {
            return false;
        }
        index = upper + 1;
        let Some(mut marker) = next_significant(index) else {
            return false;
        };
        if self.tokens[marker].kind == TokenKind::LParen {
            let mut depth = 0i32;
            loop {
                let Some(token) = self.tokens.get(marker) else {
                    return false;
                };
                match token.kind {
                    TokenKind::LParen => depth += 1,
                    TokenKind::RParen => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    TokenKind::Newline | TokenKind::Eof => return false,
                    _ => {}
                }
                marker += 1;
            }
            let Some(next) = next_significant(marker + 1) else {
                return false;
            };
            marker = next;
        }
        self.tokens
            .get(marker)
            .is_some_and(|token| token.kind == TokenKind::Colon)
    }

    fn parse_where_alias(&mut self, doc: Option<Doc>) -> Item {
        let start = self.peek().range.start;
        let ty_var_token = self.bump();
        let ty_var = self.name(ty_var_token);
        self.eat(TokenKind::Dot);
        let name_token = self.bump();
        let name = self.name(name_token);
        if self.peek().kind == TokenKind::LParen {
            self.skip_balanced(TokenKind::LParen, TokenKind::RParen);
        }
        self.eat(TokenKind::Colon);
        let indent = self.current_indent();
        let requirements = if self.eat_keyword(Keyword::Where).is_some() {
            self.parse_where_requirements(indent, 0)
        } else {
            Vec::new()
        };
        let end = self.previous_end().max(name.range.end);
        let range = TextRange::new(start, end);
        Item {
            range,
            kind: ItemKind::WhereAlias(WhereAlias {
                ty_var,
                name,
                requirements,
                doc,
                range,
            }),
        }
    }

    fn parse_annotation_or_definition(&mut self, doc: Option<Doc>) -> Item {
        let start = self.peek().range.start;
        let name_token = self.bump();
        let name = self.name(name_token);
        self.eat(TokenKind::Colon);
        if self.peek().kind == TokenKind::Newline {
            self.skip_newlines();
        }
        let indent = self.current_indent();
        let ty = self.parse_type(indent, 0, &[], &[]);
        let annotation_end = ty.range.end;
        let save = self.pos;
        self.skip_newlines();
        let lower_same = self.peek().kind == TokenKind::LowerIdent
            && self.peek().text(self.source) == name.text
            && self.peek_at(1).kind == TokenKind::Eq;
        let var_same = self.peek().keyword == Some(Keyword::Var)
            && self.peek_at(1).kind == TokenKind::LowerIdent
            && self.peek_at(1).text(self.source) == name.text;
        if lower_same || var_same {
            {
                let item = self.parse_definition(doc.clone(), Some(ty.clone()));
                if let ItemKind::Definition(mut definition) = item.kind {
                    definition.range = TextRange::new(start, definition.range.end);
                    return Item {
                        range: definition.range,
                        kind: ItemKind::Definition(definition),
                    };
                }
            }
        }
        self.pos = save;
        Item {
            range: TextRange::new(start, annotation_end),
            kind: ItemKind::Annotation(Annotation {
                name,
                ty,
                doc,
                range: TextRange::new(start, annotation_end),
            }),
        }
    }

    fn parse_type_decl(&mut self, doc: Option<Doc>) -> Item {
        let start = self.peek().range.start;
        let name_token = self.bump();
        let name = self.name(name_token);
        let mut params = Vec::new();
        let mut operator_range = self.peek().range;
        let mut kind = TypeDeclKind::Alias;

        if self.eat(TokenKind::LParen).is_some() {
            loop {
                self.skip_newlines();
                if self.eof() || self.eat(TokenKind::RParen).is_some() {
                    break;
                }
                if matches!(
                    self.peek().kind,
                    TokenKind::LowerIdent | TokenKind::UpperIdent
                ) {
                    let token = self.bump();
                    params.push(self.name(token));
                    self.eat(TokenKind::Comma);
                } else {
                    self.bump();
                }
            }
        }

        let colon = self.eat(TokenKind::Colon);
        let colon_colon = self.eat(TokenKind::ColonColon);
        let colon_equals = self.eat(TokenKind::ColonEquals);
        if let Some(token) = colon_colon.or(colon_equals) {
            operator_range = token.range;
            kind = if token.kind == TokenKind::ColonColon {
                TypeDeclKind::Opaque
            } else {
                TypeDeclKind::Nominal
            };
        } else if let Some(token) = colon {
            operator_range = token.range;
            kind = TypeDeclKind::Alias;
        }

        let indent = self.current_indent();
        let backing = self.parse_type(indent, 0, &[TokenKind::Newline], &[]);
        let mut members = Vec::new();
        let mut end = backing.range.end;
        // Nominal and opaque declarations put associated items in `.{ ... }`
        // after the backing type: `Color := [Red].{ to_hex : ... }`.
        if kind != TypeDeclKind::Alias {
            let save = self.pos;
            self.skip_newlines();
            let has_dot =
                self.peek().kind == TokenKind::Dot && self.peek_at(1).kind == TokenKind::LBrace;
            if has_dot {
                self.bump();
                let open = self.peek();
                let _ = open;
                self.bump();
                members = self.parse_member_items();
                if let Some(close) = self.eat(TokenKind::RBrace) {
                    end = close.range.end;
                }
            } else {
                self.pos = save;
            }
        } else {
            let save = self.pos;
            self.skip_newlines();
            if self.peek().kind == TokenKind::Dot && self.peek_at(1).kind == TokenKind::LBrace {
                self.bump();
                self.bump();
                members = self.parse_member_items();
                if let Some(close) = self.eat(TokenKind::RBrace) {
                    end = close.range.end;
                }
            } else {
                self.pos = save;
            }
        }
        if let Some(last) = members.last() {
            end = end.max(last.range.end);
        }
        Item {
            range: TextRange::new(start, end),
            kind: ItemKind::TypeDecl(TypeDecl {
                name,
                kind,
                params,
                backing,
                members,
                doc,
                range: TextRange::new(start, end),
                operator_range,
            }),
        }
    }

    fn parse_member_items(&mut self) -> Vec<Item> {
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            if self.eof() || self.peek().kind == TokenKind::RBrace {
                break;
            }
            let save = self.pos;
            let token = self.peek();
            let start = token.range.start;
            if token.keyword == Some(Keyword::Expect) {
                items.push(self.parse_expect_item());
            } else if token.kind == TokenKind::LowerIdent && self.is_where_alias_start() {
                let doc = self.take_doc_before(start);
                items.push(self.parse_where_alias(doc));
            } else if token.kind == TokenKind::LowerIdent
                && self.member_annotation_colon().is_some()
            {
                let doc = self.take_doc_before(start);
                items.push(self.parse_member_annotation(doc));
            } else if let Some(item) = self.parse_top_item() {
                items.push(item);
            } else {
                self.bump();
            }
            if self.pos == save {
                self.pos = (self.pos + 1).min(self.tokens.len());
            }
        }
        items
    }

    /// A member signature such as `parse : spec, state -> Try(state, err)`.
    /// Type bodies contain signatures without implementations, and a signature
    /// may span several lines; ordinary top-level annotation parsing stops at
    /// the first newline, which would misread those records as definitions.
    fn parse_member_annotation(&mut self, doc: Option<Doc>) -> Item {
        let start = self.peek().range.start;
        let mut name_end = start;
        while !self.eof() && self.peek().kind != TokenKind::Newline {
            if self
                .member_annotation_colon()
                .is_some_and(|colon| colon == self.pos)
            {
                break;
            }
            let token = self.bump();
            name_end = token.range.end;
        }
        self.eat(TokenKind::Colon);
        let name_text = self.source[start as usize..name_end as usize].trim();
        let name = Name::new(name_text.to_string(), TextRange::new(start, name_end));
        let indent = self.indent_of(start);
        if self.peek().kind == TokenKind::Newline {
            self.skip_newlines();
        }
        let ty = self.parse_type(indent, 0, &[], &[]);
        let else_save = self.pos;
        self.skip_newlines();
        if name_text
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '!')
            && self.peek().kind == TokenKind::LowerIdent
            && self.peek().text(self.source) == name_text
        {
            let after = self.peek_at(1);
            if after.kind == TokenKind::Eq || after.keyword == Some(Keyword::Var) {
                let item = self.parse_definition(doc.clone(), Some(ty.clone()));
                if let ItemKind::Definition(mut definition) = item.kind {
                    definition.range = TextRange::new(start, definition.range.end);
                    return Item {
                        range: definition.range,
                        kind: ItemKind::Definition(definition),
                    };
                }
            }
        }
        self.pos = else_save;
        Item {
            range: TextRange::new(start, ty.range.end),
            kind: ItemKind::Annotation(Annotation {
                name,
                ty,
                doc,
                range: TextRange::new(start, name_end.max(self.previous_end())),
            }),
        }
    }

    fn member_annotation_colon(&self) -> Option<usize> {
        let mut depth = 0i32;
        for (offset, token) in self.tokens[self.pos..].iter().enumerate() {
            match token.kind {
                TokenKind::Newline | TokenKind::Eof => return None,
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth -= 1;
                    if depth < 0 {
                        return None;
                    }
                }
                TokenKind::Colon if depth == 0 => return Some(self.pos + offset),
                TokenKind::ColonColon | TokenKind::ColonEquals | TokenKind::Eq if depth == 0 => {
                    return None;
                }
                TokenKind::Comma if depth == 0 => return None,
                _ => {}
            }
        }
        None
    }

    fn parse_expect_item(&mut self) -> Item {
        let start_token = self.bump();
        let doc = self.take_doc_before(start_token.range.start);
        let indent = self.indent_of(start_token.range.start);
        let expr = self.parse_expr_body(indent);
        let end = expr.range.end.max(start_token.range.end);
        Item {
            range: TextRange::new(start_token.range.start, end),
            kind: ItemKind::Expect(Expect {
                expr,
                doc,
                range: TextRange::new(start_token.range.start, end),
            }),
        }
    }

    fn parse_definition(&mut self, doc: Option<Doc>, annotation: Option<TypeExpr>) -> Item {
        let start_token = self.peek();
        let start = start_token.range.start;
        let is_var = self.eat_keyword(Keyword::Var).is_some();
        let indent = self.indent_of(start);
        let pattern = self.parse_pattern_stop(&[TokenKind::Eq], &[]);
        let equals = if self.peek().kind == TokenKind::Eq {
            self.expect(
                TokenKind::Eq,
                "expected-equals",
                "expected `=` after definition",
            )
        } else if is_var {
            // Current Roc allows declarations such as `var $count` without an
            // initializer.  It is a declaration, not a malformed definition.
            None
        } else {
            self.expect(
                TokenKind::Eq,
                "expected-equals",
                "expected `=` after definition",
            )
        };
        let value = if equals.is_some() {
            self.parse_expr_body(indent)
        } else {
            Expr::new(ExprKind::Error, TextRange::empty(pattern.range.end))
        };
        let end = value.range.end.max(pattern.range.end);
        let equals_range = equals
            .map(|token| token.range)
            .unwrap_or_else(|| TextRange::empty(end));
        Item {
            range: TextRange::new(start, end),
            kind: ItemKind::Definition(Definition {
                pattern,
                annotation,
                value,
                is_var,
                doc,
                range: TextRange::new(start, end),
                equals_range,
            }),
        }
    }

    fn recover_top_level(&mut self) {
        while !self.eof() {
            let token = self.peek();
            if token.kind == TokenKind::Eof {
                break;
            }
            if self.line_of(token.range.start) > 0
                && self.indent_of(token.range.start) == 0
                && token.kind != TokenKind::Newline
            {
                break;
            }
            self.bump();
        }
    }

    fn recover_to(&mut self, kinds: &[TokenKind]) {
        while !self.eof() {
            let token = self.peek();
            if kinds.contains(&token.kind) {
                break;
            }
            if token.kind == TokenKind::Newline && self.indent_of(token.range.start) == 0 {
                break;
            }
            self.bump();
        }
    }

    fn skip_balanced(&mut self, open: TokenKind, close: TokenKind) -> u32 {
        let start = self.peek().range.start;
        if self.eat(open).is_none() {
            return start;
        }
        let mut depth = 1usize;
        let mut end = start;
        while !self.eof() && depth > 0 {
            let token = self.bump();
            end = token.range.end;
            if token.kind == open {
                depth += 1;
            } else if token.kind == close {
                depth -= 1;
            }
        }
        if depth > 0 {
            self.diagnostics.push(SyntaxDiagnostic {
                range: TextRange::new(start, end),
                message: format!("unclosed `{}`", token_name(open)),
                code: "unclosed-delimiter",
            });
        }
        end
    }

    // ------------------------------------------------------------------
    // Expressions
    // ------------------------------------------------------------------

    fn parse_expr_body(&mut self, base_indent: usize) -> Expr {
        self.skip_inline_trivia();
        if self.raw().kind == TokenKind::Newline {
            self.skip_newlines();
            if self.eof() {
                return self.missing_expr();
            }
            let indent = self.current_indent();
            if indent <= base_indent {
                return self.missing_expr();
            }
            return self.parse_expr(indent, 0, ExprStop::NONE);
        }
        let indent = self.current_indent();
        self.parse_expr(indent.max(base_indent), 0, ExprStop::NONE)
    }

    fn parse_expr(&mut self, base_indent: usize, depth: usize, stop: ExprStop) -> Expr {
        self.skip_separators(depth);
        let start = self.peek().range.start;
        let mut lhs = self.parse_unary(base_indent, depth, stop);
        loop {
            self.skip_separators(depth);
            if self.at_expr_boundary(base_indent, depth, stop) {
                break;
            }
            let token = self.peek();
            let operator_text = self.text(token);
            let Some(_precedence) = binary_operator(token, operator_text) else {
                break;
            };
            let op_text = token
                .keyword
                .map(|keyword| keyword.as_str().to_string())
                .unwrap_or_else(|| operator_text.to_string());
            self.bump();
            let rhs = self.parse_unary(base_indent, depth, stop);
            lhs = Expr::new(
                ExprKind::Binary {
                    op: op_text,
                    op_range: token.range,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                TextRange::new(start, token.range.end),
            );
        }
        lhs
    }

    fn parse_unary(&mut self, base_indent: usize, depth: usize, stop: ExprStop) -> Expr {
        self.skip_separators(depth);
        let token = self.peek();
        if token.kind == TokenKind::Bang
            || (token.kind == TokenKind::Operator && matches!(self.text(token), "-" | "!"))
        {
            let op = self.bump();
            let expr = self.parse_unary(base_indent, depth, stop);
            return Expr::new(
                ExprKind::Unary {
                    op: self.text(op).to_string(),
                    op_range: op.range,
                    expr: Box::new(expr),
                },
                TextRange::new(op.range.start, op.range.end),
            );
        }
        let mut expr = self.parse_primary(base_indent, depth, stop);
        let start = expr.range.start;
        loop {
            self.skip_separators(depth);
            if self.at_expr_boundary(base_indent, depth, stop) {
                break;
            }
            let token = self.peek();
            match token.kind {
                TokenKind::Question if token.range.start == expr.range.end => {
                    let question = self.bump();
                    expr = Expr::new(
                        ExprKind::Unary {
                            op: "?".to_string(),
                            op_range: question.range,
                            expr: Box::new(expr),
                        },
                        TextRange::new(start, question.range.end),
                    );
                }
                TokenKind::Arrow => {
                    // Method chaining: `value ->method(args)`.
                    self.bump();
                    self.skip_inline_trivia();
                    if !matches!(
                        self.peek().kind,
                        TokenKind::LowerIdent | TokenKind::UpperIdent
                    ) {
                        break;
                    }
                    let method = self.bump();
                    let args = if self.peek().kind == TokenKind::LParen {
                        self.parse_call_args(depth)
                    } else {
                        Vec::new()
                    };
                    expr = Expr::new(
                        ExprKind::MethodCall {
                            receiver: Box::new(expr),
                            method: self.name(method),
                            args,
                        },
                        TextRange::new(start, self.previous_end()),
                    );
                }
                TokenKind::Dot => {
                    self.bump();
                    self.skip_inline_trivia();
                    if self.peek().kind == TokenKind::UpperIdent
                        && matches!(expr.kind, ExprKind::Record { .. } | ExprKind::Tag { .. })
                    {
                        // Record-builder suffix: `{ count: 1 }.Builder`.
                        let field = self.bump();
                        let type_name = QualName {
                            parts: vec![self.name(field)],
                            range: field.range,
                        };
                        expr = Expr::new(
                            ExprKind::Nominal {
                                type_name,
                                value: Box::new(expr),
                            },
                            TextRange::new(start, field.range.end),
                        );
                        continue;
                    }
                    if matches!(self.peek().kind, TokenKind::LBrace | TokenKind::LParen) {
                        let type_name = match &expr.kind {
                            ExprKind::Lookup { path } => Some(QualName {
                                parts: path.clone(),
                                range: expr.range,
                            }),
                            ExprKind::Tag { name, .. } => Some(QualName {
                                parts: vec![name.clone()],
                                range: expr.range,
                            }),
                            _ => None,
                        };
                        let value = if self.peek().kind == TokenKind::LBrace {
                            self.parse_brace_expr(base_indent, depth)
                        } else {
                            self.parse_tuple_or_paren(base_indent, depth)
                        };
                        if let Some(type_name) = type_name {
                            expr = Expr::new(
                                ExprKind::Nominal {
                                    type_name,
                                    value: Box::new(value),
                                },
                                TextRange::new(start, self.previous_end()),
                            );
                        }
                        continue;
                    }
                    if self.peek().kind == TokenKind::UpperIdent
                        || self.peek().kind == TokenKind::LowerIdent
                    {
                        let field = self.bump();
                        if self.peek().kind == TokenKind::LParen {
                            let args = self.parse_call_args(depth);
                            let expr_start = expr.range.start;
                            expr = Expr::new(
                                ExprKind::MethodCall {
                                    receiver: Box::new(expr),
                                    method: self.name(field),
                                    args,
                                },
                                TextRange::new(expr_start, self.previous_end()),
                            );
                        } else {
                            let expr_start = expr.range.start;
                            expr = match expr.kind.clone() {
                                ExprKind::Lookup { mut path } => {
                                    path.push(self.name(field));
                                    Expr::new(
                                        ExprKind::Lookup { path },
                                        TextRange::new(expr_start, field.range.end),
                                    )
                                }
                                _ => Expr::new(
                                    ExprKind::Field {
                                        base: Box::new(expr),
                                        field: self.name(field),
                                        optional: false,
                                    },
                                    TextRange::new(expr_start, field.range.end),
                                ),
                            };
                        }
                    } else if self.peek().kind == TokenKind::Number {
                        let index = self.bump();
                        if let Ok(value) = self.text(index).parse::<u32>() {
                            expr = Expr::new(
                                ExprKind::TupleIndex {
                                    base: Box::new(expr),
                                    index: value,
                                    index_range: index.range,
                                },
                                TextRange::new(start, index.range.end),
                            );
                        }
                    }
                }
                TokenKind::OptionalQuestion => {
                    let question = self.bump();
                    self.skip_inline_trivia();
                    if matches!(
                        self.peek().kind,
                        TokenKind::LowerIdent | TokenKind::UpperIdent
                    ) {
                        let field = self.bump();
                        expr = Expr::new(
                            ExprKind::Field {
                                base: Box::new(expr),
                                field: self.name(field),
                                optional: true,
                            },
                            TextRange::new(start, field.range.end.max(question.range.end)),
                        );
                    }
                }
                TokenKind::LParen => {
                    let args = self.parse_call_args(depth);
                    expr = Expr::new(
                        ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                        },
                        TextRange::new(start, self.previous_end()),
                    );
                }
                TokenKind::DotDot | TokenKind::DotDotLess | TokenKind::DotDotEq => break,
                _ => break,
            }
        }
        expr
    }

    fn parse_primary(&mut self, base_indent: usize, depth: usize, stop: ExprStop) -> Expr {
        let token = self.peek();
        let start = token.range.start;
        match token.kind {
            TokenKind::Number => {
                let token = self.bump();
                Expr::new(ExprKind::Number(self.text(token).to_string()), token.range)
            }
            TokenKind::String => {
                let token = self.bump();
                Expr::new(ExprKind::String(self.string_parts(token)), token.range)
            }
            TokenKind::MultilineString => {
                let token = self.bump();
                Expr::new(
                    ExprKind::MultilineString(self.string_parts(token)),
                    token.range,
                )
            }
            TokenKind::Char => {
                let token = self.bump();
                Expr::new(ExprKind::Char(self.text(token).to_string()), token.range)
            }
            TokenKind::LBrace => self.parse_brace_expr(base_indent, depth),
            TokenKind::LBracket => self.parse_list(depth),
            TokenKind::LParen => self.parse_tuple_or_paren(base_indent, depth),
            TokenKind::Pipe => self.parse_lambda(base_indent, depth),
            TokenKind::LowerIdent => {
                let token = self.bump();
                self.parse_lookup_or_tag(token, start)
            }
            TokenKind::UpperIdent => {
                let token = self.bump();
                if self.peek().kind == TokenKind::LParen {
                    let args = self.parse_call_args(depth);
                    Expr::new(
                        ExprKind::Tag {
                            name: self.name(token),
                            args,
                        },
                        TextRange::new(start, self.previous_end()),
                    )
                } else {
                    self.parse_lookup_or_tag(token, start)
                }
            }
            TokenKind::Underscore => {
                let token = self.bump();
                Expr::new(
                    ExprKind::Lookup {
                        path: vec![self.name(token)],
                    },
                    token.range,
                )
            }
            TokenKind::Keyword if token.keyword == Some(Keyword::If) => {
                self.parse_if(base_indent, depth)
            }
            TokenKind::Keyword if token.keyword == Some(Keyword::Match) => {
                self.parse_match(base_indent, depth)
            }
            TokenKind::Keyword if token.keyword == Some(Keyword::For) => {
                self.parse_for(base_indent, depth)
            }
            TokenKind::Keyword if token.keyword == Some(Keyword::While) => {
                self.parse_while(base_indent, depth)
            }
            TokenKind::Keyword if token.keyword == Some(Keyword::Return) => {
                let keyword = self.bump();
                let value = if self.peek().kind == TokenKind::Newline
                    || self.peek().kind == TokenKind::RBrace
                {
                    None
                } else {
                    Some(Box::new(self.parse_expr(base_indent, depth, stop)))
                };
                Expr::new(
                    ExprKind::Return(value),
                    TextRange::new(keyword.range.start, self.previous_end()),
                )
            }
            TokenKind::Keyword if token.keyword == Some(Keyword::Break) => {
                let token = self.bump();
                Expr::new(ExprKind::Break, token.range)
            }
            TokenKind::Keyword if token.keyword == Some(Keyword::Crash) => {
                let keyword = self.bump();
                let value = Box::new(self.parse_unary(base_indent, depth, stop));
                Expr::new(
                    ExprKind::Crash(value),
                    TextRange::new(keyword.range.start, self.previous_end()),
                )
            }
            TokenKind::Keyword if token.keyword == Some(Keyword::Dbg) => {
                let keyword = self.bump();
                let value = Box::new(self.parse_unary(base_indent, depth, stop));
                Expr::new(
                    ExprKind::Db(value),
                    TextRange::new(keyword.range.start, self.previous_end()),
                )
            }
            TokenKind::Error => {
                let token = self.bump();
                self.diagnostics.push(SyntaxDiagnostic {
                    range: token.range,
                    message: format!("unexpected character `{}`", self.text(token)),
                    code: "unexpected-character",
                });
                Expr::new(ExprKind::Error, token.range)
            }
            TokenKind::Eof => self.error_expr(),
            _ => {
                let token = self.bump();
                Expr::new(ExprKind::Error, token.range)
            }
        }
    }

    fn parse_lookup_or_tag(&mut self, token: Token, start: u32) -> Expr {
        let mut path = vec![self.name(token)];
        let mut end = token.range.end;
        loop {
            if self.peek().kind != TokenKind::Dot {
                break;
            }
            let after = self.peek_at(1);
            if !matches!(after.kind, TokenKind::LowerIdent | TokenKind::UpperIdent) {
                break;
            }
            self.bump();
            let segment = self.bump();
            end = segment.range.end;
            path.push(self.name(segment));
        }
        if path.len() == 1 && token.kind == TokenKind::UpperIdent {
            Expr::new(
                ExprKind::Tag {
                    name: path.remove(0),
                    args: Vec::new(),
                },
                TextRange::new(start, end),
            )
        } else {
            Expr::new(ExprKind::Lookup { path }, TextRange::new(start, end))
        }
    }

    fn parse_call_args(&mut self, depth: usize) -> Vec<Expr> {
        let mut args = Vec::new();
        if self.eat(TokenKind::LParen).is_none() {
            return args;
        }
        loop {
            self.skip_separators(depth + 1);
            if self.eof() {
                self.diagnostics.push(SyntaxDiagnostic {
                    range: TextRange::empty({
                        let token = self.raw();
                        token.range.start
                    }),
                    message: "unclosed function call".to_string(),
                    code: "unclosed-call",
                });
                break;
            }
            if self.eat(TokenKind::RParen).is_some() {
                break;
            }
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            let argument_indent = self.current_indent();
            let expr = self.parse_expr(argument_indent, depth + 1, ExprStop::kinds(EXPR_STOP_CALL));
            let empty = expr.range.is_empty();
            args.push(expr);
            self.skip_separators(depth + 1);
            self.eat(TokenKind::Comma);
            if empty {
                self.bump();
            }
        }
        args
    }

    fn parse_list(&mut self, depth: usize) -> Expr {
        let open = self.bump();
        let mut elements = Vec::new();
        loop {
            self.skip_separators(depth + 1);
            if self.eof() {
                self.diagnostics.push(SyntaxDiagnostic {
                    range: TextRange::new(open.range.start, open.range.end),
                    message: "unclosed list".to_string(),
                    code: "unclosed-list",
                });
                break;
            }
            if self.eat(TokenKind::RBracket).is_some() {
                break;
            }
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            let element_indent = self.current_indent();
            let expr = self.parse_expr(element_indent, depth + 1, ExprStop::kinds(EXPR_STOP_LIST));
            elements.push(expr);
            self.skip_separators(depth + 1);
            self.eat(TokenKind::Comma);
        }
        Expr::new(
            ExprKind::List(elements),
            TextRange::new(open.range.start, self.previous_end()),
        )
    }

    fn parse_tuple_or_paren(&mut self, base_indent: usize, depth: usize) -> Expr {
        let open = self.bump();
        self.skip_separators(depth + 1);
        if self.eat(TokenKind::RParen).is_some() {
            return Expr::new(
                ExprKind::Tuple(Vec::new()),
                TextRange::new(open.range.start, self.previous_end()),
            );
        }
        let first_indent = base_indent.max(self.current_indent());
        let first = self.parse_expr(
            first_indent,
            depth + 1,
            ExprStop::kinds(&[TokenKind::Comma, TokenKind::RParen]),
        );
        self.skip_separators(depth + 1);
        if self.eat(TokenKind::Comma).is_some() {
            let mut elements = vec![first];
            loop {
                self.skip_separators(depth + 1);
                if self.eof() || self.eat(TokenKind::RParen).is_some() {
                    break;
                }
                let expr = self.parse_expr(
                    base_indent,
                    depth + 1,
                    ExprStop::kinds(&[TokenKind::Comma, TokenKind::RParen]),
                );
                elements.push(expr);
                self.skip_separators(depth + 1);
                self.eat(TokenKind::Comma);
            }
            Expr::new(
                ExprKind::Tuple(elements),
                TextRange::new(open.range.start, self.previous_end()),
            )
        } else {
            self.eat(TokenKind::RParen);
            Expr::new(
                ExprKind::Paren(Box::new(first)),
                TextRange::new(open.range.start, self.previous_end()),
            )
        }
    }

    fn parse_lambda(&mut self, base_indent: usize, depth: usize) -> Expr {
        let open = self.bump();
        let mut params = Vec::new();
        if self.peek().kind == TokenKind::Pipe {
            self.bump();
        } else {
            loop {
                self.skip_separators(depth);
                if self.eof() || self.eat(TokenKind::Pipe).is_some() {
                    break;
                }
                let pattern = self.parse_pattern_stop(&[TokenKind::Comma, TokenKind::Pipe], &[]);
                params.push(pattern);
                self.skip_separators(depth);
                self.eat(TokenKind::Comma);
            }
        }
        let body = self.parse_expr_body(base_indent);
        Expr::new(
            ExprKind::Lambda {
                params,
                body: Box::new(body),
            },
            TextRange::new(open.range.start, self.previous_end()),
        )
    }

    fn parse_brace_expr(&mut self, base_indent: usize, depth: usize) -> Expr {
        if self.looks_like_record() {
            self.parse_record(base_indent, depth)
        } else {
            self.parse_block(base_indent, depth)
        }
    }

    fn looks_like_record(&mut self) -> bool {
        let mut brace_depth = 0i32;
        let mut paren_depth = 0i32;
        let mut bracket_depth = 0i32;
        let mut first_top: Option<Token> = None;
        let mut second_top: Option<Token> = None;
        let mut index = self.pos;
        while index < self.tokens.len() {
            let token = self.tokens[index];
            if brace_depth == 1 && paren_depth == 0 && bracket_depth == 0 {
                if token.kind == TokenKind::Eq {
                    return false;
                }
                if !token.kind.is_trivia()
                    && !matches!(token.kind, TokenKind::RBrace | TokenKind::Eof)
                {
                    if first_top.is_none() {
                        first_top = Some(token);
                    } else if second_top.is_none() {
                        second_top = Some(token);
                    }
                }
            }
            match token.kind {
                TokenKind::LBrace => brace_depth += 1,
                TokenKind::RBrace => {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        break;
                    }
                }
                TokenKind::LParen => paren_depth += 1,
                TokenKind::RParen => paren_depth -= 1,
                TokenKind::LBracket => bracket_depth += 1,
                TokenKind::RBracket => bracket_depth -= 1,
                TokenKind::Eof => break,
                _ => {}
            }
            index += 1;
        }
        let Some(first) = first_top else {
            return true;
        };
        match first.kind {
            TokenKind::LowerIdent => second_top.map_or(true, |token| {
                matches!(
                    token.kind,
                    TokenKind::Colon | TokenKind::Comma | TokenKind::RBrace
                )
            }),
            TokenKind::DotDot => true,
            _ => false,
        }
    }

    fn parse_record(&mut self, base_indent: usize, depth: usize) -> Expr {
        let open = self.bump();
        let mut fields = Vec::new();
        let mut update = None;
        loop {
            self.skip_separators(depth + 1);
            if self.eof() {
                self.diagnostics.push(SyntaxDiagnostic {
                    range: TextRange::new(open.range.start, open.range.end),
                    message: "unclosed record".to_string(),
                    code: "unclosed-record",
                });
                break;
            }
            if self.eat(TokenKind::RBrace).is_some() {
                break;
            }
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            if self.eat(TokenKind::DotDot).is_some() {
                update = Some(Box::new(self.parse_expr(
                    base_indent,
                    depth + 1,
                    ExprStop::kinds(EXPR_STOP_RECORD),
                )));
                self.skip_separators(depth + 1);
                self.eat(TokenKind::Comma);
                continue;
            }
            if matches!(
                self.peek().kind,
                TokenKind::LowerIdent | TokenKind::UpperIdent
            ) {
                let name_token = self.bump();
                let name = self.name(name_token);
                let mut value = None;
                if self.eat(TokenKind::Colon).is_some() {
                    value = Some(self.parse_expr(
                        base_indent,
                        depth + 1,
                        ExprStop::kinds(EXPR_STOP_RECORD),
                    ));
                }
                let range = TextRange::new(name_token.range.start, self.previous_end());
                fields.push(RecordField {
                    name,
                    value,
                    optional: false,
                    default: None,
                    range,
                });
                self.skip_separators(depth + 1);
                self.eat(TokenKind::Comma);
            } else {
                self.bump();
            }
        }
        Expr::new(
            ExprKind::Record { fields, update },
            TextRange::new(open.range.start, self.previous_end()),
        )
    }

    fn parse_block(&mut self, _base_indent: usize, depth: usize) -> Expr {
        let open = self.bump();
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines();
            if self.eof() {
                self.diagnostics.push(SyntaxDiagnostic {
                    range: TextRange::new(open.range.start, open.range.end),
                    message: "unclosed block".to_string(),
                    code: "unclosed-block",
                });
                break;
            }
            if self.eat(TokenKind::RBrace).is_some() {
                break;
            }
            let token = self.peek();
            let stmt_start = token.range.start;
            let stmt_indent = self.indent_of(stmt_start);
            if token.kind == TokenKind::Keyword {
                match token.keyword {
                    Some(Keyword::Dbg) => {
                        self.bump();
                        let value = self.parse_expr(stmt_indent, depth, ExprStop::NONE);
                        stmts.push(Stmt {
                            range: TextRange::new(stmt_start, value.range.end),
                            kind: StmtKind::Db(value),
                        });
                        continue;
                    }
                    Some(Keyword::Expect) => {
                        self.bump();
                        let value = self.parse_expr(stmt_indent, depth, ExprStop::NONE);
                        stmts.push(Stmt {
                            range: TextRange::new(stmt_start, value.range.end),
                            kind: StmtKind::Expect(value),
                        });
                        continue;
                    }
                    Some(Keyword::Return) => {
                        self.bump();
                        let value = if self.peek().kind == TokenKind::Newline
                            || self.peek().kind == TokenKind::RBrace
                        {
                            None
                        } else {
                            Some(self.parse_expr(stmt_indent, depth, ExprStop::NONE))
                        };
                        stmts.push(Stmt {
                            range: TextRange::new(stmt_start, self.previous_end()),
                            kind: StmtKind::Return(value),
                        });
                        continue;
                    }
                    Some(Keyword::Break) => {
                        self.bump();
                        stmts.push(Stmt {
                            range: TextRange::new(stmt_start, self.previous_end()),
                            kind: StmtKind::Break,
                        });
                        continue;
                    }
                    Some(Keyword::Crash) => {
                        self.bump();
                        let value = self.parse_expr(stmt_indent, depth, ExprStop::NONE);
                        stmts.push(Stmt {
                            range: TextRange::new(stmt_start, self.previous_end()),
                            kind: StmtKind::Crash(value),
                        });
                        continue;
                    }
                    _ => {}
                }
            }

            if self.peek().keyword == Some(Keyword::Var)
                && self.peek_at(1).kind == TokenKind::LowerIdent
            {
                let definition = self.parse_definition(None, None);
                if let ItemKind::Definition(definition) = definition.kind {
                    stmts.push(Stmt {
                        range: definition.range,
                        kind: StmtKind::Definition(definition),
                    });
                    continue;
                }
            }
            if let Some((pattern, value, range)) = self.try_parse_block_var_assignment(stmt_indent)
            {
                stmts.push(Stmt {
                    range,
                    kind: StmtKind::Assignment { pattern, value },
                });
                continue;
            }
            if let Some(definition) = self.try_parse_block_definition(stmt_indent) {
                stmts.push(Stmt {
                    range: definition.range,
                    kind: StmtKind::Definition(definition),
                });
                continue;
            }
            if self.peek().kind == TokenKind::UpperIdent
                && matches!(
                    self.peek_at(1).kind,
                    TokenKind::Colon | TokenKind::ColonColon | TokenKind::ColonEquals
                )
            {
                let item = self.parse_type_decl(None);
                if let ItemKind::TypeDecl(decl) = item.kind {
                    stmts.push(Stmt {
                        range: item.range,
                        kind: StmtKind::TypeDecl(decl),
                    });
                    continue;
                }
            }
            if let Some(definition) = self.try_parse_block_annotation_definition(stmt_indent, depth)
            {
                stmts.push(Stmt {
                    range: definition.range,
                    kind: StmtKind::Definition(definition),
                });
                continue;
            }
            let expr = self.parse_expr(stmt_indent, depth, ExprStop::NONE);
            if expr.range.is_empty() {
                if self.eof() || self.peek().kind == TokenKind::RBrace {
                    break;
                }
                self.bump();
                continue;
            }
            stmts.push(Stmt {
                range: expr.range,
                kind: StmtKind::Expr(expr),
            });
        }
        Expr::new(
            ExprKind::Block { stmts, tail: None },
            TextRange::new(open.range.start, self.previous_end()),
        )
    }

    fn try_parse_block_annotation_definition(
        &mut self,
        _base_indent: usize,
        _depth: usize,
    ) -> Option<Definition> {
        let token = self.peek();
        if token.kind != TokenKind::LowerIdent {
            return None;
        }
        if self.peek_at(1).kind != TokenKind::Colon {
            return None;
        }
        if matches!(
            self.peek_at(2).kind,
            TokenKind::Colon | TokenKind::ColonColon | TokenKind::ColonEquals
        ) {
            return None;
        }
        let name_token = self.bump();
        let name = self.name(name_token);
        self.bump(); // colon
        let indent = self.current_indent();
        let ty = self.parse_type(indent, 0, &[TokenKind::Newline, TokenKind::Eq], &[]);
        self.skip_newlines();
        let followed_by_definition = self.peek().kind == TokenKind::LowerIdent
            && self.peek().text(self.source) == name.text
            && self.peek_at(1).kind == TokenKind::Eq;
        let followed_by_var = self.peek().keyword == Some(Keyword::Var)
            && self.peek_at(1).kind == TokenKind::LowerIdent
            && self.peek_at(1).text(self.source) == name.text;
        if followed_by_definition || followed_by_var {
            let item = self.parse_definition(None, Some(ty));
            if let ItemKind::Definition(definition) = item.kind {
                return Some(definition);
            }
        }
        // The annotation itself is consumed even without a following
        // definition; the next statement is parsed normally.
        None
    }

    fn try_parse_block_var_assignment(
        &mut self,
        base_indent: usize,
    ) -> Option<(Pattern, Expr, TextRange)> {
        let start = self.pos;
        let token = self.peek();
        if token.kind != TokenKind::LowerIdent || !token.text(self.source).starts_with('$') {
            return None;
        }
        let mut index = self.pos;
        let mut found_eq = false;
        while index < self.tokens.len() {
            match self.tokens[index].kind {
                TokenKind::Newline | TokenKind::Eof => break,
                TokenKind::Eq => {
                    found_eq = true;
                    break;
                }
                TokenKind::Comma | TokenKind::ColonColon | TokenKind::ColonEquals => break,
                _ => {}
            }
            index += 1;
        }
        if !found_eq {
            self.pos = start;
            return None;
        }
        let pattern = self.parse_pattern_stop(&[TokenKind::Eq], &[]);
        self.expect(
            TokenKind::Eq,
            "expected-equals",
            "expected `=` after reassignment target",
        );
        let value = self.parse_expr_body(base_indent);
        let range = TextRange::new(pattern.range.start, value.range.end);
        Some((pattern, value, range))
    }

    fn try_parse_block_definition(&mut self, base_indent: usize) -> Option<Definition> {
        let start = self.pos;
        let token = self.peek();
        if !matches!(
            token.kind,
            TokenKind::LowerIdent
                | TokenKind::UpperIdent
                | TokenKind::Underscore
                | TokenKind::LParen
                | TokenKind::LBrace
        ) && token.keyword != Some(Keyword::Var)
        {
            return None;
        }
        // Scan the current line for an assignment `=` before any structural
        // token. This distinguishes `x = 1` from an expression like `x == 1`.
        let mut index = self.pos;
        let mut found_eq = false;
        let mut depth_counter = 0i32;
        while index < self.tokens.len() {
            let candidate = self.tokens[index];
            match candidate.kind {
                TokenKind::Newline | TokenKind::Eof if depth_counter == 0 => break,
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth_counter += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth_counter -= 1;
                    if depth_counter < 0 {
                        break;
                    }
                }
                TokenKind::Eq if depth_counter == 0 => {
                    found_eq = true;
                    break;
                }
                TokenKind::Comma | TokenKind::ColonColon | TokenKind::ColonEquals
                    if depth_counter == 0 =>
                {
                    break;
                }
                _ => {}
            }
            index += 1;
        }
        if !found_eq {
            self.pos = start;
            return None;
        }
        let def_start = self.peek().range.start;
        let is_var = self.eat_keyword(Keyword::Var).is_some();
        let pattern = self.parse_pattern_stop(&[TokenKind::Eq], &[]);
        let equals = self.expect(TokenKind::Eq, "expected-equals", "expected `=`");
        let value = self.parse_expr_body(base_indent);
        let range = TextRange::new(def_start, value.range.end);
        Some(Definition {
            pattern,
            annotation: None,
            value,
            is_var,
            doc: None,
            range,
            equals_range: equals.map(|token| token.range).unwrap_or(range),
        })
    }

    fn parse_if(&mut self, base_indent: usize, depth: usize) -> Expr {
        let keyword = self.bump();
        let condition = self.parse_expr(
            base_indent,
            depth + 1,
            ExprStop {
                kinds: &[TokenKind::LBrace],
                keywords: &[],
                at_newline: true,
            },
        );
        let then_branch = self.parse_branch_expr(base_indent, depth, Keyword::Else);
        let before_else = self.pos;
        self.skip_newlines();
        if self.eat_keyword(Keyword::Else).is_none() {
            self.pos = before_else;
        }
        if self.previous_keyword() == Some(Keyword::Else) {
            let else_branch = if self.peek().keyword == Some(Keyword::If) {
                self.parse_if(base_indent, depth)
            } else {
                self.parse_branch_expr(base_indent, depth, Keyword::Else)
            };
            Expr::new(
                ExprKind::If {
                    condition: Box::new(condition),
                    then_branch: Box::new(then_branch),
                    else_branch: Box::new(else_branch),
                },
                TextRange::new(keyword.range.start, self.previous_end()),
            )
        } else {
            Expr::new(
                ExprKind::If {
                    condition: Box::new(condition),
                    then_branch: Box::new(then_branch),
                    else_branch: Box::new(self.error_expr()),
                },
                TextRange::new(keyword.range.start, self.previous_end()),
            )
        }
    }

    fn parse_branch_expr(
        &mut self,
        base_indent: usize,
        depth: usize,
        stop_keyword: Keyword,
    ) -> Expr {
        self.skip_inline_trivia();
        if self.peek().kind == TokenKind::LBrace {
            self.parse_brace_expr(base_indent, depth)
        } else {
            self.parse_expr_body_with_stop(base_indent, depth, ExprStop::keyword(stop_keyword))
        }
    }

    fn parse_expr_body_with_stop(
        &mut self,
        base_indent: usize,
        depth: usize,
        stop: ExprStop,
    ) -> Expr {
        self.skip_inline_trivia();
        if self.raw().kind == TokenKind::Newline {
            self.skip_newlines();
            if self.eof() {
                return self.missing_expr();
            }
            let indent = self.current_indent();
            if indent <= base_indent {
                return self.missing_expr();
            }
            return self.parse_expr(indent, depth, stop);
        }
        self.parse_expr(base_indent, depth, stop)
    }

    fn parse_match(&mut self, base_indent: usize, depth: usize) -> Expr {
        let keyword = self.bump();
        let subject = self.parse_expr(
            base_indent,
            depth + 1,
            ExprStop {
                kinds: &[TokenKind::LBrace],
                keywords: &[],
                at_newline: true,
            },
        );
        if self.eat(TokenKind::LBrace).is_none() {
            return Expr::new(
                ExprKind::Match {
                    subject: Box::new(subject),
                    branches: Vec::new(),
                },
                keyword.range,
            );
        }
        let mut branches = Vec::new();
        loop {
            self.skip_newlines();
            if self.eof() || self.eat(TokenKind::RBrace).is_some() {
                break;
            }
            let branch_start = self.peek().range.start;
            let branch_indent = self.indent_of(branch_start);
            let mut patterns =
                vec![self
                    .parse_pattern_stop(&[TokenKind::FatArrow, TokenKind::Pipe], &[Keyword::If])];
            while self.eat(TokenKind::Pipe).is_some() {
                patterns.push(
                    self.parse_pattern_stop(
                        &[TokenKind::FatArrow, TokenKind::Pipe],
                        &[Keyword::If],
                    ),
                );
            }
            let guard = if self.eat_keyword(Keyword::If).is_some() {
                Some(self.parse_expr(
                    branch_indent,
                    depth + 1,
                    ExprStop::kinds(&[TokenKind::FatArrow]),
                ))
            } else {
                None
            };
            if self.eat(TokenKind::FatArrow).is_none() {
                self.recover_to(&[TokenKind::Newline, TokenKind::RBrace]);
                continue;
            }
            let body = self.parse_expr_body_with_stop(branch_indent, depth, ExprStop::NONE);
            branches.push(MatchBranch {
                patterns,
                guard,
                range: TextRange::new(branch_start, body.range.end),
                body,
            });
        }
        Expr::new(
            ExprKind::Match {
                subject: Box::new(subject),
                branches,
            },
            TextRange::new(keyword.range.start, self.previous_end()),
        )
    }

    fn parse_for(&mut self, base_indent: usize, depth: usize) -> Expr {
        let keyword = self.bump();
        let pattern = self.parse_pattern_stop(&[TokenKind::Keyword], &[Keyword::In]);
        self.eat_keyword(Keyword::In);
        let iterable = self.parse_expr(
            base_indent,
            depth + 1,
            ExprStop {
                kinds: &[TokenKind::LBrace],
                keywords: &[],
                at_newline: true,
            },
        );
        let body = if self.peek().kind == TokenKind::LBrace {
            self.parse_brace_expr(base_indent, depth)
        } else {
            self.error_expr()
        };
        Expr::new(
            ExprKind::For {
                pattern,
                iterable: Box::new(iterable),
                body: Box::new(body),
            },
            TextRange::new(keyword.range.start, self.previous_end()),
        )
    }

    fn parse_while(&mut self, base_indent: usize, depth: usize) -> Expr {
        let keyword = self.bump();
        let condition = self.parse_expr(
            base_indent,
            depth + 1,
            ExprStop {
                kinds: &[TokenKind::LBrace],
                keywords: &[],
                at_newline: true,
            },
        );
        let body = if self.peek().kind == TokenKind::LBrace {
            self.parse_brace_expr(base_indent, depth)
        } else {
            self.error_expr()
        };
        Expr::new(
            ExprKind::While {
                condition: Box::new(condition),
                body: Box::new(body),
            },
            TextRange::new(keyword.range.start, self.previous_end()),
        )
    }

    fn at_expr_boundary(&mut self, base_indent: usize, depth: usize, stop: ExprStop) -> bool {
        let token = self.peek();
        if token.kind == TokenKind::Eof {
            return true;
        }
        if stop.contains(&token) {
            return true;
        }
        if matches!(
            token.kind,
            TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
                | TokenKind::Comma
                | TokenKind::FatArrow
        ) {
            return true;
        }
        if token.kind == TokenKind::Newline {
            if depth > 0 && !stop.at_newline {
                return false;
            }
            self.skip_newlines();
            let next = self.peek();
            let boundary = next.kind == TokenKind::Eof
                || stop.contains(&next)
                || matches!(
                    next.kind,
                    TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace | TokenKind::Comma
                )
                || {
                    let indent = self.indent_of(next.range.start);
                    let continuation = matches!(
                        next.kind,
                        TokenKind::Dot
                            | TokenKind::OptionalQuestion
                            | TokenKind::Operator
                            | TokenKind::Question
                            | TokenKind::Arrow
                    );
                    indent <= base_indent && !continuation
                };
            return boundary;
        }
        false
    }

    fn string_parts(&self, token: Token) -> Vec<StrPart> {
        let mut parts = Vec::new();
        let (inner_start, inner_end) = if token.kind == TokenKind::MultilineString {
            (token.range.start, token.range.end)
        } else {
            (token.range.start + 1, token.range.end.saturating_sub(1))
        };
        let inner = &self.source[(inner_start as usize).min(self.source.len())
            ..(inner_end as usize).min(self.source.len())];
        let mut offset = 0usize;
        while let Some(found) = inner[offset..].find("${") {
            let interpolation_start = offset + found;
            if interpolation_start > offset {
                parts.push(StrPart {
                    kind: StrPartKind::Text,
                    range: TextRange::new(
                        inner_start + offset as u32,
                        inner_start + interpolation_start as u32,
                    ),
                });
            }
            let mut depth = 0i32;
            let mut index = interpolation_start + 2;
            let mut end = None;
            while index < inner.len() {
                match inner.as_bytes()[index] {
                    b'{' => depth += 1,
                    b'}' => {
                        if depth == 0 {
                            end = Some(index);
                            break;
                        }
                        depth -= 1;
                    }
                    _ => {}
                }
                index += 1;
            }
            let Some(end) = end else {
                parts.push(StrPart {
                    kind: StrPartKind::Text,
                    range: TextRange::new(
                        inner_start + interpolation_start as u32,
                        inner_start + inner.len() as u32,
                    ),
                });
                break;
            };
            let expr_start = inner_start + (interpolation_start + 2) as u32;
            let expr_end = inner_start + end as u32;
            parts.push(StrPart {
                kind: StrPartKind::Interpolation(self.parse_interpolation(expr_start, expr_end)),
                range: TextRange::new(
                    inner_start + interpolation_start as u32,
                    inner_start + (end + 1) as u32,
                ),
            });
            offset = end + 1;
        }
        if offset < inner.len() {
            parts.push(StrPart {
                kind: StrPartKind::Text,
                range: TextRange::new(
                    inner_start + offset as u32,
                    inner_start + inner.len() as u32,
                ),
            });
        }
        parts
    }

    /// Parse the body of a `${...}` interpolation with absolute ranges.
    fn parse_interpolation(&self, start: u32, end: u32) -> Expr {
        let start_usize = (start as usize).min(self.source.len());
        let end_usize = (end as usize).min(self.source.len());
        if start_usize >= end_usize {
            return Expr::new(ExprKind::Error, TextRange::new(start, end));
        }
        let slice = &self.source[start_usize..end_usize];
        let mut tokens = lexer::lex(slice);
        for token in &mut tokens {
            token.range = TextRange::new(token.range.start + start, token.range.end + start);
        }
        let mut parser = Parser::new(self.source, &tokens);
        let expr = parser.parse_expr(0, 0, ExprStop::NONE);
        if matches!(expr.kind, ExprKind::Error) {
            Expr::new(ExprKind::Error, TextRange::new(start, end))
        } else {
            expr
        }
    }

    fn error_expr(&mut self) -> Expr {
        Expr::new(ExprKind::Error, TextRange::empty(self.peek().range.start))
    }

    fn missing_expr(&mut self) -> Expr {
        let range = TextRange::empty(self.peek().range.start);
        self.diagnostics.push(SyntaxDiagnostic {
            range,
            message: "expected an expression".to_string(),
            code: "expected-expression",
        });
        Expr::new(ExprKind::Error, range)
    }

    fn previous_keyword(&self) -> Option<Keyword> {
        let mut index = self.pos;
        while index > 0 {
            index -= 1;
            let token = self.tokens[index];
            if !token.kind.is_trivia() {
                return token.keyword;
            }
        }
        None
    }

    fn previous_end(&self) -> u32 {
        self.tokens
            .iter()
            .take(self.pos)
            .rev()
            .find(|token| token.kind != TokenKind::Eof)
            .map(|token| token.range.end)
            .unwrap_or(0)
    }

    // ------------------------------------------------------------------
    // Patterns
    // ------------------------------------------------------------------

    fn parse_pattern_stop(
        &mut self,
        stop_kinds: &[TokenKind],
        stop_keywords: &[Keyword],
    ) -> Pattern {
        let pattern = self.parse_pattern_primary(stop_kinds, stop_keywords);
        if self.eat_keyword(Keyword::As).is_some()
            && matches!(self.peek().kind, TokenKind::LowerIdent)
        {
            let name_token = self.bump();
            let range = pattern.range.union(name_token.range);
            return Pattern::new(
                PatternKind::As {
                    pattern: Box::new(pattern),
                    name: self.name(name_token),
                },
                range,
            );
        }
        pattern
    }

    fn parse_pattern_primary(
        &mut self,
        stop_kinds: &[TokenKind],
        stop_keywords: &[Keyword],
    ) -> Pattern {
        self.skip_inline_trivia();
        let token = self.peek();
        let start = token.range.start;
        match token.kind {
            TokenKind::Underscore => {
                let token = self.bump();
                Pattern::new(PatternKind::Wildcard, token.range)
            }
            TokenKind::LowerIdent => {
                let token = self.bump();
                let is_var = self.text(token).starts_with('$');
                Pattern::new(
                    PatternKind::Name {
                        name: self.name(token),
                        is_var,
                    },
                    token.range,
                )
            }
            TokenKind::UpperIdent => {
                let token = self.bump();
                let mut parts = vec![self.name(token)];
                while self.peek().kind == TokenKind::Dot
                    && self.peek_at(1).kind == TokenKind::UpperIdent
                {
                    self.bump();
                    let part = self.bump();
                    parts.push(self.name(part));
                }
                if self.peek().kind == TokenKind::Dot
                    && matches!(self.peek_at(1).kind, TokenKind::LBrace | TokenKind::LParen)
                {
                    self.bump();
                    let inner = self.parse_pattern_primary(stop_kinds, stop_keywords);
                    let range = TextRange::new(start, inner.range.end);
                    return Pattern::new(
                        PatternKind::Nominal {
                            type_name: qual_name_from_parts(&parts),
                            pattern: Box::new(inner),
                        },
                        range,
                    );
                }
                let name = parts.pop().expect("pattern starts with a name");
                let qualifier = if parts.is_empty() {
                    None
                } else {
                    Some(qual_name_from_parts(&parts))
                };
                let mut args = Vec::new();
                if self.peek().kind == TokenKind::LParen {
                    self.bump();
                    loop {
                        self.skip_separators(1);
                        if self.eof() || self.eat(TokenKind::RParen).is_some() {
                            break;
                        }
                        if self.eat(TokenKind::Comma).is_some() {
                            continue;
                        }
                        args.push(
                            self.parse_pattern_stop(&[TokenKind::Comma, TokenKind::RParen], &[]),
                        );
                        self.skip_separators(1);
                        self.eat(TokenKind::Comma);
                    }
                }
                Pattern::new(
                    PatternKind::Tag {
                        qualifier,
                        name,
                        args,
                    },
                    TextRange::new(start, self.previous_end()),
                )
            }
            TokenKind::Number => {
                let token = self.bump();
                Pattern::new(
                    PatternKind::Number(self.text(token).to_string()),
                    token.range,
                )
            }
            TokenKind::String => {
                let token = self.bump();
                Pattern::new(PatternKind::String(self.string_parts(token)), token.range)
            }
            TokenKind::Char => {
                let token = self.bump();
                Pattern::new(PatternKind::Char(self.text(token).to_string()), token.range)
            }
            TokenKind::LBrace => self.parse_record_pattern(),
            TokenKind::LBracket => self.parse_list_pattern(),
            TokenKind::LParen => {
                let open = self.bump();
                let mut elements = Vec::new();
                loop {
                    self.skip_separators(1);
                    if self.eof() || self.eat(TokenKind::RParen).is_some() {
                        break;
                    }
                    if self.eat(TokenKind::Comma).is_some() {
                        continue;
                    }
                    elements
                        .push(self.parse_pattern_stop(&[TokenKind::Comma, TokenKind::RParen], &[]));
                    self.skip_separators(1);
                    self.eat(TokenKind::Comma);
                }
                let kind = if elements.len() == 1 {
                    elements.remove(0).kind
                } else {
                    PatternKind::Tuple(elements)
                };
                Pattern::new(kind, TextRange::new(open.range.start, self.previous_end()))
            }
            _ => {
                let token = self.bump();
                if stop_kinds.contains(&token.kind)
                    || stop_keywords.contains(&token.keyword.unwrap_or(Keyword::And))
                {
                    Pattern::new(PatternKind::Error, TextRange::empty(start))
                } else {
                    Pattern::new(PatternKind::Error, token.range)
                }
            }
        }
    }

    fn parse_record_pattern(&mut self) -> Pattern {
        let open = self.bump();
        let mut fields = Vec::new();
        let mut rest = None;
        loop {
            self.skip_separators(1);
            if self.eof() {
                break;
            }
            if self.eat(TokenKind::RBrace).is_some() {
                break;
            }
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            if self.eat(TokenKind::DotDot).is_some() {
                if matches!(self.peek().kind, TokenKind::LowerIdent) {
                    let name = self.bump();
                    rest = Some(Some(self.name(name)));
                } else {
                    rest = Some(None);
                }
                self.eat(TokenKind::Comma);
                continue;
            }
            if matches!(
                self.peek().kind,
                TokenKind::LowerIdent | TokenKind::UpperIdent
            ) {
                let name_token = self.bump();
                let name = self.name(name_token);
                let pattern = if self.eat(TokenKind::Colon).is_some() {
                    Some(self.parse_pattern_stop(&[TokenKind::Comma, TokenKind::RBrace], &[]))
                } else {
                    None
                };
                let range = TextRange::new(name_token.range.start, self.previous_end());
                fields.push(PatternField {
                    name,
                    pattern,
                    range,
                });
                self.skip_separators(1);
                self.eat(TokenKind::Comma);
            } else {
                self.bump();
            }
        }
        Pattern::new(
            PatternKind::Record { fields, rest },
            TextRange::new(open.range.start, self.previous_end()),
        )
    }

    fn parse_list_pattern(&mut self) -> Pattern {
        let open = self.bump();
        let mut elements = Vec::new();
        let mut rest = None;
        loop {
            self.skip_separators(1);
            if self.eof() {
                break;
            }
            if self.eat(TokenKind::RBracket).is_some() {
                break;
            }
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            if self.eat(TokenKind::DotDot).is_some() {
                if self.eat_keyword(Keyword::As).is_some() {
                    if matches!(self.peek().kind, TokenKind::LowerIdent) {
                        let name = self.bump();
                        rest = Some(Some(self.name(name)));
                    } else {
                        rest = Some(None);
                    }
                } else if matches!(self.peek().kind, TokenKind::LowerIdent) {
                    let name = self.bump();
                    rest = Some(Some(self.name(name)));
                } else {
                    rest = Some(None);
                }
                continue;
            }
            elements.push(self.parse_pattern_stop(&[TokenKind::Comma, TokenKind::RBracket], &[]));
            self.skip_separators(1);
            self.eat(TokenKind::Comma);
        }
        Pattern::new(
            PatternKind::List { elements, rest },
            TextRange::new(open.range.start, self.previous_end()),
        )
    }

    // ------------------------------------------------------------------
    // Types
    // ------------------------------------------------------------------

    fn parse_type(
        &mut self,
        base_indent: usize,
        depth: usize,
        stop_kinds: &[TokenKind],
        stop_keywords: &[Keyword],
    ) -> TypeExpr {
        self.skip_separators(depth);
        let start = self.peek().range.start;
        let mut first = self.parse_type_primary(base_indent, depth, stop_kinds, stop_keywords);
        if self
            .peek()
            .keyword
            .is_some_and(|keyword| stop_keywords.contains(&keyword))
        {
            return first;
        }
        if self.peek().kind == TokenKind::Comma && !stop_kinds.contains(&TokenKind::Comma) {
            let mut args = vec![first];
            loop {
                self.skip_separators(depth);
                if depth == 0
                    && self.raw().kind == TokenKind::Newline
                    && !stop_kinds.contains(&TokenKind::Newline)
                {
                    // Unparenthesized function type arguments may continue on
                    // the next line: `spec, { ... } -> Try(...)`.
                    self.skip_newlines();
                }
                if self.eof() || self.peek().kind == TokenKind::Comma {
                    if self.eat(TokenKind::Comma).is_some() {
                        continue;
                    }
                    break;
                }
                if self.peek().kind == TokenKind::Arrow || self.peek().kind == TokenKind::FatArrow {
                    break;
                }
                args.push(self.parse_type_primary(base_indent, depth, stop_kinds, stop_keywords));
                if self.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            if matches!(self.peek().kind, TokenKind::Arrow | TokenKind::FatArrow) {
                let arrow = self.bump();
                let ret = self.parse_type(base_indent, depth, stop_kinds, stop_keywords);
                let end = ret.range.end;
                let function = TypeExpr::new(
                    TypeKind::Function {
                        args,
                        ret: Box::new(ret),
                        effectful: arrow.kind == TokenKind::FatArrow,
                    },
                    TextRange::new(start, end),
                );
                return self.finish_type(function, base_indent, depth);
            }
            first = args.remove(0);
        }
        if matches!(self.peek().kind, TokenKind::Arrow | TokenKind::FatArrow) {
            let arrow = self.bump();
            let ret = self.parse_type(base_indent, depth, stop_kinds, stop_keywords);
            let function = TypeExpr::new(
                TypeKind::Function {
                    args: vec![first],
                    ret: Box::new(ret),
                    effectful: arrow.kind == TokenKind::FatArrow,
                },
                TextRange::new(start, self.previous_end()),
            );
            return self.finish_type(function, base_indent, depth);
        }
        self.finish_type(first, base_indent, depth)
    }

    fn finish_type(&mut self, mut base: TypeExpr, base_indent: usize, depth: usize) -> TypeExpr {
        if self.peek().kind == TokenKind::Newline {
            let save = self.pos;
            self.skip_newlines();
            if self.peek().keyword != Some(Keyword::Where) {
                self.pos = save;
            }
        }
        if self.eat_keyword(Keyword::Where).is_some() {
            let start = base.range.start;
            let requirements = self.parse_where_requirements(base_indent, depth);
            base = TypeExpr::new(
                TypeKind::Where {
                    base: Box::new(base),
                    requirements,
                },
                TextRange::new(start, self.previous_end()),
            );
        }
        base
    }

    fn parse_where_requirements(
        &mut self,
        base_indent: usize,
        depth: usize,
    ) -> Vec<WhereRequirement> {
        let mut requirements = Vec::new();
        if self.eat(TokenKind::LBracket).is_some() {
            loop {
                self.skip_newlines();
                if self.eof() || self.eat(TokenKind::RBracket).is_some() {
                    break;
                }
                if self.eat(TokenKind::Comma).is_some() {
                    continue;
                }
                let ty_var = if matches!(self.peek().kind, TokenKind::LowerIdent) {
                    let token = self.bump();
                    self.name(token)
                } else {
                    Name::new("_".to_string(), self.peek().range)
                };
                self.eat(TokenKind::Dot);
                let method = if matches!(
                    self.peek().kind,
                    TokenKind::LowerIdent | TokenKind::UpperIdent
                ) {
                    let token = self.bump();
                    self.name(token)
                } else {
                    Name::new("_".to_string(), self.peek().range)
                };
                self.eat(TokenKind::Colon);
                let ty = self.parse_type(
                    base_indent,
                    depth + 1,
                    &[TokenKind::Comma, TokenKind::RBracket],
                    &[],
                );
                let range = ty_var.range.union(ty.range);
                requirements.push(WhereRequirement {
                    ty_var,
                    method,
                    ty,
                    range,
                });
                self.eat(TokenKind::Comma);
            }
        }
        requirements
    }

    fn parse_type_primary(
        &mut self,
        base_indent: usize,
        depth: usize,
        stop_kinds: &[TokenKind],
        _stop_keywords: &[Keyword],
    ) -> TypeExpr {
        self.skip_separators(depth);
        let token = self.peek();
        let start = token.range.start;
        if stop_kinds.contains(&token.kind)
            || token.kind == TokenKind::Eof
            || token
                .keyword
                .is_some_and(|keyword| _stop_keywords.contains(&keyword))
        {
            return TypeExpr::new(TypeKind::Error, TextRange::empty(start));
        }
        match token.kind {
            TokenKind::UpperIdent | TokenKind::LowerIdent => {
                let qual = self.parse_qual_name().unwrap_or(QualName {
                    parts: vec![self.name(token)],
                    range: token.range,
                });
                let mut end = qual.range.end;
                if self.peek().kind == TokenKind::LParen {
                    self.bump();
                    let mut args = Vec::new();
                    loop {
                        self.skip_separators(depth + 1);
                        if self.eof() || self.eat(TokenKind::RParen).is_some() {
                            break;
                        }
                        if self.eat(TokenKind::Comma).is_some() {
                            continue;
                        }
                        args.push(self.parse_type(
                            base_indent,
                            depth + 1,
                            &[TokenKind::Comma, TokenKind::RParen],
                            &[],
                        ));
                        self.skip_separators(depth + 1);
                        self.eat(TokenKind::Comma);
                    }
                    end = self.previous_end();
                    TypeExpr::new(
                        TypeKind::Apply { name: qual, args },
                        TextRange::new(start, end),
                    )
                } else if qual.parts.len() == 1 && token.kind == TokenKind::LowerIdent {
                    TypeExpr::new(TypeKind::Var(qual.parts[0].clone()), qual.range)
                } else {
                    TypeExpr::new(TypeKind::Named(qual), TextRange::new(start, end))
                }
            }
            TokenKind::Underscore => {
                let token = self.bump();
                TypeExpr::new(TypeKind::Wildcard, token.range)
            }
            TokenKind::LBrace => self.parse_record_type(base_indent, depth),
            TokenKind::LBracket => self.parse_tag_union_type(base_indent, depth),
            TokenKind::LParen => {
                let open = self.bump();
                let mut elements = Vec::new();
                loop {
                    self.skip_separators(depth + 1);
                    if self.eof() || self.eat(TokenKind::RParen).is_some() {
                        break;
                    }
                    if self.eat(TokenKind::Comma).is_some() {
                        continue;
                    }
                    elements.push(self.parse_type(
                        base_indent,
                        depth + 1,
                        &[TokenKind::Comma, TokenKind::RParen],
                        &[],
                    ));
                    self.skip_separators(depth + 1);
                    self.eat(TokenKind::Comma);
                }
                if elements.len() == 1 {
                    let mut only = elements.remove(0);
                    only.range = TextRange::new(open.range.start, self.previous_end());
                    only
                } else {
                    TypeExpr::new(
                        TypeKind::Tuple(elements),
                        TextRange::new(open.range.start, self.previous_end()),
                    )
                }
            }
            _ => {
                let token = self.bump();
                TypeExpr::new(TypeKind::Error, token.range)
            }
        }
    }

    fn parse_record_type(&mut self, base_indent: usize, depth: usize) -> TypeExpr {
        let open = self.bump();
        let mut fields = Vec::new();
        let mut open_row = None;
        loop {
            self.skip_separators(depth + 1);
            if self.eof() {
                break;
            }
            if self.eat(TokenKind::RBrace).is_some() {
                break;
            }
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            if self.eat(TokenKind::DotDot).is_some() {
                if matches!(self.peek().kind, TokenKind::LowerIdent) {
                    let name = self.bump();
                    open_row = Some(Some(self.name(name)));
                } else {
                    open_row = Some(None);
                }
                self.eat(TokenKind::Comma);
                continue;
            }
            if self.peek().kind == TokenKind::Underscore {
                let name_token = self.bump();
                let name = self.name(name_token);
                self.eat(TokenKind::Colon);
                let ty = self.parse_type(
                    base_indent,
                    depth + 1,
                    &[TokenKind::Comma, TokenKind::RBrace],
                    &[],
                );
                let range = TextRange::new(name_token.range.start, self.previous_end());
                fields.push(TypeField {
                    name,
                    ty,
                    optional: false,
                    default: None,
                    range,
                });
                self.skip_separators(depth + 1);
                self.eat(TokenKind::Comma);
            } else if matches!(
                self.peek().kind,
                TokenKind::LowerIdent | TokenKind::UpperIdent
            ) {
                let name_token = self.bump();
                let name = self.name(name_token);
                let optional = if self.peek().kind == TokenKind::Question {
                    self.bump();
                    self.eat(TokenKind::Colon);
                    true
                } else {
                    self.eat(TokenKind::Colon);
                    false
                };
                let ty = self.parse_type(
                    base_indent,
                    depth + 1,
                    &[TokenKind::Comma, TokenKind::RBrace],
                    &[],
                );
                let token = self.peek();
                let default = if token.kind == TokenKind::Operator && self.text(token) == "??" {
                    self.bump();
                    Some(self.parse_expr(base_indent, depth + 1, ExprStop::kinds(EXPR_STOP_RECORD)))
                } else {
                    None
                };
                let range = TextRange::new(name_token.range.start, self.previous_end());
                fields.push(TypeField {
                    name,
                    ty,
                    optional,
                    default,
                    range,
                });
                self.skip_separators(depth + 1);
                self.eat(TokenKind::Comma);
            } else {
                self.bump();
            }
        }
        TypeExpr::new(
            TypeKind::Record {
                fields,
                open: open_row,
            },
            TextRange::new(open.range.start, self.previous_end()),
        )
    }

    fn parse_tag_union_type(&mut self, base_indent: usize, depth: usize) -> TypeExpr {
        let open = self.bump();
        let mut tags = Vec::new();
        let mut open_row = None;
        loop {
            self.skip_separators(depth + 1);
            if self.eof() {
                break;
            }
            if self.eat(TokenKind::RBracket).is_some() {
                break;
            }
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            if self.eat(TokenKind::DotDot).is_some() {
                if matches!(self.peek().kind, TokenKind::LowerIdent) {
                    let name = self.bump();
                    open_row = Some(Some(self.name(name)));
                } else {
                    open_row = Some(None);
                }
                self.eat(TokenKind::Comma);
                continue;
            }
            if self.peek().kind == TokenKind::UpperIdent {
                let name_token = self.bump();
                let mut payloads = Vec::new();
                if self.eat(TokenKind::LParen).is_some() {
                    loop {
                        self.skip_separators(depth + 1);
                        if self.eof() || self.eat(TokenKind::RParen).is_some() {
                            break;
                        }
                        if self.eat(TokenKind::Comma).is_some() {
                            continue;
                        }
                        payloads.push(self.parse_type(
                            base_indent,
                            depth + 1,
                            &[TokenKind::Comma, TokenKind::RParen],
                            &[],
                        ));
                        self.skip_separators(depth + 1);
                        self.eat(TokenKind::Comma);
                    }
                }
                let range = TextRange::new(name_token.range.start, self.previous_end());
                tags.push(TypeTag {
                    name: self.name(name_token),
                    payloads,
                    range,
                });
                self.eat(TokenKind::Comma);
            } else {
                self.bump();
            }
        }
        TypeExpr::new(
            TypeKind::TagUnion {
                tags,
                open: open_row,
            },
            TextRange::new(open.range.start, self.previous_end()),
        )
    }

    fn parse_qual_name(&mut self) -> Option<QualName> {
        self.skip_inline_trivia();
        let first = self.peek();
        if !matches!(first.kind, TokenKind::LowerIdent | TokenKind::UpperIdent) {
            return None;
        }
        let first = self.bump();
        let mut parts = vec![self.name(first)];
        let mut end = first.range.end;
        loop {
            if self.peek().kind != TokenKind::Dot {
                break;
            }
            let after = self.peek_at(1);
            if !matches!(after.kind, TokenKind::LowerIdent | TokenKind::UpperIdent) {
                break;
            }
            self.bump();
            let part = self.bump();
            end = part.range.end;
            parts.push(self.name(part));
        }
        Some(QualName {
            range: TextRange::new(first.range.start, end),
            parts,
        })
    }
}

fn line_of(line_starts: &[u32], offset: u32) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(line) => line,
        Err(next) => next.saturating_sub(1),
    }
}

fn start_of(token: &Token, _open: Token) -> u32 {
    token.range.start
}

fn unquote(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .unwrap_or(text)
}

fn token_name(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::LParen => "(",
        TokenKind::LBrace => "{",
        TokenKind::LBracket => "[",
        _ => "delimiter",
    }
}

fn binary_operator(token: Token, text: &str) -> Option<u8> {
    if let Some(keyword) = token.keyword {
        return match keyword {
            Keyword::And => Some(40),
            Keyword::Or => Some(30),
            _ => None,
        };
    }
    match token.kind {
        TokenKind::DotDot | TokenKind::DotDotLess | TokenKind::DotDotEq => Some(20),
        TokenKind::Question => Some(60),
        TokenKind::Operator => Some(match text {
            "|>" => 100,
            "*" | "/" | "%" | "//" => 90,
            "+" | "-" => 80,
            "??" => 70,
            "==" | "!=" | "<" | ">" | "<=" | ">=" => 50,
            ".." | "..<" | "..=" => 20,
            "^" => 85,
            _ => 10,
        }),
        _ => None,
    }
}

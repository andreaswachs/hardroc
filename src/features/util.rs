//! Shared helpers for feature implementations.

use lsp_types::{
    Location, MarkupContent, MarkupKind, Position, Range, SymbolKind as LspSymbolKind, Url,
};

use crate::analysis::{AnalyzedFile, SymbolKind};
use crate::text::{SourceFile, TextRange};

pub fn range(source: &SourceFile, range: TextRange) -> Range {
    source.range(range)
}

pub fn location(uri: &Url, source: &SourceFile, range: TextRange) -> Location {
    Location {
        uri: uri.clone(),
        range: source.range(range),
    }
}

pub fn offset_at(analysis: &AnalyzedFile, position: Position) -> u32 {
    analysis.source.offset_at(position)
}

pub fn symbol_kind(kind: SymbolKind) -> LspSymbolKind {
    match kind {
        SymbolKind::Module => LspSymbolKind::MODULE,
        SymbolKind::Package => LspSymbolKind::PACKAGE,
        SymbolKind::Type => LspSymbolKind::STRUCT,
        SymbolKind::Function => LspSymbolKind::FUNCTION,
        SymbolKind::Method => LspSymbolKind::METHOD,
        SymbolKind::Constant => LspSymbolKind::CONSTANT,
        SymbolKind::Variable => LspSymbolKind::VARIABLE,
        SymbolKind::Field => LspSymbolKind::FIELD,
        SymbolKind::Tag => LspSymbolKind::ENUM_MEMBER,
        SymbolKind::TypeParameter => LspSymbolKind::TYPE_PARAMETER,
        SymbolKind::Parameter => LspSymbolKind::VARIABLE,
        SymbolKind::Local => LspSymbolKind::VARIABLE,
        SymbolKind::Import => LspSymbolKind::NAMESPACE,
    }
}

pub fn markdown(value: impl Into<String>) -> MarkupContent {
    MarkupContent {
        kind: MarkupKind::Markdown,
        value: value.into(),
    }
}

pub fn markdown_doc(doc: &str, detail: Option<&str>) -> String {
    let mut output = String::new();
    if let Some(detail) = detail {
        output.push_str("```roc\n");
        output.push_str(detail.trim());
        output.push_str("\n```");
    }
    if !doc.trim().is_empty() {
        if !output.is_empty() {
            output.push_str("\n\n---\n\n");
        }
        output.push_str(doc.trim());
    }
    output
}

pub fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_' || ch == '$')
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '!' || ch == '$')
}

pub fn line_range(source: &SourceFile, offset: u32) -> TextRange {
    let line = source.line_of_offset(offset);
    TextRange::new(source.line_start(line), source.line_end(line))
}

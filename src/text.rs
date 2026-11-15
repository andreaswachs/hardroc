//! Shared text and position utilities.
//!
//! Roc-LS stores byte offsets internally.  LSP positions are UTF-16 based, so
//! every conversion goes through [`SourceFile`], which owns an immutable copy
//! of the document text and a precomputed line index.  Getting this wrong is
//! the classic source of corrupted edits in language servers, so conversions
//! clamp defensively and treat a position in the middle of a surrogate pair as
//! the end of that code point.

use std::path::{Path, PathBuf};

use lsp_types::{Position, Range, Url};

/// A half-open byte range in a UTF-8 source file.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

impl TextRange {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    pub const fn empty(at: u32) -> Self {
        Self { start: at, end: at }
    }

    pub fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }

    pub fn contains(self, offset: u32) -> bool {
        self.start <= offset && offset < self.end
    }

    pub fn contains_inclusive(self, offset: u32) -> bool {
        self.start <= offset && offset <= self.end
    }

    pub fn intersects(self, other: TextRange) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn union(self, other: TextRange) -> TextRange {
        TextRange {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    pub fn clamp_to(self, len: u32) -> TextRange {
        let start = self.start.min(len);
        TextRange {
            start,
            end: self.end.max(start).min(len),
        }
    }
}

impl From<TextRange> for std::ops::Range<usize> {
    fn from(value: TextRange) -> Self {
        value.start as usize..value.end as usize
    }
}

/// Immutable source text with fast byte-offset <-> LSP-position conversion.
#[derive(Debug, Clone)]
pub struct SourceFile {
    uri: Url,
    text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(uri: Url, text: String) -> Self {
        let line_starts = compute_line_starts(&text);
        Self {
            uri,
            text,
            line_starts,
        }
    }

    pub fn uri(&self) -> &Url {
        &self.uri
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> u32 {
        self.text.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn line_start(&self, line: usize) -> u32 {
        self.line_starts.get(line).copied().unwrap_or(self.len())
    }

    pub fn line_end(&self, line: usize) -> u32 {
        let start = self.line_start(line) as usize;
        let end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.len()) as usize;
        let mut end = end.max(start);
        while end > start && self.text.as_bytes().get(end - 1) == Some(&b'\n') {
            end -= 1;
            if end > start && self.text.as_bytes().get(end - 1) == Some(&b'\r') {
                end -= 1;
            }
        }
        end as u32
    }

    pub fn line_of_offset(&self, offset: u32) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next.saturating_sub(1),
        }
    }

    pub fn position_at(&self, offset: u32) -> Position {
        let offset = offset.min(self.len());
        let line = self.line_of_offset(offset);
        let line_start = self.line_start(line);
        let mut utf16 = 0u32;
        for ch in self.text[line_start as usize..offset as usize].chars() {
            utf16 += ch.len_utf16() as u32;
        }
        Position {
            line: line as u32,
            character: utf16,
        }
    }

    pub fn offset_at(&self, position: Position) -> u32 {
        let line = position.line as usize;
        if line >= self.line_starts.len() {
            return self.len();
        }
        let start = self.line_start(line);
        let end = self.line_end(line);
        let mut utf16 = 0u32;
        for (idx, ch) in self.text[start as usize..end as usize].char_indices() {
            if utf16 >= position.character {
                return start + idx as u32;
            }
            utf16 += ch.len_utf16() as u32;
            if utf16 > position.character {
                // Position points inside a surrogate pair. Treat it as the end
                // of the code point so edits never split UTF-8.
                return start + (idx + ch.len_utf8()) as u32;
            }
        }
        end
    }

    pub fn range(&self, range: TextRange) -> Range {
        Range {
            start: self.position_at(range.start),
            end: self.position_at(range.end),
        }
    }

    pub fn text_range(&self, range: Range) -> TextRange {
        let start = self.offset_at(range.start);
        let end = self.offset_at(range.end);
        TextRange {
            start,
            end: end.max(start),
        }
    }

    pub fn slice(&self, range: TextRange) -> &str {
        let range = range.clamp_to(self.len());
        let start = floor_char_boundary(&self.text, range.start as usize);
        let end = floor_char_boundary(&self.text, range.end as usize);
        &self.text[start..end]
    }

    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_start(line) as usize;
        let end = self.line_end(line) as usize;
        &self.text[start..end]
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.uri.to_file_path().ok()
    }
}

fn compute_line_starts(text: &str) -> Vec<u32> {
    let mut starts = Vec::with_capacity(text.len() / 32 + 1);
    starts.push(0);
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push((idx + 1) as u32);
        }
    }
    starts
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Apply a single LSP content change to a string, using the given source file
/// for position conversion.
pub fn apply_change(text: &mut String, source: &SourceFile, range: Range, new_text: &str) {
    let edit = source.text_range(range);
    let start = floor_char_boundary(text, edit.start as usize);
    let end = floor_char_boundary(text, edit.end as usize).max(start);
    text.replace_range(start..end, new_text);
}

/// Build a `file://` URI from a path, preserving percent encoding correctness.
pub fn path_to_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(text: &str) -> SourceFile {
        SourceFile::new(Url::parse("file:///test.roc").unwrap(), text.to_string())
    }

    #[test]
    fn round_trips_ascii_positions() {
        let src = source("abc\ndef\n");
        for offset in 0..=src.len() {
            let pos = src.position_at(offset);
            let back = src.offset_at(pos);
            assert_eq!(back, offset, "offset {offset} -> {pos:?} -> {back}");
        }
    }

    #[test]
    fn handles_utf16_positions() {
        let src = source("a😀b\nx");
        assert_eq!(src.position_at(1), Position::new(0, 1));
        assert_eq!(src.position_at(5), Position::new(0, 3));
        assert_eq!(src.offset_at(Position::new(0, 3)), 5);
        assert_eq!(src.offset_at(Position::new(0, 2)), 5, "inside surrogate");
        assert_eq!(src.offset_at(Position::new(1, 1)), 8);
    }

    #[test]
    fn line_end_excludes_newline() {
        let src = source("abc\r\ndef");
        assert_eq!(src.line_end(0), 3);
        assert_eq!(src.line_start(1), 5);
        assert_eq!(src.line_text(0), "abc");
    }
}

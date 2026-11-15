//! Diagnostic helpers shared by analysis passes.

use crate::text::TextRange;

use super::{AnalysisDiagnostic, DiagnosticFix, SuggestedEdit};

pub fn with_fix(
    mut diagnostic: AnalysisDiagnostic,
    title: impl Into<String>,
    edits: Vec<SuggestedEdit>,
) -> AnalysisDiagnostic {
    diagnostic.fixes.push(DiagnosticFix {
        title: title.into(),
        edits,
    });
    diagnostic
}

pub fn span_line(range: TextRange) -> TextRange {
    range
}

pub fn unknown_name(range: TextRange, name: &str) -> AnalysisDiagnostic {
    AnalysisDiagnostic::error(
        range,
        "unknown-name",
        format!("`{name}` is not defined in this scope"),
    )
}

pub fn unknown_module(range: TextRange, name: &str) -> AnalysisDiagnostic {
    AnalysisDiagnostic::error(
        range,
        "unknown-module",
        format!("module `{name}` could not be found"),
    )
}
